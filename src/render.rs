//! Post-render item spacing: blank lines between generated items.
//!
//! prettyplease emits no blank line between items, so generated output
//! reads as a wall — a struct's closing brace with the next type's doc
//! comment on the very next line. [`space_rendered`] separates items
//! with a single blank line, at the top level and inside every inline
//! `pub mod { ... }` body at any depth, keeping runs of one-line
//! declarations tight: consecutive `use` items (the import preamble,
//! `pub use` re-exports) and consecutive body-less `pub mod x;`
//! declarations (the root `mod.rs` shape) stay contiguous blocks.
//! Everything else — types, impls, inline-body modules, macro
//! definitions and invocations — gets separated.
//!
//! Same safety posture as
//! [`polish_rendered`](crate::condense::polish_rendered): blank lines
//! are only inserted at item boundaries (span line numbers of the
//! re-parsed source, which start at an item's doc/attribute stack, so
//! docs move with their item), never inside an item, and the result
//! must re-parse to the identical token stream or the input is
//! returned unchanged.

use quote::ToTokens;
use syn::spanned::Spanned;

use crate::condense::{finish_lines, polish_rendered, tokens_equal};

/// Render one output document's body: prettyplease, then the condensed
/// style's macro polish, then item spacing. The shared final step of
/// [`render_file`](crate::render_file) and the tree writer's per-file
/// rendering ([`plan_file_tree`](crate::plan_file_tree)), so
/// single-file and folder-tree output are spaced identically.
pub(crate) fn render_body(file: &syn::File) -> String {
    space_rendered(polish_rendered(prettyplease::unparse(file)))
}

/// Insert a single blank line between adjacent items, recursively
/// through inline module bodies. Runs of `use` items and runs of
/// body-less module declarations stay tight; a run followed by any
/// other item kind gets the blank line like everything else.
pub(crate) fn space_rendered(source: String) -> String {
    let Ok(parsed) = syn::parse_file(&source) else {
        return source;
    };

    // 1-based line numbers that should have a blank line inserted
    // before them: the first line (doc/attr stack included) of every
    // item whose predecessor it must not sit flush against.
    let mut gap_lines: Vec<usize> = Vec::new();
    collect_gap_lines(&parsed.items, &mut gap_lines);
    if gap_lines.is_empty() {
        return source;
    }

    // Insert bottom-up so earlier indices stay valid.
    let mut lines: Vec<String> = source.lines().map(String::from).collect();
    gap_lines.sort_unstable();
    for line in gap_lines.into_iter().rev() {
        lines.insert(line - 1, String::new());
    }
    let spaced = finish_lines(lines, &source);

    // Fidelity gate: a whitespace-only change re-parses to the exact
    // same token stream; anything else means a span misfired (e.g. a
    // blank line landed inside a multi-line literal) — keep the input.
    match syn::parse_file(&spaced) {
        Ok(reparsed)
            if tokens_equal(parsed.to_token_stream(), reparsed.to_token_stream()) =>
        {
            spaced
        }
        _ => source,
    }
}

/// Collect the start line of every item that needs a blank line before
/// it, in `items` and recursively in inline module bodies.
fn collect_gap_lines(items: &[syn::Item], gap_lines: &mut Vec<usize>) {
    for pair in items.windows(2) {
        if !stays_tight(&pair[0], &pair[1]) {
            gap_lines.push(item_start_line(&pair[1]));
        }
    }
    for item in items {
        if let syn::Item::Mod(module) = item
            && let Some((_, children)) = &module.content
        {
            collect_gap_lines(children, gap_lines);
        }
    }
}

/// The one-line declaration runs that stay contiguous: `use` blocks
/// (including `pub use` re-exports) and `pub mod x;` declaration
/// blocks. Mixed pairs — a use followed by a mod declaration, either
/// followed by anything else — get the blank line.
fn stays_tight(prev: &syn::Item, next: &syn::Item) -> bool {
    let is_use = |item: &syn::Item| matches!(item, syn::Item::Use(_));
    let is_mod_decl = |item: &syn::Item| {
        matches!(item, syn::Item::Mod(module) if module.content.is_none())
    };
    (is_use(prev) && is_use(next)) || (is_mod_decl(prev) && is_mod_decl(next))
}

/// The 1-based source line where `item` starts. `syn::Item`'s
/// `ToTokens` emits outer attributes first, so the joined span starts
/// at the doc-comment/attribute stack — the blank line goes before the
/// docs, never between the docs and their item (pinned by the
/// `doc_and_attr_stacks_stay_attached` test below).
fn item_start_line(item: &syn::Item) -> usize {
    item.span().start().line
}

#[cfg(test)]
mod tests {
    use super::space_rendered;

    fn spaced(packed: &str) -> String {
        space_rendered(packed.to_string())
    }

    /// Adjacent types come apart; nothing is inserted inside a body.
    #[test]
    fn types_are_separated() {
        let out = spaced(
            "pub struct A {\n    pub x: i32,\n    pub y: i32,\n}\npub struct B {\n    pub z: i32,\n}\n",
        );
        assert_eq!(
            out,
            "pub struct A {\n    pub x: i32,\n    pub y: i32,\n}\n\npub struct B {\n    pub z: i32,\n}\n",
        );
    }

    /// The import preamble stays one tight block, with the blank line
    /// before the first type — the leaf-file shape.
    #[test]
    fn use_block_stays_tight() {
        let out = spaced(
            "use ::serde::{Deserialize, Serialize};\nuse ::struct_patch::Patch;\npub use super::support::error;\npub struct A;\n",
        );
        assert_eq!(
            out,
            "use ::serde::{Deserialize, Serialize};\nuse ::struct_patch::Patch;\npub use super::support::error;\n\npub struct A;\n",
        );
    }

    /// Body-less module declarations stay one tight block — the root
    /// `mod.rs` shape — including a doc-commented declaration mid-run.
    #[test]
    fn mod_declaration_runs_stay_tight() {
        let packed =
            "pub mod create_pet;\npub mod get_pet;\npub mod shared;\n/// Shared support items.\npub mod support;\n";
        assert_eq!(spaced(packed), packed);
    }

    /// Items inside inline module bodies are spaced at every depth,
    /// and an inline-body module is separated from its neighbors.
    #[test]
    fn nested_module_bodies_are_spaced() {
        let out = spaced(
            "pub mod a {\n    use x::y;\n    pub struct A;\n    pub mod b {\n        pub struct B;\n        pub type T = i32;\n    }\n}\npub struct C;\n",
        );
        assert_eq!(
            out,
            "pub mod a {\n    use x::y;\n\n    pub struct A;\n\n    pub mod b {\n        pub struct B;\n\n        pub type T = i32;\n    }\n}\n\npub struct C;\n",
        );
    }

    /// The condensed style's macro invocation is an item like any
    /// other: separated from the enum it implements.
    #[test]
    fn enum_and_macro_invocation_are_separated() {
        let out = spaced(
            "pub enum E {\n    A,\n}\nimpl_string_enum!(E {\n    A => \"a\",\n});\n",
        );
        assert_eq!(
            out,
            "pub enum E {\n    A,\n}\n\nimpl_string_enum!(E {\n    A => \"a\",\n});\n",
        );
    }

    /// The blank line goes before an item's doc-comment/attribute
    /// stack, never between the docs and their item.
    #[test]
    fn doc_and_attr_stacks_stay_attached() {
        let out = spaced(
            "pub struct A;\n///Doc for B.\n#[derive(Debug)]\npub struct B;\n",
        );
        assert_eq!(
            out,
            "pub struct A;\n\n///Doc for B.\n#[derive(Debug)]\npub struct B;\n",
        );
    }

    /// The pass is whitespace-only: the spaced output re-parses to the
    /// identical token stream (the internal gate; asserted here
    /// end-to-end), and the trailing-newline shape is preserved.
    #[test]
    fn token_identity_and_trailing_newline_hold() {
        use quote::ToTokens as _;

        let packed = "pub mod a {\n    ///Doc.\n    pub struct A;\n    impl A {\n        pub fn f() {}\n    }\n}\npub struct B;";
        let out = spaced(packed);
        assert_ne!(out, packed, "spacing must have inserted blank lines");
        assert!(!out.ends_with('\n'), "no trailing newline appears from nowhere");

        let before = syn::parse_file(packed).unwrap().to_token_stream().to_string();
        let after = syn::parse_file(&out).unwrap().to_token_stream().to_string();
        assert_eq!(before, after);
    }

    /// Input that does not parse is returned unchanged.
    #[test]
    fn unparsable_input_is_untouched() {
        let broken = "pub struct {\n";
        assert_eq!(spaced(broken), broken);
    }
}

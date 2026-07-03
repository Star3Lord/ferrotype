//! The readability passes every rendered document goes through:
//! doc-comment normalization before prettyplease, item spacing after.
//!
//! **Doc normalization** ([`normalize_docs`]): typify carries schema
//! descriptions as raw `#[doc = "..."]` strings, which prettyplease
//! renders cramped (`///text`, no space after the slashes), as
//! `/** ... */` blocks when the string holds newlines, and at whatever
//! line length the spec author used. The pass rewrites the doc
//! attributes on every item, struct field, and enum variant (recursing
//! into inline module bodies): multi-line strings split into stacked
//! single-line `#[doc]`s (adjacent doc attributes concatenate with
//! newlines in rustdoc, so this is rendering-equivalent), every
//! non-empty line gets exactly one leading space, and lines longer
//! than [`DOC_WIDTH`] soft-wrap at word boundaries — each source line
//! individually, never re-flowing across the spec's own line
//! structure (markdown collapses single newlines, so the wrap is
//! display-equivalent). Doc blocks containing a fenced code block
//! (schema-in-docs `<details>` sections with ```json fences and
//! deliberate alignment) pass through byte-untouched, as do
//! non-name-value forms like `#[doc(hidden)]` and inner attributes.
//!
//! **Item spacing** ([`space_rendered`]): prettyplease emits no blank
//! line between items, so generated output reads as a wall — a
//! struct's closing brace with the next type's doc comment on the very
//! next line. The pass separates items with a single blank line, at
//! the top level and inside every inline `pub mod { ... }` body at any
//! depth, keeping runs of one-line declarations tight: consecutive
//! `use` items (the import preamble, `pub use` re-exports) and
//! consecutive body-less `pub mod x;` declarations (the root `mod.rs`
//! shape) stay contiguous blocks. Everything else — types, impls,
//! inline-body modules, macro definitions and invocations — gets
//! separated. Same safety posture as
//! [`polish_rendered`](crate::condense::polish_rendered): blank lines
//! are only inserted at item boundaries (span line numbers of the
//! re-parsed source, which start at an item's doc/attribute stack, so
//! docs move with their item), never inside an item, and the result
//! must re-parse to the identical token stream or the input is
//! returned unchanged.

use quote::ToTokens;
use syn::spanned::Spanned;

use crate::condense::{finish_lines, polish_rendered, tokens_equal};

/// Render one output document's body: doc normalization, prettyplease,
/// the condensed style's macro polish, then item spacing. The shared
/// final step of [`render_file`](crate::render_file) and the tree
/// writer's per-file rendering
/// ([`plan_file_tree`](crate::plan_file_tree)), so single-file and
/// folder-tree output are formatted identically.
pub(crate) fn render_body(file: &syn::File) -> String {
    let mut file = file.clone();
    normalize_docs(&mut file.items);
    space_rendered(polish_rendered(prettyplease::unparse(&file)))
}

/// Maximum characters of doc-line content (excluding the injected
/// leading space). Rendered as `/// ` + content, a wrapped line stays
/// within ~100 columns at the module depths generated output uses
/// (96 at the top level, 100 one level deep).
const DOC_WIDTH: usize = 92;

/// Normalize the doc attributes of every item in `items`, recursing
/// into inline module bodies, struct/union fields, and enum variants
/// (including variant fields).
pub(crate) fn normalize_docs(items: &mut [syn::Item]) {
    for item in items {
        if let Some(attrs) = item_attrs_mut(item) {
            normalize_attr_docs(attrs);
        }
        match item {
            syn::Item::Mod(module) => {
                if let Some((_, children)) = &mut module.content {
                    normalize_docs(children);
                }
            }
            syn::Item::Struct(item_struct) => {
                for field in &mut item_struct.fields {
                    normalize_attr_docs(&mut field.attrs);
                }
            }
            syn::Item::Enum(item_enum) => {
                for variant in &mut item_enum.variants {
                    normalize_attr_docs(&mut variant.attrs);
                    for field in &mut variant.fields {
                        normalize_attr_docs(&mut field.attrs);
                    }
                }
            }
            syn::Item::Union(item_union) => {
                for field in &mut item_union.fields.named {
                    normalize_attr_docs(&mut field.attrs);
                }
            }
            _ => {}
        }
    }
}

/// The outer attribute list of an item, for the kinds that carry one.
fn item_attrs_mut(item: &mut syn::Item) -> Option<&mut Vec<syn::Attribute>> {
    match item {
        syn::Item::Const(i) => Some(&mut i.attrs),
        syn::Item::Enum(i) => Some(&mut i.attrs),
        syn::Item::ExternCrate(i) => Some(&mut i.attrs),
        syn::Item::Fn(i) => Some(&mut i.attrs),
        syn::Item::ForeignMod(i) => Some(&mut i.attrs),
        syn::Item::Impl(i) => Some(&mut i.attrs),
        syn::Item::Macro(i) => Some(&mut i.attrs),
        syn::Item::Mod(i) => Some(&mut i.attrs),
        syn::Item::Static(i) => Some(&mut i.attrs),
        syn::Item::Struct(i) => Some(&mut i.attrs),
        syn::Item::Trait(i) => Some(&mut i.attrs),
        syn::Item::TraitAlias(i) => Some(&mut i.attrs),
        syn::Item::Type(i) => Some(&mut i.attrs),
        syn::Item::Union(i) => Some(&mut i.attrs),
        syn::Item::Use(i) => Some(&mut i.attrs),
        _ => None,
    }
}

/// Rewrite one attribute owner's outer `#[doc = "..."]` name-value
/// attributes into normalized single-line ones, in place and in order.
/// Skipped wholesale when any doc line opens a fenced code block — the
/// schema-in-docs `<details>` sections must pass through untouched.
fn normalize_attr_docs(attrs: &mut Vec<syn::Attribute>) {
    let has_fence = attrs.iter().any(|attr| {
        doc_value(attr).is_some_and(|value| {
            value.lines().any(|line| line.trim_start().starts_with("```"))
        })
    });
    if has_fence {
        return;
    }

    let mut normalized: Vec<syn::Attribute> = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        let Some(value) = doc_value(&attr) else {
            normalized.push(attr);
            continue;
        };
        for line in normalized_doc_lines(&value) {
            normalized.push(syn::parse_quote!(#[doc = #line]));
        }
    }
    *attrs = normalized;
}

/// The string value of an outer `#[doc = "..."]` name-value attribute;
/// `None` for everything else (`#[doc(hidden)]`, inner docs, non-doc
/// attributes), which the pass leaves untouched.
fn doc_value(attr: &syn::Attribute) -> Option<String> {
    if !matches!(attr.style, syn::AttrStyle::Outer) || !attr.path().is_ident("doc") {
        return None;
    }
    let syn::Meta::NameValue(name_value) = &attr.meta else {
        return None;
    };
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(lit),
        ..
    }) = &name_value.value
    else {
        return None;
    };
    Some(lit.value())
}

/// One doc attribute's value as normalized single-line doc strings:
/// split on the embedded newlines (dropping empty edge lines, the
/// artifact of descriptions ending in `\n` — interior empty lines are
/// paragraph separators and stay), each non-empty line trimmed,
/// wrapped at [`DOC_WIDTH`], and given exactly one leading space.
fn normalized_doc_lines(value: &str) -> Vec<String> {
    let mut lines: Vec<&str> = value.split('\n').collect();
    if lines.len() > 1 {
        while lines.first().is_some_and(|line| line.trim().is_empty()) {
            lines.remove(0);
        }
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
    }

    let mut normalized = Vec::with_capacity(lines.len());
    for line in lines {
        let text = line.trim();
        if text.is_empty() {
            normalized.push(String::new());
        } else if text.chars().count() <= DOC_WIDTH {
            normalized.push(format!(" {text}"));
        } else {
            normalized.extend(wrap_words(text).into_iter().map(|chunk| format!(" {chunk}")));
        }
    }
    normalized
}

/// Greedy word wrap of one line at [`DOC_WIDTH`] characters. A single
/// word longer than the width stands alone unbroken; text is never
/// re-flowed across input lines (each line wraps independently).
fn wrap_words(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for word in text.split_whitespace() {
        let word_width = word.chars().count();
        if current.is_empty() {
            current.push_str(word);
            current_width = word_width;
        } else if current_width + 1 + word_width <= DOC_WIDTH {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            chunks.push(std::mem::take(&mut current));
            current.push_str(word);
            current_width = word_width;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
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

    // ─── Doc normalization ──────────────────────────────────────────

    /// Parse, normalize docs, and render — the pass runs before
    /// prettyplease, so assertions read the final `///` shape.
    fn normalized(source: &str) -> String {
        let mut file = syn::parse_file(source).unwrap();
        super::normalize_docs(&mut file.items);
        prettyplease::unparse(&file)
    }

    /// A multi-line doc string becomes stacked `///` lines, not a
    /// `/** ... */` block, and a trailing newline in the value doesn't
    /// leave a dangling empty doc line.
    #[test]
    fn multi_line_docs_split_into_doc_lines() {
        let out = normalized(
            "#[doc = \"Lists the journeys.\\n* For one-way, one element.\\n* For round-trip, two.\\n\"]\npub struct A;\n",
        );
        assert_eq!(
            out,
            "/// Lists the journeys.\n/// * For one-way, one element.\n/// * For round-trip, two.\npub struct A;\n",
        );
    }

    /// Every non-empty line gets exactly one leading space — added
    /// when missing, collapsed when the spec indented further.
    #[test]
    fn doc_lines_get_one_leading_space() {
        let out = normalized("#[doc = \"Allows pricing.\"]\npub struct A;\n");
        assert_eq!(out, "/// Allows pricing.\npub struct A;\n");

        let out = normalized("#[doc = \"   deeply indented\"]\npub struct A;\n");
        assert_eq!(out, "/// deeply indented\npub struct A;\n");

        let already = "/// Already spaced.\npub struct A;\n";
        assert_eq!(normalized(already), already);
    }

    /// Interior empty lines survive as `///` paragraph separators.
    #[test]
    fn interior_empty_doc_lines_survive() {
        let out = normalized("#[doc = \"First.\\n\\nSecond.\"]\npub struct A;\n");
        assert_eq!(out, "/// First.\n///\n/// Second.\npub struct A;\n");
    }

    /// Lines wrap at the width boundary — at most `DOC_WIDTH` content
    /// characters per line — without re-flowing across input lines,
    /// and an unbreakable over-long word stands alone.
    #[test]
    fn long_doc_lines_wrap_at_word_boundaries() {
        use super::DOC_WIDTH;

        // 15 chars + separator per repeat; two repeats over the width.
        let word = "abcdefghijklmn"; // 14 chars
        let per_line = DOC_WIDTH / 15; // words per full line
        let text = vec![word; per_line * 2].join(" ");
        let out = normalized(&format!("#[doc = \"{text}\"]\npub struct A;\n"));
        let full_line = format!("/// {}", vec![word; per_line].join(" "));
        assert_eq!(out, format!("{full_line}\n{full_line}\npub struct A;\n"));

        // Exactly at the width: no wrap.
        let exact = "x".repeat(DOC_WIDTH);
        let out = normalized(&format!("#[doc = \"{exact}\"]\npub struct A;\n"));
        assert_eq!(out, format!("/// {exact}\npub struct A;\n"));

        // One char over, as a single unbreakable word: kept whole.
        let over = "x".repeat(DOC_WIDTH + 1);
        let out = normalized(&format!("#[doc = \"{over}\"]\npub struct A;\n"));
        assert_eq!(out, format!("/// {over}\npub struct A;\n"));

        // Two input lines never merge, even when both are short.
        let out = normalized("#[doc = \"short one\\nshort two\"]\npub struct A;\n");
        assert_eq!(out, "/// short one\n/// short two\npub struct A;\n");
    }

    /// A doc block containing a fenced code block passes through
    /// byte-untouched — the schema-in-docs `<details>` sections rely
    /// on their exact alignment.
    #[test]
    fn fenced_doc_blocks_are_skipped() {
        let source = "#[doc = \"Summary.\\n\\n<details><summary>JSON schema</summary>\\n\\n```json\\n{ \\\"type\\\": \\\"object\\\" }\\n```\\n</details>\"]\npub struct A;\n";
        let untouched = prettyplease::unparse(&syn::parse_file(source).unwrap());
        assert_eq!(normalized(source), untouched);
        assert!(normalized(source).contains("```json"));
    }

    /// Struct fields and enum variants are normalized; non-name-value
    /// doc forms and other attributes are left alone.
    #[test]
    fn field_and_variant_docs_are_normalized() {
        let out = normalized(
            "pub struct A {\n    #[doc = \"Field doc.\"]\n    pub x: i32,\n}\npub enum E {\n    #[doc = \"Variant doc.\"]\n    V,\n}\n#[doc(hidden)]\n#[doc = \"docs\"]\npub struct H;\n",
        );
        assert!(out.contains("/// Field doc.\n    pub x: i32"), "{out}");
        assert!(out.contains("/// Variant doc.\n    V"), "{out}");
        assert!(out.contains("#[doc(hidden)]\n/// docs\npub struct H;"), "{out}");
    }

    /// Docs inside inline module bodies are normalized at every depth.
    #[test]
    fn nested_module_docs_are_normalized() {
        let out = normalized(
            "pub mod a {\n    pub mod b {\n        #[doc = \"Deep.\"]\n        pub struct D;\n    }\n}\n",
        );
        assert!(out.contains("/// Deep.\n        pub struct D;"), "{out}");
    }

    /// Applying the pass twice equals applying it once.
    #[test]
    fn doc_normalization_is_idempotent() {
        let long = "word ".repeat(60);
        let source = format!(
            "#[doc = \"First.\\n  indented line\\n\\n{long}\\n\"]\npub struct A {{\n    #[doc = \"f\"]\n    pub x: i32,\n}}\n",
        );
        let mut once = syn::parse_file(&source).unwrap();
        super::normalize_docs(&mut once.items);
        let mut twice = once.clone();
        super::normalize_docs(&mut twice.items);
        assert_eq!(
            prettyplease::unparse(&once),
            prettyplease::unparse(&twice),
        );
    }
}

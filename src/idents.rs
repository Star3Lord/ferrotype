//! Rust identifier forms of schema names, matching typify's internal
//! sanitization.
//!
//! Config selectors (`[types."X"]`, `[fields."Type.field"]`, `[[rules]]`
//! globs) are written against schema names; the generation-time hooks and
//! the AST passes observe the *Rust* names typify emits. The old fork
//! exported its sanitizers (`typify::rust_type_ident` /
//! `typify::rust_field_ident`) for exactly this translation; the current
//! fork removed them in favor of [`typify::TypeSpace::iter_definitions`] +
//! `Type::name()`. Selector resolution, however, happens *before* a
//! populated `TypeSpace` exists (settings like `with_replacement` are
//! keyed by the sanitized name), so this module ports the fork's
//! `sanitize` verbatim. Drift is guarded by comparing against
//! `iter_definitions` output wherever a `TypeSpace` is in hand — the
//! names must agree or generation fails loudly (see
//! [`crate::partition::Partition::to_rust_partition`]).

use heck::{ToPascalCase, ToSnakeCase};
use typify::accept_as_ident;
use unicode_ident::{is_xid_continue, is_xid_start};

enum Case {
    Pascal,
    Snake,
}

/// Port of typify's `sanitize` (util.rs): the exact transformation from a
/// schema name to the identifier typify generates for it.
fn sanitize(input: &str, case: Case) -> String {
    let to_case = match case {
        Case::Pascal => str::to_pascal_case,
        Case::Snake => str::to_snake_case,
    };

    // Replace hyphens or whatever else people might use with underscores;
    // the +1/-1 cases mirror typify's special handling.
    let out = match input {
        "+1" => "plus1".to_string(),
        "-1" => "minus1".to_string(),
        _ => to_case(&input.replace("'", "").replace(|c| !is_xid_continue(c), "-")),
    };

    let prefix = to_case("x");

    let out = match out.chars().next() {
        None => prefix,
        Some(c) if is_xid_start(c) => out,
        Some(_) => format!("{}{}", prefix, out),
    };

    if accept_as_ident(&out) {
        out
    } else {
        format!("{}_", out)
    }
}

/// The Rust type identifier typify generates for a schema/definition name
/// (Pascal case, keyword-safe). `rust_type_ident("useCSL") == "UseCsl"`.
pub fn rust_type_ident(name: &str) -> String {
    sanitize(name, Case::Pascal)
}

/// The Rust field identifier typify generates for a wire property name
/// (snake case, keyword-safe). `rust_field_ident("photoUrls") == "photo_urls"`.
pub fn rust_field_ident(name: &str) -> String {
    sanitize(name, Case::Snake)
}

/// Whether a struct-level `#[serde(rename_all = "<case>")]` makes a
/// per-field `#[serde(rename = "<wire_name>")]` redundant — applying
/// `<case>` to the snake-cased Rust field name reproduces `<wire_name>`
/// verbatim. Unrecognized cases return `false`, preserving the rename.
/// (Port of the old fork's `rename_all_covers_rename`.)
pub(crate) fn rename_all_covers_rename(rust_field: &str, wire_name: &str, case: &str) -> bool {
    use heck::{ToKebabCase, ToLowerCamelCase, ToShoutyKebabCase, ToShoutySnakeCase};
    let transformed = match case {
        "lowercase" => rust_field.to_lowercase().replace('_', ""),
        "UPPERCASE" => rust_field.to_uppercase().replace('_', ""),
        "PascalCase" => rust_field.to_pascal_case(),
        "camelCase" => rust_field.to_lower_camel_case(),
        "snake_case" => rust_field.to_snake_case(),
        "SCREAMING_SNAKE_CASE" => rust_field.to_shouty_snake_case(),
        "kebab-case" => rust_field.to_kebab_case(),
        "SCREAMING-KEBAB-CASE" => rust_field.to_shouty_kebab_case(),
        _ => return false,
    };
    transformed == wire_name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_idents_match_typify() {
        assert_eq!(
            rust_type_ident("CancelBookingRequest"),
            "CancelBookingRequest"
        );
        assert_eq!(rust_type_ident("useCSL"), "UseCsl");
        assert_eq!(rust_type_ident("base-thing"), "BaseThing");
        // The +1/-1 special cases bypass recasing, exactly as in typify.
        assert_eq!(rust_type_ident("+1"), "plus1");
    }

    #[test]
    fn field_idents_match_typify() {
        assert_eq!(rust_field_ident("photoUrls"), "photo_urls");
        assert_eq!(rust_field_ident("type"), "type_");
    }

    #[test]
    fn rename_all_coverage() {
        assert!(rename_all_covers_rename(
            "photo_urls",
            "photoUrls",
            "camelCase"
        ));
        assert!(!rename_all_covers_rename(
            "photo_urls",
            "PhotoURLs",
            "camelCase"
        ));
        assert!(rename_all_covers_rename(
            "root_rq",
            "ROOT-RQ",
            "SCREAMING-KEBAB-CASE"
        ));
        assert!(!rename_all_covers_rename("a", "a", "unknown-case"));
    }
}

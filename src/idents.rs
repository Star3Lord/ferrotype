//! Rust identifier forms of schema names, matching typify's sanitization.
//!
//! Config selectors (`[types."X"]`, `[fields."Type.field"]`, `[[rules]]`
//! globs) are written against schema names; the generation-time settings
//! and the AST passes observe the *Rust* names typify emits. The fork
//! exports its sanitizers for exactly this pre-generation translation —
//! re-exported here (and from the crate root) so every internal call
//! site shares one import path. Wherever a populated
//! [`typify::TypeSpace`] exists, `iter_definitions` + `Type::name()`
//! remain the authoritative bridge.

pub use typify::{rust_field_ident, rust_type_ident};

/// Whether a struct-level `#[serde(rename_all = "<case>")]` makes a
/// per-field `#[serde(rename = "<wire_name>")]` redundant — applying
/// `<case>` to the snake-cased Rust field name reproduces `<wire_name>`
/// verbatim. Unrecognized cases return `false`, preserving the rename.
/// (Port of the old fork's `rename_all_covers_rename`, which drove its
/// generation-time elision; the decoration pass applies the same rule to
/// the AST.)
pub(crate) fn rename_all_covers_rename(rust_field: &str, wire_name: &str, case: &str) -> bool {
    use heck::{
        ToKebabCase, ToLowerCamelCase, ToPascalCase, ToShoutyKebabCase, ToShoutySnakeCase,
        ToSnakeCase,
    };
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
        // The special cases bypass recasing, mirroring generation exactly.
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

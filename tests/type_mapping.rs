//! Custom type mappings (docs/MIGRATION.md D18): the `[style] formats`
//! table (schema `type`+`format` → arbitrary Rust type, the fork's
//! `with_format_type`) and the per-type `replace` override (named schema
//! → existing Rust type, the fork's upstream `with_replacement`).
//!
//! Assertions run on the generated source text — the mapped types are
//! deliberately not dependencies of this crate.

use openapi_codegen::{Generator, StyleConfig, StyleProfile};

/// Write `document` (an OpenAPI JSON document) to a temp spec file and
/// return a generator for it.
fn generator_for(name: &str, profile: StyleProfile, document: serde_json::Value) -> Generator {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("type_mapping");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    Generator::new(path).profile(profile)
}

/// A schema exercising one field per mappable (instance type, format)
/// pair, plus an unmapped-format control.
fn event_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "Event": {
                "type": "object",
                "required": ["createdAt", "startDate", "amount", "count"],
                "properties": {
                    "createdAt": { "type": "string", "format": "date-time" },
                    "startDate": { "type": "string", "format": "date" },
                    "amount": { "type": "string", "format": "decimal" },
                    "count": { "type": "integer", "format": "int64" }
                }
            }
        } }
    })
}

/// Cross-referencing schemas for the `replace` tests: `Order` holds a
/// required and an optional `Money`, plus an optional `Category` as the
/// deep-patch control.
fn order_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "Money": {
                "type": "object",
                "required": ["amount"],
                "properties": { "amount": { "type": "string" } }
            },
            "Order": {
                "type": "object",
                "required": ["total"],
                "properties": {
                    "total": { "$ref": "#/components/schemas/Money" },
                    "discount": { "$ref": "#/components/schemas/Money" },
                    "category": { "$ref": "#/components/schemas/Category" }
                }
            },
            "Category": {
                "type": "object",
                "properties": { "name": { "type": "string" } }
            }
        } }
    })
}

// ─── [style] formats ─────────────────────────────────────────────────────────

#[test]
fn formats_table_maps_type_format_pairs() {
    let out = generator_for("formats_hit", StyleProfile::Typify, event_spec())
        .style(|style| {
            style
                .formats
                .insert("string/date-time".into(), "::time::OffsetDateTime".into());
            style
                .formats
                .insert("string/decimal".into(), "::rust_decimal::Decimal".into());
            style
                .formats
                .insert("integer/int64".into(), "::my_crate::BigInt".into());
        })
        .generate_to_string()
        .unwrap();

    assert!(out.contains("pub created_at: ::time::OffsetDateTime"), "{out}");
    assert!(out.contains("pub amount: ::rust_decimal::Decimal"), "{out}");
    assert!(out.contains("pub count: ::my_crate::BigInt"), "{out}");
    // Unmapped format: the default path survives untouched (the typify
    // profile keeps upstream's chrono mapping for `date`).
    assert!(out.contains("pub start_date: ::chrono::naive::NaiveDate"), "{out}");
}

#[test]
fn formats_win_over_sugar_keys() {
    // The api-client preset sets the date/date-time/uuid sugar keys to
    // String; a formats entry for the same format wins, while formats
    // the table doesn't name keep honoring the sugar.
    let out = generator_for("formats_precedence", StyleProfile::ApiClient, event_spec())
        .style(|style| {
            assert_eq!(style.date_time.as_deref(), Some("::std::string::String"));
            style
                .formats
                .insert("string/date-time".into(), "::time::OffsetDateTime".into());
        })
        .generate_to_string()
        .unwrap();

    assert!(out.contains("pub created_at: ::time::OffsetDateTime"), "{out}");
    assert!(out.contains("pub start_date: ::std::string::String"), "{out}");
}

#[test]
fn formats_round_trip_through_codegen_toml() {
    let config = StyleConfig::from_toml_str(
        "profile = \"api-client\"\n\
         [style.formats]\n\
         \"string/date-time\" = \"::time::OffsetDateTime\"\n\
         \"number/decimal\" = \"::rust_decimal::Decimal\"\n",
        StyleConfig::api_client(),
    )
    .unwrap();
    assert_eq!(
        config.formats.get("string/date-time").map(String::as_str),
        Some("::time::OffsetDateTime"),
    );
    assert_eq!(
        config.formats.get("number/decimal").map(String::as_str),
        Some("::rust_decimal::Decimal"),
    );
}

#[test]
fn malformed_formats_key_errors() {
    let error = format!(
        "{:#}",
        generator_for("formats_bad_key", StyleProfile::ApiClient, event_spec())
            .style(|style| {
                style
                    .formats
                    .insert("date-time".into(), "::time::OffsetDateTime".into());
            })
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(
        error.contains("date-time") && error.contains("<instance-type>/<format>"),
        "{error}",
    );
}

// ─── [types] replace ─────────────────────────────────────────────────────────

#[test]
fn replace_swaps_generated_type_for_existing_path() {
    let out = generator_for("replace_hit", StyleProfile::ApiClient, order_spec())
        .style(|style| {
            style.types.entry("Money".to_string()).or_default().replace =
                Some("::my_crate::Money".to_string());
        })
        .generate_to_string()
        .unwrap();

    // No item is generated for the schema...
    assert!(!out.contains("pub struct Money"), "{out}");
    assert!(!out.contains("MoneyPatch"), "{out}");
    // ...and every reference names the replacement, `Option<...>`
    // wrappers included.
    assert!(out.contains("pub total: ::my_crate::Money"), "{out}");
    assert!(
        out.contains("pub discount: ::std::option::Option<::my_crate::Money>"),
        "{out}",
    );
}

#[test]
fn replaced_type_gets_no_deep_patch_annotation() {
    let out = generator_for("replace_deep_patch", StyleProfile::ApiClient, order_spec())
        .style(|style| {
            style.types.entry("Money".to_string()).or_default().replace =
                Some("::my_crate::Money".to_string());
        })
        .generate_to_string()
        .unwrap();

    // The api-client profile deep-patches every Option<Struct> field:
    // the generated Category still gets its annotation, the replaced
    // Money must not (no `MoneyPatch` companion exists).
    assert!(out.contains("#[patch(name = \"Option<CategoryPatch>\")]"), "{out}");
    assert!(!out.contains("MoneyPatch"), "{out}");
}

#[test]
fn replace_combos_with_generation_overrides_error() {
    for (label, mutate) in [
        (
            "patch",
            Box::new(|override_: &mut openapi_codegen::config::TypeOverride| {
                override_.patch = Some(false);
            }) as Box<dyn Fn(&mut openapi_codegen::config::TypeOverride)>,
        ),
        (
            "derives-add",
            Box::new(|override_: &mut openapi_codegen::config::TypeOverride| {
                override_.derives_add = vec!["Hash".to_string()];
            }),
        ),
        (
            "module",
            Box::new(|override_: &mut openapi_codegen::config::TypeOverride| {
                override_.module = Some("shared".to_string());
            }),
        ),
    ] {
        let error = format!(
            "{:#}",
            generator_for(
                &format!("replace_combo_{label}"),
                StyleProfile::ApiClient,
                order_spec(),
            )
            .style(move |style| {
                let override_ = style.types.entry("Money".to_string()).or_default();
                override_.replace = Some("::my_crate::Money".to_string());
                mutate(override_);
            })
            .generate_to_string()
            .unwrap_err(),
        );
        assert!(
            error.contains("combines `replace`"),
            "combo with {label} should error: {error}",
        );
    }
}

#[test]
fn replace_impls_without_replace_errors() {
    let error = format!(
        "{:#}",
        generator_for("replace_impls_alone", StyleProfile::ApiClient, order_spec())
            .style(|style| {
                style
                    .types
                    .entry("Money".to_string())
                    .or_default()
                    .replace_impls = vec![openapi_codegen::config::ReplaceImpl::Display];
            })
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(error.contains("`replace-impls` without `replace`"), "{error}");
}

#[test]
fn unmatched_replace_selector_errors() {
    let error = format!(
        "{:#}",
        generator_for("replace_unmatched", StyleProfile::ApiClient, order_spec())
            .style(|style| {
                style.types.entry("Ghost".to_string()).or_default().replace =
                    Some("::my_crate::Ghost".to_string());
            })
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(error.contains("appears nowhere"), "{error}");
}

#[test]
fn replace_and_replace_impls_parse_from_codegen_toml() {
    let config = StyleConfig::from_toml_str(
        "profile = \"api-client\"\n\
         [types.\"Money\"]\n\
         replace = \"::my_crate::Money\"\n\
         replace-impls = [\"display\", \"from-str\", \"default\"]\n",
        StyleConfig::api_client(),
    )
    .unwrap();
    let override_ = &config.types["Money"];
    assert_eq!(override_.replace.as_deref(), Some("::my_crate::Money"));
    assert_eq!(
        override_.replace_impls,
        vec![
            openapi_codegen::config::ReplaceImpl::Display,
            openapi_codegen::config::ReplaceImpl::FromStr,
            openapi_codegen::config::ReplaceImpl::Default,
        ],
    );
}

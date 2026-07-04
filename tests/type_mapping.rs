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
        config
            .formats
            .get("string/date-time")
            .map(openapi_codegen::config::FormatMapping::type_path),
        Some("::time::OffsetDateTime"),
    );
    assert_eq!(
        config
            .formats
            .get("number/decimal")
            .map(openapi_codegen::config::FormatMapping::type_path),
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
                    .replace_impls = vec![openapi_codegen::config::Capability::Display];
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
            openapi_codegen::config::Capability::Display,
            openapi_codegen::config::Capability::FromStr,
            openapi_codegen::config::Capability::Default,
        ],
    );
}

// ─── Table-form mappings: field attrs and capabilities (D19) ─────────────────

/// A spec whose `Event` has required and optional date-times plus a
/// struct chain for transitive pruning: `Wrapper` requires `Event`,
/// `Deep` requires `Wrapper`, `Holder` holds `Option<Event>`.
fn chain_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "Event": {
                "type": "object",
                "required": ["createdAt"],
                "properties": {
                    "createdAt": { "type": "string", "format": "date-time" },
                    "updatedAt": { "type": "string", "format": "date-time" }
                }
            },
            "Wrapper": {
                "type": "object",
                "required": ["event"],
                "properties": { "event": { "$ref": "#/components/schemas/Event" } }
            },
            "Deep": {
                "type": "object",
                "required": ["wrapper"],
                "properties": { "wrapper": { "$ref": "#/components/schemas/Wrapper" } }
            },
            "Holder": {
                "type": "object",
                "properties": { "event": { "$ref": "#/components/schemas/Event" } }
            }
        } }
    })
}

fn time_mapping() -> openapi_codegen::config::FormatMapping {
    openapi_codegen::config::FormatMapping::Table(openapi_codegen::config::FormatMappingTable {
        type_path: "::time::OffsetDateTime".to_string(),
        field_attrs: vec!["serde(with = \"time::serde::iso8601\")".to_string()],
        optional_field_attrs: vec!["serde(with = \"time::serde::iso8601::option\")".to_string()],
        impls: vec![
            openapi_codegen::config::Capability::Serialize,
            openapi_codegen::config::Capability::Deserialize,
        ],
    })
}

#[test]
fn mapping_attrs_attach_by_field_optionality() {
    let out = generator_for("attrs_attach", StyleProfile::ApiClient, chain_spec())
        .style(|style| {
            style.formats.insert("string/date-time".into(), time_mapping());
        })
        .generate_to_string()
        .unwrap();

    // Required field: the plain module; optional field: the ::option
    // module — never the plain one.
    assert!(
        out.contains(
            "#[serde(with = \"time::serde::iso8601\")]\n    pub created_at: ::time::OffsetDateTime",
        ),
        "{out}",
    );
    assert!(
        out.contains(
            "#[serde(with = \"time::serde::iso8601::option\")]\n    pub updated_at: \
             ::std::option::Option<::time::OffsetDateTime>",
        ),
        "{out}",
    );
}

#[test]
fn optional_fields_never_inherit_required_attrs() {
    // Only `field-attrs` configured: optional fields get nothing — a
    // `serde(with = ...)` module for `T` cannot handle `Option<T>`.
    let mapping = openapi_codegen::config::FormatMapping::Table(
        openapi_codegen::config::FormatMappingTable {
            type_path: "::time::OffsetDateTime".to_string(),
            field_attrs: vec!["serde(with = \"time::serde::iso8601\")".to_string()],
            optional_field_attrs: vec![],
            impls: vec![],
        },
    );
    let out = generator_for("attrs_no_fallback", StyleProfile::ApiClient, chain_spec())
        .style(move |style| {
            style.formats.insert("string/date-time".into(), mapping.clone());
        })
        .generate_to_string()
        .unwrap();

    let updated_at = out
        .lines()
        .zip(out.lines().skip(1))
        .find(|(_, next)| next.contains("pub updated_at"))
        .map(|(prev, _)| prev.to_string())
        .unwrap();
    assert!(
        !updated_at.contains("serde(with"),
        "optional field must not inherit field-attrs: {updated_at}",
    );
}

#[test]
fn invalid_attr_body_errors_with_config_key() {
    let mapping = openapi_codegen::config::FormatMapping::Table(
        openapi_codegen::config::FormatMappingTable {
            type_path: "::time::OffsetDateTime".to_string(),
            field_attrs: vec!["serde(with = ".to_string()],
            optional_field_attrs: vec![],
            impls: vec![],
        },
    );
    let error = format!(
        "{:#}",
        generator_for("attrs_invalid", StyleProfile::ApiClient, chain_spec())
            .style(move |style| {
                style.formats.insert("string/date-time".into(), mapping.clone());
            })
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(
        error.contains("string/date-time") && error.contains("field-attrs"),
        "{error}",
    );
}

#[test]
fn missing_capability_prunes_default_transitively() {
    let out = generator_for("prune_default", StyleProfile::ApiClient, chain_spec())
        .style(|style| {
            style.formats.insert("string/date-time".into(), time_mapping());
        })
        .generate_to_string()
        .unwrap();

    // Direct: Event has a required OffsetDateTime (no `default`
    // capability declared) → loses Default. Transitive: Wrapper
    // requires Event, Deep requires Wrapper → both lose it. The
    // mapping also declares no `partial-eq`, so PartialEq goes too
    // (for the equality family even Option-wrapped fields constrain).
    for name in ["Event", "Wrapper", "Deep"] {
        let derive_line = derive_line_of(&out, name);
        assert!(
            !derive_line.contains("Default") && !derive_line.contains("PartialEq"),
            "{name} must lose Default and PartialEq: {derive_line}",
        );
        assert!(
            derive_line.contains("Serialize") && derive_line.contains("Clone"),
            "{name} keeps the rest: {derive_line}",
        );
    }
    // Option-wrapped usage doesn't constrain Default — Holder keeps it —
    // but does constrain PartialEq (`Option<T>: PartialEq` needs
    // `T: PartialEq`), which Holder therefore loses.
    let holder = derive_line_of(&out, "Holder");
    assert!(holder.contains("Default"), "{holder}");
    assert!(!holder.contains("PartialEq"), "{holder}");

    // Patch companions: fields are all Option, so Default survives
    // there — while PartialEq is pruned from the companion too.
    let event_block_start = out.find("pub struct Event").unwrap();
    let block = &out[event_block_start.saturating_sub(800)..event_block_start];
    assert!(
        block.contains(
            "#[patch(attribute(derive(Debug, Clone, Default, Serialize, Deserialize)))]",
        ),
        "companion keeps Default, sheds PartialEq:\n{block}",
    );
}

/// The main `#[derive(...)]` line of the item named `name`.
fn derive_line_of<'a>(out: &'a str, name: &str) -> &'a str {
    let decl = format!("pub struct {name}");
    let position = out
        .find(&decl)
        .unwrap_or_else(|| panic!("no `{decl}` in output"));
    out[..position]
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("#[derive("))
        .unwrap_or_else(|| panic!("no derive line above {name}"))
}

#[test]
fn declared_default_capability_keeps_derive() {
    let mapping = openapi_codegen::config::FormatMapping::Table(
        openapi_codegen::config::FormatMappingTable {
            type_path: "::my_crate::Stamp".to_string(),
            field_attrs: vec![],
            optional_field_attrs: vec![],
            impls: vec![openapi_codegen::config::Capability::Default],
        },
    );
    let out = generator_for("keep_default", StyleProfile::ApiClient, chain_spec())
        .style(move |style| {
            style.formats.insert("string/date-time".into(), mapping.clone());
        })
        .generate_to_string()
        .unwrap();
    assert!(derive_line_of(&out, "Event").contains("Default"), "{out}");
    assert!(derive_line_of(&out, "Wrapper").contains("Default"), "{out}");
}

#[test]
fn formats_table_and_shorthand_parse_from_codegen_toml() {
    let config = StyleConfig::from_toml_str(
        "profile = \"api-client\"\n\
         [style.formats]\n\
         \"string/decimal\" = \"::rust_decimal::Decimal\"\n\
         [style.formats.\"string/date-time\"]\n\
         type = \"::time::OffsetDateTime\"\n\
         field-attrs = [\"serde(with = \\\"time::serde::iso8601\\\")\"]\n\
         optional-field-attrs = [\"serde(with = \\\"time::serde::iso8601::option\\\")\"]\n\
         impls = [\"serialize\", \"deserialize\"]\n",
        StyleConfig::api_client(),
    )
    .unwrap();

    let shorthand = &config.formats["string/decimal"];
    assert_eq!(shorthand.type_path(), "::rust_decimal::Decimal");

    let table = &config.formats["string/date-time"];
    assert_eq!(table.type_path(), "::time::OffsetDateTime");
    match table {
        openapi_codegen::config::FormatMapping::Table(table) => {
            assert_eq!(table.field_attrs.len(), 1);
            assert_eq!(table.optional_field_attrs.len(), 1);
            assert_eq!(table.impls.len(), 2);
        }
        openapi_codegen::config::FormatMapping::Path(_) => panic!("expected table form"),
    }

    // Unknown capability names are hard errors.
    let error = StyleConfig::from_toml_str(
        "[style.formats.\"string/date-time\"]\n\
         type = \"::time::OffsetDateTime\"\n\
         impls = [\"defaultable\"]\n",
        StyleConfig::api_client(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("formats"), "{error:#}");
}

#[test]
fn untagged_enum_default_synthesis_skipped_when_payload_lost_default() {
    // `Choice` is an untagged oneOf whose FIRST variant's payload
    // (`Payload`) loses Default through the mapping; the api-client
    // profile would normally synthesize `impl Default for Choice`.
    let spec = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "Payload": {
                "type": "object",
                "required": ["at"],
                "properties": { "at": { "type": "string", "format": "date-time" } }
            },
            "Other": {
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string" } }
            },
            "Choice": {
                "oneOf": [
                    { "$ref": "#/components/schemas/Payload" },
                    { "$ref": "#/components/schemas/Other" }
                ]
            },
            "Basket": {
                "type": "object",
                "required": ["choice"],
                "properties": { "choice": { "$ref": "#/components/schemas/Choice" } }
            }
        } }
    });
    let out = generator_for("enum_synthesis_guard", StyleProfile::ApiClient, spec)
        .style(|style| {
            style.formats.insert("string/date-time".into(), time_mapping());
        })
        .generate_to_string()
        .unwrap();

    // No synthesized Default for the enum, and the struct requiring it
    // loses its Default derive through the enum.
    assert!(
        !out.contains("impl ::std::default::Default for Choice"),
        "synthesis must be skipped:\n{out}",
    );
    assert!(
        !derive_line_of(&out, "Basket").contains("Default"),
        "Basket must lose Default through Choice: {}",
        derive_line_of(&out, "Basket"),
    );
}

// ─── The opt-in compile gate ─────────────────────────────────────────────────

/// A minimal spec so the scratch `cargo check` stays fast.
fn tiny_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "Thing": {
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string" } }
            }
        } }
    })
}

#[test]
fn verify_gate_passes_compiling_output() {
    generator_for("verify_ok", StyleProfile::ApiClient, tiny_spec())
        .verify_compile(true)
        .generate_to_string()
        .expect("compiling output must pass the gate");
}

#[test]
fn verify_gate_fails_non_compiling_output() {
    // A mapping to a nonexistent type produces output that cannot
    // compile; the gate must fail with the compiler's message.
    let error = format!(
        "{:#}",
        generator_for("verify_fail", StyleProfile::ApiClient, chain_spec())
            .style(|style| {
                style
                    .formats
                    .insert("string/date-time".into(), "::nonexistent_crate::Missing".into());
            })
            .verify_compile(true)
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(
        error.contains("failed to compile"),
        "gate must report the failure: {error}",
    );
    assert!(
        error.contains("nonexistent_crate"),
        "compiler output should name the missing crate: {error}",
    );
    // Unresolved-crate failures carry the targeted [verify] hint.
    assert!(
        error.contains("could not resolve: nonexistent_crate")
            && error.contains("[verify]")
            && error.contains("dependencies = ["),
        "the missing-crate hint should name the crate and the fix: {error}",
    );
}

//! Unit tests for IR-engine behavior the fixtures don't exercise
//! (docs/MIGRATION.md D5): oneOf/anyOf untagged enums, Option-via-null,
//! cycles → Box, inline-schema naming, name-collision errors, merge
//! fallback, config plumbing (style hooks, codegen.toml, overrides).

use openapi_codegen::{Engine, Generator, StyleConfig};

/// Write `document` (an OpenAPI JSON document) to a temp spec file and
/// return an IR-engine generator for it.
fn generator_for(name: &str, document: serde_json::Value) -> Generator {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ir_unit");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    Generator::new(path)
        .profile(openapi_codegen::StyleProfile::ApiClient)
        .engine(Engine::Ir)
}

fn spec_with_schemas(schemas: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": schemas }
    })
}

#[test]
fn untagged_one_of_enum_with_default_synthesis() {
    let out = generator_for(
        "untagged",
        spec_with_schemas(serde_json::json!({
            "Payload": {
                "oneOf": [
                    { "title": "ById", "type": "object",
                      "properties": { "id": { "type": "string" } },
                      "required": ["id"] },
                    { "title": "ByName", "type": "object",
                      "properties": { "name": { "type": "string" } },
                      "required": ["name"] }
                ]
            }
        })),
    )
    .generate_to_string()
    .unwrap();

    assert!(out.contains("#[serde(untagged)]"), "untagged serde attr:\n{out}");
    assert!(out.contains("pub enum Payload"), "enum emitted:\n{out}");
    assert!(out.contains("ById(PayloadById)"), "variant naming from titles:\n{out}");
    // The old postprocess.rs behavior, now ImplSynthPass: untagged enums
    // (no unit variant) get a first-variant Default so struct Default
    // derives hold up.
    assert!(
        out.contains("impl ::std::default::Default for Payload"),
        "untagged Default synthesized:\n{out}",
    );
    assert!(out.contains("Self::ById(::std::default::Default::default())"));
    // The synthetic variant payload types exist.
    assert!(out.contains("pub struct PayloadById"));
    assert!(out.contains("pub struct PayloadByName"));
}

#[test]
fn any_of_with_null_is_option_not_enum() {
    let out = generator_for(
        "nullable_anyof",
        spec_with_schemas(serde_json::json!({
            "Wrapper": {
                "type": "object",
                "properties": {
                    "inner": { "anyOf": [
                        { "$ref": "#/components/schemas/Inner" },
                        { "type": "null" }
                    ] }
                },
                "required": ["inner"]
            },
            "Inner": { "type": "object", "properties": { "x": { "type": "string" } } }
        })),
    )
    .generate_to_string()
    .unwrap();

    // Required-but-nullable: Option from the null arm, no untagged enum.
    assert!(
        out.contains("pub inner: ::std::option::Option<Inner>"),
        "anyOf [T, null] lowers to Option<T>:\n{out}",
    );
    assert!(!out.contains("untagged"), "no untagged enum:\n{out}");
}

#[test]
fn nullable_flag_wraps_in_option() {
    let out = generator_for(
        "nullable_flag",
        spec_with_schemas(serde_json::json!({
            "Thing": {
                "type": "object",
                "properties": {
                    "label": { "type": "string", "nullable": true }
                },
                "required": ["label"]
            }
        })),
    )
    .generate_to_string()
    .unwrap();
    assert!(
        out.contains("pub label: ::std::option::Option<::std::string::String>"),
        "nullable required field is Option:\n{out}",
    );
}

#[test]
fn reference_cycles_get_boxed() {
    let out = generator_for(
        "cycles",
        spec_with_schemas(serde_json::json!({
            "Node": {
                "type": "object",
                "properties": {
                    "next": { "$ref": "#/components/schemas/Node" },
                    "children": {
                        "type": "array",
                        "items": { "$ref": "#/components/schemas/Node" }
                    }
                }
            }
        })),
    )
    .generate_to_string()
    .unwrap();

    // The direct self-reference must be boxed; the Vec edge already has
    // indirection and must not be.
    assert!(
        out.contains("pub next: ::std::option::Option<::std::boxed::Box<Node>>"),
        "self-reference boxed:\n{out}",
    );
    assert!(
        out.contains("pub children: ::std::option::Option<::std::vec::Vec<Node>>"),
        "vec edge not boxed:\n{out}",
    );
    // Deep patch must not fire through the Box (matches the fork's
    // Option<Box<Struct>> handling — it does annotate through Box).
    assert!(out.contains("#[patch(name = \"Option<NodePatch>\")]"));
}

#[test]
fn mutual_cycle_both_edges_boxed() {
    let out = generator_for(
        "mutual_cycle",
        spec_with_schemas(serde_json::json!({
            "A": { "type": "object", "properties": { "b": { "$ref": "#/components/schemas/B" } } },
            "B": { "type": "object", "properties": { "a": { "$ref": "#/components/schemas/A" } } }
        })),
    )
    .generate_to_string()
    .unwrap();
    assert!(out.contains("pub b: ::std::option::Option<::std::boxed::Box<B>>"));
    assert!(out.contains("pub a: ::std::option::Option<::std::boxed::Box<A>>"));
}

#[test]
fn inline_object_properties_get_synthetic_names() {
    let out = generator_for(
        "inline",
        spec_with_schemas(serde_json::json!({
            "Outer": {
                "type": "object",
                "properties": {
                    "innerThing": {
                        "type": "object",
                        "properties": { "x": { "type": "string" } }
                    },
                    "choices": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "y": { "type": "string" } }
                        }
                    }
                }
            }
        })),
    )
    .generate_to_string()
    .unwrap();

    assert!(out.contains("pub struct OuterInnerThing"), "{out}");
    assert!(out.contains("pub struct OuterChoicesItem"), "{out}");
    assert!(out.contains("pub inner_thing: ::std::option::Option<OuterInnerThing>"));
    assert!(
        out.contains("pub choices: ::std::option::Option<::std::vec::Vec<OuterChoicesItem>>"),
    );
}

#[test]
fn name_collisions_are_loud_errors() {
    let error = generator_for(
        "collision",
        spec_with_schemas(serde_json::json!({
            "useCSL": { "type": "object", "properties": { "x": { "type": "string" } } },
            "useCsl": { "type": "object", "properties": { "y": { "type": "string" } } }
        })),
    )
    .generate_to_string()
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("UseCsl"),
        "collision error names the Rust name: {error}",
    );
}

#[test]
fn all_of_merge_fallback_unions_properties() {
    // A non-composable allOf (no $ref bases) merges properties.
    let out = generator_for(
        "merge",
        spec_with_schemas(serde_json::json!({
            "Merged": {
                "allOf": [
                    { "type": "object", "properties": { "a": { "type": "string" } },
                      "required": ["a"] },
                    { "type": "object", "properties": { "b": { "type": "integer", "format": "int32" } } }
                ]
            }
        })),
    )
    .generate_to_string()
    .unwrap();
    assert!(out.contains("pub a: ::std::string::String"), "{out}");
    assert!(out.contains("pub b: ::std::option::Option<i32>"), "{out}");
    assert!(!out.contains("flatten"), "merge, not compose:\n{out}");
}

#[test]
fn all_of_singleton_ref_vanishes_into_target() {
    let out = generator_for(
        "singleton",
        spec_with_schemas(serde_json::json!({
            "Base": { "type": "object", "properties": { "x": { "type": "string" } } },
            "Alias": { "allOf": [ { "$ref": "#/components/schemas/Base" } ] },
            "User": {
                "type": "object",
                "properties": { "field": { "$ref": "#/components/schemas/Alias" } }
            }
        })),
    )
    .generate_to_string()
    .unwrap();
    // The alias resolves to Base at the use site; no Alias item exists.
    assert!(out.contains("pub field: ::std::option::Option<Base>"), "{out}");
    assert!(!out.contains("pub struct Alias"), "{out}");
}

#[test]
fn named_scalar_schemas_inline_like_typify() {
    let out = generator_for(
        "scalars",
        spec_with_schemas(serde_json::json!({
            "PlainString": { "type": "string" },
            "Holder": {
                "type": "object",
                "properties": { "value": { "$ref": "#/components/schemas/PlainString" } },
                "required": ["value"]
            }
        })),
    )
    .generate_to_string()
    .unwrap();
    assert!(out.contains("pub value: ::std::string::String"), "{out}");
    assert!(!out.contains("PlainString"), "named scalars vanish:\n{out}");
}

#[test]
fn style_hook_and_type_overrides_apply() {
    let out = generator_for(
        "overrides",
        spec_with_schemas(serde_json::json!({
            "Simple": { "type": "object", "properties": { "x": { "type": "string" } } }
        })),
    )
    .style(|style| {
        style
            .types
            .entry("Simple".to_string())
            .or_default()
            .derives_add
            .push("Eq".to_string());
    })
    .generate_to_string()
    .unwrap();
    assert!(
        out.contains("#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Patch, Eq)]"),
        "per-type derive appended after the ordered list:\n{out}",
    );
}

#[test]
fn unmatched_override_selectors_error() {
    let error = format!(
        "{:#}",
        generator_for(
            "unmatched",
            spec_with_schemas(serde_json::json!({
                "Simple": { "type": "object", "properties": { "x": { "type": "string" } } }
            })),
        )
        .style(|style| {
            style
                .types
                .entry("Nonexistent".to_string())
                .or_default()
                .derives_add
                .push("Eq".to_string());
        })
        .generate_to_string()
        .unwrap_err()
    );
    assert!(error.contains("Nonexistent"), "{error}");
}

#[test]
fn codegen_toml_overrides_preset() {
    let toml = r#"
        profile = "api-client"

        [style]
        rename-all = "snake_case"
        deep-patch = "off"

        [types."Simple"]
        derives-add = ["Eq"]

        [fields."Simple.x"]
        type = "::my_crate::Special"
    "#;
    let config = StyleConfig::from_toml_str(toml, StyleConfig::api_client()).unwrap();
    assert_eq!(config.rename_all.as_deref(), Some("snake_case"));
    assert_eq!(config.deep_patch, openapi_codegen::config::DeepPatchMode::Off);

    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ir_unit");
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("codegen.toml");
    std::fs::write(&config_path, toml).unwrap();

    let out = generator_for(
        "toml_config",
        spec_with_schemas(serde_json::json!({
            "Simple": { "type": "object", "properties": { "x": { "type": "string" } } }
        })),
    )
    .config_file(config_path)
    .generate_to_string()
    .unwrap();
    assert!(out.contains("rename_all = \"snake_case\""), "{out}");
    assert!(out.contains("Eq)]"), "{out}");
    assert!(
        out.contains("pub x: ::std::option::Option<::my_crate::Special>"),
        "field type override keeps the Option wrapper:\n{out}",
    );
}

#[test]
fn unsupported_modes_error_loudly() {
    let error = generator_for(
        "unsupported",
        spec_with_schemas(serde_json::json!({
            "Simple": { "type": "object", "properties": { "x": { "type": "string" } } }
        })),
    )
    .style(|style| style.optional_fields = openapi_codegen::config::OptionalFields::Bare)
    .generate_to_string()
    .unwrap_err()
    .to_string();
    assert!(error.contains("optional-fields"), "{error}");
}

#[test]
fn custom_ir_pass_runs_after_builtins() {
    struct StripDocs;
    impl openapi_codegen::ir::passes::Pass for StripDocs {
        fn name(&self) -> &'static str {
            "strip-docs"
        }
        fn run(
            &self,
            ir: &mut openapi_codegen::ir::Ir,
            _cx: &openapi_codegen::ir::passes::PassCx<'_>,
        ) -> openapi_codegen::Result<()> {
            for def in &mut ir.types {
                def.description = Some("REDACTED".to_string());
            }
            Ok(())
        }
    }

    let out = generator_for(
        "custom_pass",
        spec_with_schemas(serde_json::json!({
            "Simple": {
                "type": "object",
                "description": "Something detailed.",
                "properties": { "x": { "type": "string" } }
            }
        })),
    )
    .ir_pass(StripDocs)
    .generate_to_string()
    .unwrap();
    assert!(out.contains("///REDACTED"), "{out}");
    assert!(!out.contains("Something detailed."), "{out}");
}

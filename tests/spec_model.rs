//! Step-1 guards: the typed spec model must be a drop-in replacement for
//! the historical `Value`-surgery lowering on the typify path.
//!
//! The final-Rust byte guarantee is covered by `tests/parity.rs` (the
//! pipeline now routes through `Spec`); these tests pin the intermediate
//! artifact itself — the draft-07 `definitions` document — so any drift is
//! caught at its source with a JSON-level diff instead of a rendered-Rust
//! diff.

use openapi_codegen::spec::Spec;
use serde_json::Value;

fn load(spec: &str, patches: Option<&str>) -> Value {
    let mut document = openapi_codegen::load_spec(std::path::Path::new(spec)).unwrap();
    if let Some(dir) = patches {
        openapi_codegen::apply_patches_dir(&mut document, std::path::Path::new(dir)).unwrap();
    }
    document
}

/// The historical lowering: in-place surgery, then extract
/// `/components/schemas`.
fn legacy_definitions(document: &Value) -> Value {
    let mut lowered = document.clone();
    openapi_codegen::lower_to_json_schema(&mut lowered);
    lowered.pointer("/components/schemas").cloned().unwrap()
}

/// First differing definition, for readable failures.
fn assert_definitions_equal(label: &str, legacy: &Value, modeled: &Value) {
    let legacy_map = legacy.as_object().unwrap();
    let modeled_map = modeled.as_object().unwrap();
    let legacy_keys: Vec<_> = legacy_map.keys().collect();
    let modeled_keys: Vec<_> = modeled_map.keys().collect();
    assert!(
        legacy_keys == modeled_keys,
        "{label}: definition key sets differ\n  legacy:  {legacy_keys:?}\n  modeled: {modeled_keys:?}",
    );
    for (name, legacy_schema) in legacy_map {
        let modeled_schema = &modeled_map[name];
        assert!(
            legacy_schema == modeled_schema,
            "{label}: definition {name} differs\n--- legacy ---\n{}\n--- modeled ---\n{}",
            serde_json::to_string_pretty(legacy_schema).unwrap(),
            serde_json::to_string_pretty(modeled_schema).unwrap(),
        );
    }
}

#[test]
fn petstore_spec_model_matches_legacy_lowering() {
    let document = load("specs/petstore.yaml", None);
    let legacy = legacy_definitions(&document);
    let spec = Spec::from_value(&document).unwrap();
    let modeled = Value::Object(spec.to_draft07_definitions());
    assert_definitions_equal("petstore", &legacy, &modeled);
}

#[test]
fn sabre_spec_model_matches_legacy_lowering() {
    let document = load(
        "specs/sabre-booking/spec.openapi.yaml",
        Some("specs/sabre-booking/patches"),
    );
    let legacy = legacy_definitions(&document);
    let spec = Spec::from_value(&document).unwrap();
    let modeled = Value::Object(spec.to_draft07_definitions());
    assert_definitions_equal("sabre", &legacy, &modeled);
}

#[test]
fn spec_model_preserves_what_lowering_strips() {
    // The model keeps discriminator and examples (decision D7) even
    // though the draft-07 render drops them.
    let document: Value = serde_json::from_str(
        r##"{
            "openapi": "3.0.0",
            "info": { "title": "t", "version": "1" },
            "paths": {},
            "components": { "schemas": {
                "Pet": {
                    "type": "object",
                    "discriminator": { "propertyName": "petType" },
                    "example": { "petType": "dog" },
                    "properties": {
                        "petType": { "type": "string", "example": "dog" }
                    }
                }
            } }
        }"##,
    )
    .unwrap();
    let spec = Spec::from_value(&document).unwrap();
    let pet = &spec.schemas["Pet"];
    assert!(pet.discriminator.is_some());
    assert_eq!(pet.examples.len(), 1);
    assert_eq!(pet.properties["petType"].examples.len(), 1);

    let rendered = pet.to_draft07();
    assert!(rendered.get("discriminator").is_none());
    assert!(rendered.get("example").is_none());
}

#[test]
fn spec_model_captures_operations() {
    let document = load("specs/petstore.yaml", None);
    let spec = Spec::from_value(&document).unwrap();
    assert_eq!(spec.operations.len(), 2);

    let create = spec
        .operations
        .iter()
        .find(|op| op.operation_id.as_deref() == Some("createPet"))
        .unwrap();
    assert_eq!(create.method, "post");
    assert_eq!(create.path, "/pets");
    assert_eq!(create.request.len(), 1);
    assert_eq!(
        create.request[0].schema.as_ref().unwrap().reference.as_deref(),
        Some("#/components/schemas/CreatePetRequest"),
    );

    let get = spec
        .operations
        .iter()
        .find(|op| op.operation_id.as_deref() == Some("getPet"))
        .unwrap();
    assert_eq!(get.params.len(), 1);
    assert_eq!(get.params[0].name, "petId");
    assert!(get.params[0].required);
}

#[test]
fn nullable_named_object_wraps_without_self_collision() {
    // A named schema with `nullable: true` lowers to the draft-07
    // type array `["object", "null"]`, under which typify emits
    // `X(Option<XInner>)`. The old `anyOf [T, null]` wrap named the
    // inner subschema after the definition itself — `X(Option<X>)` —
    // colliding with the real type (GitHub's `nullable-*` schema
    // family; docs/SPEC_COVERAGE.md).
    let document = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "auto-merge": {
                "title": "Auto merge",
                "type": "object", "nullable": true,
                "required": ["enabled_by"],
                "properties": { "enabled_by": { "type": "string" } }
            },
            "repo": {
                "type": "object",
                "properties": { "auto_merge": { "$ref": "#/components/schemas/auto-merge" } }
            }
        } }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spec_model");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nullable_named.json");
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let out = openapi_codegen::Generator::new(&path)
        .profile(openapi_codegen::StyleProfile::ApiClient)
        .generate_to_string()
        .unwrap();

    assert!(
        out.contains("pub struct AutoMerge(pub ::std::option::Option<AutoMergeInner>);"),
        "{out}",
    );
    assert!(out.contains("pub struct AutoMergeInner {"), "{out}");
    assert_eq!(
        out.matches("pub struct AutoMerge").count(),
        2, // the wrapper + the Inner — not two colliding `AutoMerge`s
        "{out}",
    );
}

#[test]
fn string_enum_with_mistyped_scalar_members_stringifies() {
    // Plaid-class YAML artifact: `type: string` enums whose members
    // parsed as booleans/numbers (`- true`). The declared type is the
    // author's intent; the members stringify instead of failing
    // generation, and a nullable enum still folds into Option.
    let document = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "RetirementIndicator": {
                "type": "string", "nullable": true,
                "enum": [true, false]
            },
            "Holder": {
                "type": "object",
                "required": ["indicator"],
                "properties": {
                    "indicator": { "$ref": "#/components/schemas/RetirementIndicator" }
                }
            }
        } }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spec_model");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mistyped_enum.json");
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let out = openapi_codegen::Generator::new(&path)
        .profile(openapi_codegen::StyleProfile::ApiClient)
        .generate_to_string()
        .unwrap();

    assert!(out.contains("#[serde(rename = \"true\")]"), "{out}");
    assert!(out.contains("#[serde(rename = \"false\")]"), "{out}");
    assert!(
        out.contains("pub indicator: ::std::option::Option<RetirementIndicator>")
            || out.contains("pub struct RetirementIndicator(pub ::std::option::Option<"),
        "nullable enum still folds into Option: {out}",
    );
}

#[test]
fn nullable_allof_wrapper_hoists_inner_definition() {
    // Plaid-class: a named untyped `nullable: true` allOf composition
    // renders as `anyOf [inner, null]`; typify hands the definition's
    // name to the inner subschema, colliding with the Option's newtype
    // wrapper (`X(Option<X>)`, E0428). The render hoists the inner into
    // a `{name}Inner` definition instead.
    let document = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "AccessNullable": {
                "nullable": true,
                "description": "Allowed products.",
                "allOf": [
                    { "$ref": "#/components/schemas/Access" },
                    { "type": "object", "additionalProperties": true }
                ]
            },
            "Access": {
                "type": "object",
                "properties": { "account_data": { "type": "string" } }
            },
            "Holder": {
                "type": "object",
                "properties": {
                    "access": { "$ref": "#/components/schemas/AccessNullable" }
                }
            }
        } }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spec_model");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nullable_allof.json");
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let out = openapi_codegen::Generator::new(&path)
        .profile(openapi_codegen::StyleProfile::ApiClient)
        .generate_to_string()
        .unwrap();

    assert!(
        out.contains("pub struct AccessNullable(pub ::std::option::Option<AccessNullableInner>);"),
        "{out}",
    );
    assert!(out.contains("pub struct AccessNullableInner {"), "{out}");
}

#[test]
fn null_member_string_enum_hoists_inner_definition() {
    // Plaid-class: a named `type: string` enum with a literal `null`
    // member (no `nullable: true`) prunes the null into an Option whose
    // wrapper takes the definition's name; the variant enum needs a
    // distinct one. The render hoists the null-free enum into
    // `{name}Inner`.
    let document = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "PayFrequencyValue": {
                "type": "string",
                "title": "PayFrequencyValue",
                "description": "The frequency of the pay period.",
                "enum": ["monthly", "weekly", null]
            },
            "Holder": {
                "type": "object",
                "properties": {
                    "frequency": { "$ref": "#/components/schemas/PayFrequencyValue" }
                }
            }
        } }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spec_model");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("null_member_enum.json");
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let out = openapi_codegen::Generator::new(&path)
        .profile(openapi_codegen::StyleProfile::ApiClient)
        .generate_to_string()
        .unwrap();

    assert!(
        out.contains(
            "pub struct PayFrequencyValue(pub ::std::option::Option<PayFrequencyValueInner>);"
        ),
        "{out}",
    );
    assert!(out.contains("pub enum PayFrequencyValueInner"), "{out}");
    assert!(out.contains("Monthly"), "{out}");
}

#[test]
fn null_default_on_non_nullable_node_is_dropped() {
    // DigitalOcean-class: `default: null` on a plain oneOf union means
    // "no default" (null is not a value of the type); typify would
    // reject or panic on it. Nullable nodes' `default: null` is the
    // Option's intrinsic default — identical to no default — and is
    // omitted too, so it can't leak onto the inner type of typify's
    // Option conversion.
    let document = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "Holder": {
                "type": "object",
                "properties": {
                    "stop": {
                        "default": null,
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ]
                    }
                }
            }
        } }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spec_model");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("null_default.json");
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let out = openapi_codegen::Generator::new(&path)
        .profile(openapi_codegen::StyleProfile::ApiClient)
        .generate_to_string()
        .unwrap();
    assert!(out.contains("pub enum HolderStop"), "{out}");
}

#[test]
fn spec_model_rejects_unsupported_schema_keywords() {
    // Unmodeled schema-bearing keywords are loud errors, not silent
    // passthrough: they would need $ref rewriting inside them.
    let document: Value = serde_json::from_str(
        r##"{
            "openapi": "3.0.0",
            "info": { "title": "t", "version": "1" },
            "paths": {},
            "components": { "schemas": {
                "Weird": { "type": "object", "patternProperties": { "^x": { "type": "string" } } }
            } }
        }"##,
    )
    .unwrap();
    let error = Spec::from_value(&document).unwrap_err().to_string();
    assert!(error.contains("Weird"), "error should name the schema: {error}");
}

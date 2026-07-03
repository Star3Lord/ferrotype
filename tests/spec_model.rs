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

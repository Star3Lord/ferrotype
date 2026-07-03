//! OpenAPI 3.x → JSON Schema (draft-07) lowering — the legacy door.
//!
//! typify consumes JSON Schema, not OpenAPI. The differences that matter for
//! type generation are small and mechanical, so we rewrite them in place on
//! the parsed `serde_json::Value` tree rather than pulling in a full OpenAPI
//! object model.
//!
//! Since migration step 1 (docs/MIGRATION.md) the generate pipeline no
//! longer uses this module: it normalizes into the typed [`crate::spec`]
//! model and renders draft-07 from there, byte-identically (guarded by
//! `tests/spec_model.rs`). This module remains public and backs the `lower`
//! CLI subcommand — the documented door for feeding *upstream* typify
//! (`cargo typify`, `typify::import_types!`) without the rest of the
//! pipeline.

use anyhow::Context;
use schemars::schema::RootSchema;
use serde_json::Value;

use crate::Result;

/// Lower `spec` (mutated in place) and return the
/// `{"definitions": {...}}` root schema built from `components/schemas`.
///
/// The transformations, applied recursively:
///
/// 1. `$ref: "#/components/schemas/X"` → `"#/definitions/X"`.
/// 2. `{..., nullable: true}` → `{anyOf: [{...}, {type: "null"}]}` so
///    typify wraps the resulting type in `Option`.
/// 3. Schemas carrying `format` without `type` get the type inferred
///    (`int32`/`int64` → integer, `float`/`double` → number, else string);
///    OpenAPI tooling tolerates the omission but typify expects a concrete
///    primitive schema.
/// 4. OpenAPI 3.0 boolean `exclusiveMinimum` / `exclusiveMaximum` flags are
///    rewritten to draft-07's numeric form.
/// 5. OpenAPI-only metadata that a strict draft-07 parser rejects
///    (`example`, `examples`, `xml`, `externalDocs`, `discriminator`) is
///    stripped.
pub fn lowered_root_schema(spec: &mut Value) -> Result<RootSchema> {
    lower_to_json_schema(spec);

    let schemas = spec
        .pointer("/components/schemas")
        .cloned()
        .context("OpenAPI spec is missing /components/schemas")?;

    let root: RootSchema = serde_json::from_value(serde_json::json!({
        "definitions": schemas,
    }))
    .context("failed to deserialize lowered schemas as a JSON Schema root")?;
    Ok(root)
}

/// Apply the lowering rewrites described on [`lowered_root_schema`] to the
/// whole document in place, without extracting the root schema.
pub fn lower_to_json_schema(value: &mut Value) {
    strip_unsupported_metadata(value);
    lower_node(value);
}

fn lower_node(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get_mut("$ref")
                && let Some(rest) = reference.strip_prefix("#/components/schemas/")
            {
                *reference = format!("#/definitions/{rest}");
            }
            infer_missing_type_from_format(map);
            normalize_exclusive_bound(map, "minimum", "exclusiveMinimum");
            normalize_exclusive_bound(map, "maximum", "exclusiveMaximum");

            let nullable = matches!(map.remove("nullable"), Some(Value::Bool(true)));
            for child in map.values_mut() {
                lower_node(child);
            }
            if nullable && !map.is_empty() {
                let inner = std::mem::take(map);
                map.insert(
                    "anyOf".to_string(),
                    Value::Array(vec![
                        Value::Object(inner),
                        serde_json::json!({ "type": "null" }),
                    ]),
                );
            }
        }
        Value::Array(items) => {
            for child in items {
                lower_node(child);
            }
        }
        _ => {}
    }
}

/// Some published schemas carry `format` without `type`; typify expects a
/// concrete primitive schema.
fn infer_missing_type_from_format(map: &mut serde_json::Map<String, Value>) {
    if map.contains_key("type") {
        return;
    }
    let Some(Value::String(format)) = map.get("format") else {
        return;
    };
    let inferred = match format.as_str() {
        "int32" | "int64" => "integer",
        "float" | "double" => "number",
        _ => "string",
    };
    map.insert("type".to_string(), Value::String(inferred.to_string()));
}

/// Convert OpenAPI 3.0 boolean exclusivity flags to JSON Schema draft-07's
/// numeric `exclusiveMinimum` / `exclusiveMaximum` form.
fn normalize_exclusive_bound(
    map: &mut serde_json::Map<String, Value>,
    bound_key: &str,
    exclusive_key: &str,
) {
    let Some(Value::Bool(exclusive)) = map.remove(exclusive_key) else {
        return;
    };
    if !exclusive {
        return;
    }
    let Some(bound @ Value::Number(_)) = map.remove(bound_key) else {
        return;
    };
    map.insert(exclusive_key.to_string(), bound);
}

/// Remove schema metadata that the OpenAPI dialect permits but that a strict
/// JSON-Schema draft-07 parser rejects with `unknown field`.
fn strip_unsupported_metadata(value: &mut Value) {
    const DROP_KEYS: &[&str] = &["example", "examples", "xml", "externalDocs", "discriminator"];
    match value {
        Value::Object(map) => {
            for key in DROP_KEYS {
                map.remove(*key);
            }
            for child in map.values_mut() {
                strip_unsupported_metadata(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                strip_unsupported_metadata(child);
            }
        }
        _ => {}
    }
}

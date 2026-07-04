//! The typed schema node: parse from OpenAPI, render to draft-07.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use super::Origin;
use crate::Result;

/// The `type` keyword, when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeHint {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    Array,
    Null,
}

impl TypeHint {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "string" => TypeHint::String,
            "integer" => TypeHint::Integer,
            "number" => TypeHint::Number,
            "boolean" => TypeHint::Boolean,
            "object" => TypeHint::Object,
            "array" => TypeHint::Array,
            "null" => TypeHint::Null,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TypeHint::String => "string",
            TypeHint::Integer => "integer",
            TypeHint::Number => "number",
            TypeHint::Boolean => "boolean",
            TypeHint::Object => "object",
            TypeHint::Array => "array",
            TypeHint::Null => "null",
        }
    }
}

/// `additionalProperties`: draft-07 allows a boolean or a schema.
#[derive(Debug, Clone)]
pub enum AdditionalProperties {
    Allowed(bool),
    Schema(Box<Schema>),
}

/// A numeric bound that OpenAPI 3.0 spells as a boolean flag
/// (`exclusiveMinimum: true` next to `minimum`) and draft-07 / 3.1 spell
/// as a number.
#[derive(Debug, Clone)]
pub enum BoolOrNumber {
    Bool(bool),
    Number(serde_json::Number),
}

/// One schema node, typed. Keywords are orthogonal fields (mirroring JSON
/// Schema's real shape — `$ref` with annotation siblings is legal and
/// occurs in the wild) rather than an exclusive enum. Anything the model
/// does not understand lands in [`Self::extra`] verbatim, **except**
/// schema-bearing keywords we don't model, which are hard errors: they
/// would need `$ref` rewriting inside them, so passing them through
/// silently would corrupt output.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    /// The verbatim `$ref` string.
    pub reference: Option<String>,
    pub ty: Option<TypeHint>,
    pub format: Option<String>,
    pub description: Option<String>,
    pub title: Option<String>,
    /// Object properties, sorted by wire name (the whole pipeline is
    /// BTreeMap-ordered; see the parity contract).
    pub properties: BTreeMap<String, Schema>,
    /// The `required` list, in spec order.
    pub required: Vec<String>,
    pub additional_properties: Option<AdditionalProperties>,
    /// Array `items`. The draft-07 tuple form (`items: [..]`) is not
    /// modeled and errors at parse.
    pub items: Option<Box<Schema>>,
    /// `enum` values, in spec order.
    pub enumeration: Vec<Value>,
    pub default: Option<Value>,
    pub all_of: Vec<Schema>,
    pub one_of: Vec<Schema>,
    pub any_of: Vec<Schema>,
    /// OpenAPI 3.0 `nullable: true`, normalized here; 3.1's
    /// `type: [T, "null"]` also folds into this flag.
    pub nullable: bool,
    /// Preserved for the future oneOf mapping / client generator; not
    /// rendered to draft-07 (decision D7).
    pub discriminator: Option<Value>,
    /// `example` (OpenAPI) and `examples` (3.1/JSON Schema) values,
    /// preserved in the model, not rendered to draft-07 (decision D7).
    pub examples: Vec<Value>,
    // ── validation keywords, passed through faithfully ──
    pub pattern: Option<String>,
    pub min_length: Option<serde_json::Number>,
    pub max_length: Option<serde_json::Number>,
    pub minimum: Option<serde_json::Number>,
    pub maximum: Option<serde_json::Number>,
    pub exclusive_minimum: Option<BoolOrNumber>,
    pub exclusive_maximum: Option<BoolOrNumber>,
    pub multiple_of: Option<serde_json::Number>,
    pub min_items: Option<serde_json::Number>,
    pub max_items: Option<serde_json::Number>,
    pub unique_items: Option<bool>,
    pub min_properties: Option<serde_json::Number>,
    pub max_properties: Option<serde_json::Number>,
    /// Unknown, non-schema-bearing keywords (vendor extensions,
    /// `deprecated`, `readOnly`, …), passed through to the render
    /// verbatim.
    pub extra: BTreeMap<String, Value>,
    /// Where this node lives in the patched document.
    pub origin: Origin,
}

/// Keywords that contain subschemas but are not modeled. Passing them
/// through `extra` would skip `$ref` rewriting inside them, so they fail
/// loudly instead. The documented workaround is the patch mechanism.
const UNSUPPORTED_SCHEMA_KEYWORDS: &[&str] = &[
    "patternProperties",
    "propertyNames",
    "if",
    "then",
    "else",
    "not",
    "contains",
    "dependencies",
    "definitions",
    "additionalItems",
];

/// Keys stripped by the historical lowering and *not* preserved as typed
/// model fields (unlike `discriminator` and examples).
const DROPPED_KEYS: &[&str] = &["xml", "externalDocs"];

impl Schema {
    /// Parse one schema node.
    pub(crate) fn from_value(raw: &Value, origin: Origin) -> Result<Self> {
        let map = match raw {
            Value::Object(map) => map,
            Value::Bool(_) => bail!(
                "boolean schemas are not supported at {origin} \
                 (patch the spec to an explicit object schema)",
            ),
            other => bail!("schema at {origin} is not an object: {other}"),
        };

        let mut schema = Schema {
            origin: origin.clone(),
            ..Schema::default()
        };

        for (key, value) in map {
            let child = origin.child(key);
            match key.as_str() {
                "$ref" => {
                    schema.reference = Some(
                        value
                            .as_str()
                            .with_context(|| format!("$ref at {child} is not a string"))?
                            .to_string(),
                    );
                }
                "type" => schema.parse_type(value, &child)?,
                "format" => {
                    schema.format = Some(
                        value
                            .as_str()
                            .with_context(|| format!("format at {child} is not a string"))?
                            .to_string(),
                    );
                }
                "description" => {
                    schema.description = Some(
                        value
                            .as_str()
                            .with_context(|| format!("description at {child} is not a string"))?
                            .to_string(),
                    );
                }
                "title" => {
                    schema.title = Some(
                        value
                            .as_str()
                            .with_context(|| format!("title at {child} is not a string"))?
                            .to_string(),
                    );
                }
                "properties" => {
                    let properties = value
                        .as_object()
                        .with_context(|| format!("properties at {child} is not an object"))?;
                    for (name, property) in properties {
                        let property_origin = child.child(name);
                        schema.properties.insert(
                            name.clone(),
                            Schema::from_value(property, property_origin)?,
                        );
                    }
                }
                "required" => {
                    let entries = value
                        .as_array()
                        .with_context(|| format!("required at {child} is not an array"))?;
                    for entry in entries {
                        schema.required.push(
                            entry
                                .as_str()
                                .with_context(|| {
                                    format!("required entry at {child} is not a string")
                                })?
                                .to_string(),
                        );
                    }
                }
                "additionalProperties" => {
                    schema.additional_properties = Some(match value {
                        Value::Bool(allowed) => AdditionalProperties::Allowed(*allowed),
                        other => AdditionalProperties::Schema(Box::new(Schema::from_value(
                            other, child,
                        )?)),
                    });
                }
                "items" => {
                    if value.is_array() {
                        bail!(
                            "tuple-form `items` arrays are not supported at {child} \
                             (patch the spec)",
                        );
                    }
                    schema.items = Some(Box::new(Schema::from_value(value, child)?));
                }
                "enum" => {
                    schema.enumeration = value
                        .as_array()
                        .with_context(|| format!("enum at {child} is not an array"))?
                        .clone();
                }
                "default" => schema.default = Some(value.clone()),
                "allOf" | "oneOf" | "anyOf" => {
                    let subschemas = value
                        .as_array()
                        .with_context(|| format!("{key} at {child} is not an array"))?;
                    let mut parsed = Vec::with_capacity(subschemas.len());
                    for (index, subschema) in subschemas.iter().enumerate() {
                        parsed.push(Schema::from_value(subschema, child.index(index))?);
                    }
                    match key.as_str() {
                        "allOf" => schema.all_of = parsed,
                        "oneOf" => schema.one_of = parsed,
                        _ => schema.any_of = parsed,
                    }
                }
                "nullable" => {
                    schema.nullable = value.as_bool().unwrap_or(false);
                }
                "discriminator" => schema.discriminator = Some(value.clone()),
                "example" => schema.examples.push(value.clone()),
                "examples" => match value {
                    Value::Array(entries) => schema.examples.extend(entries.iter().cloned()),
                    other => schema.examples.push(other.clone()),
                },
                "pattern" => {
                    schema.pattern = Some(
                        value
                            .as_str()
                            .with_context(|| format!("pattern at {child} is not a string"))?
                            .to_string(),
                    );
                }
                "minLength" => schema.min_length = Some(number_at(value, &child)?),
                "maxLength" => schema.max_length = Some(number_at(value, &child)?),
                "minimum" => schema.minimum = Some(number_at(value, &child)?),
                "maximum" => schema.maximum = Some(number_at(value, &child)?),
                "exclusiveMinimum" => {
                    schema.exclusive_minimum = Some(bool_or_number(value, &child)?)
                }
                "exclusiveMaximum" => {
                    schema.exclusive_maximum = Some(bool_or_number(value, &child)?)
                }
                "multipleOf" => schema.multiple_of = Some(number_at(value, &child)?),
                "minItems" => schema.min_items = Some(number_at(value, &child)?),
                "maxItems" => schema.max_items = Some(number_at(value, &child)?),
                "uniqueItems" => {
                    schema.unique_items = Some(value.as_bool().with_context(|| {
                        format!("uniqueItems at {child} is not a boolean")
                    })?);
                }
                "minProperties" => schema.min_properties = Some(number_at(value, &child)?),
                "maxProperties" => schema.max_properties = Some(number_at(value, &child)?),
                key if UNSUPPORTED_SCHEMA_KEYWORDS.contains(&key) => {
                    bail!(
                        "schema keyword `{key}` at {child} is not supported by the spec \
                         model (patch the spec, or extend the model)",
                    );
                }
                key if DROPPED_KEYS.contains(&key) => {
                    // Dropped by normalization, matching the historical
                    // lowering (decision D7).
                }
                _ => {
                    schema.extra.insert(key.clone(), value.clone());
                }
            }
        }

        Ok(schema)
    }

    /// Handle `type`, including the 3.1 array form: `[T, "null"]` folds
    /// into `ty: T` + `nullable`.
    fn parse_type(&mut self, value: &Value, origin: &Origin) -> Result<()> {
        match value {
            Value::String(raw) => {
                self.ty = Some(
                    TypeHint::parse(raw)
                        .with_context(|| format!("unknown type {raw:?} at {origin}"))?,
                );
            }
            Value::Array(entries) => {
                // 3.1 dialect. Exactly one non-null type plus an optional
                // "null" is representable; anything richer is a union we
                // don't model yet.
                let mut non_null = Vec::new();
                for entry in entries {
                    let raw = entry
                        .as_str()
                        .with_context(|| format!("type array entry at {origin} is not a string"))?;
                    if raw == "null" {
                        self.nullable = true;
                    } else {
                        non_null.push(raw);
                    }
                }
                match non_null.as_slice() {
                    [] => self.ty = Some(TypeHint::Null),
                    [single] => {
                        self.ty = Some(TypeHint::parse(single).with_context(|| {
                            format!("unknown type {single:?} at {origin}")
                        })?);
                    }
                    several => bail!(
                        "union type {several:?} at {origin} is not supported \
                         (patch the spec into oneOf)",
                    ),
                }
            }
            other => bail!("type at {origin} is not a string or array: {other}"),
        }
        Ok(())
    }

    /// Render this node as draft-07 JSON for the typify bridge. See
    /// [`super::Spec::to_draft07_definitions`] for the contract.
    pub fn to_draft07(&self) -> Value {
        let mut map = Map::new();

        if let Some(reference) = &self.reference {
            let rewritten = reference
                .strip_prefix("#/components/schemas/")
                .map(|rest| format!("#/definitions/{rest}"))
                .unwrap_or_else(|| reference.clone());
            map.insert("$ref".to_string(), Value::String(rewritten));
        }

        // `format` implies `type` when the spec omitted it — port of the
        // historical inference. A nullable *typed* node renders as the
        // draft-07 type array `[T, "null"]`: typify's native null form,
        // under which a named nullable definition becomes
        // `X(Option<XInner>)` — the `anyOf [T, null]` wrap (still used
        // for untyped nullables below) names the inner subschema after
        // the definition itself and collides (`X(Option<X>)`;
        // real-world corpus: GitHub's 50+ `nullable-*` schemas, see
        // docs/SPEC_COVERAGE.md).
        let render_type = |ty: &str| {
            if self.nullable {
                serde_json::json!([ty, "null"])
            } else {
                Value::String(ty.to_string())
            }
        };
        match (&self.ty, &self.format) {
            (Some(ty), _) => {
                map.insert("type".to_string(), render_type(ty.as_str()));
            }
            (None, Some(format)) => {
                let inferred = match format.as_str() {
                    "int32" | "int64" => "integer",
                    "float" | "double" => "number",
                    _ => "string",
                };
                map.insert("type".to_string(), render_type(inferred));
            }
            (None, None) => {}
        }
        if let Some(format) = &self.format {
            map.insert("format".to_string(), Value::String(format.clone()));
        }
        if let Some(description) = &self.description {
            map.insert(
                "description".to_string(),
                Value::String(description.clone()),
            );
        }
        // A nullable typed node renders as `type: [T, "null"]` (above),
        // and typify names the Option wrapper's INNER type from the
        // node's `title` when one is present — GitHub titles every
        // schema with its own name, so the inner would collide with
        // the wrapper (`X(Option<X>)`). Withhold the title on exactly
        // that shape; the inner then falls back to typify's
        // collision-free `{Wrapper}Inner` naming. The description
        // (the useful doc text) still renders.
        if let Some(title) = &self.title
            && !(self.nullable && self.ty.is_some())
        {
            map.insert("title".to_string(), Value::String(title.clone()));
        }

        if !self.properties.is_empty() {
            let properties: Map<String, Value> = self
                .properties
                .iter()
                .map(|(name, property)| (name.clone(), property.to_draft07()))
                .collect();
            map.insert("properties".to_string(), Value::Object(properties));
        }
        if !self.required.is_empty() {
            map.insert(
                "required".to_string(),
                Value::Array(
                    self.required
                        .iter()
                        .map(|name| Value::String(name.clone()))
                        .collect(),
                ),
            );
        }
        if let Some(additional) = &self.additional_properties {
            let rendered = match additional {
                AdditionalProperties::Allowed(allowed) => Value::Bool(*allowed),
                AdditionalProperties::Schema(schema) => schema.to_draft07(),
            };
            map.insert("additionalProperties".to_string(), rendered);
        }
        if let Some(items) = &self.items {
            map.insert("items".to_string(), items.to_draft07());
        }
        if !self.enumeration.is_empty() {
            // A `type: string` enum whose members are booleans or
            // numbers is a YAML-authoring artifact (`- true` parses as
            // a boolean; Plaid declares dozens of string enums this
            // way). The declared type is the author's intent and what
            // the wire carries: stringify the mistyped scalars. A
            // nullable enum keeps its `null` member so typify prunes
            // it into the Option wrapper.
            let members = self.enumeration.iter().map(|member| {
                match (&self.ty, member) {
                    (Some(TypeHint::String), Value::Bool(flag)) => {
                        Value::String(flag.to_string())
                    }
                    (Some(TypeHint::String), Value::Number(number)) => {
                        Value::String(number.to_string())
                    }
                    _ => member.clone(),
                }
            });
            let mut members: Vec<Value> = members.collect();
            // A nullable typed enum renders `type: [T, "null"]`; the
            // values list must offer the null typify folds into
            // `Option`.
            if self.nullable && self.ty.is_some() && !members.iter().any(Value::is_null) {
                members.push(Value::Null);
            }
            map.insert("enum".to_string(), Value::Array(members));
        }
        if let Some(default) = &self.default {
            // `default: null` on a non-nullable node is the JSON-ism
            // for "no default" (DigitalOcean writes it on plain oneOf
            // unions): null is not a value of the type, and typify
            // either rejects the default or panics rendering it. A
            // nullable node keeps it — null is the Option's intrinsic
            // default there.
            if !default.is_null() || self.nullable {
                map.insert("default".to_string(), default.clone());
            }
        }
        for (key, subschemas) in [
            ("allOf", &self.all_of),
            ("oneOf", &self.one_of),
            ("anyOf", &self.any_of),
        ] {
            if !subschemas.is_empty() {
                map.insert(
                    key.to_string(),
                    Value::Array(subschemas.iter().map(Schema::to_draft07).collect()),
                );
            }
        }

        if let Some(pattern) = &self.pattern {
            map.insert("pattern".to_string(), Value::String(pattern.clone()));
        }
        for (key, value) in [
            ("minLength", &self.min_length),
            ("maxLength", &self.max_length),
            ("multipleOf", &self.multiple_of),
            ("minItems", &self.min_items),
            ("maxItems", &self.max_items),
            ("minProperties", &self.min_properties),
            ("maxProperties", &self.max_properties),
        ] {
            if let Some(number) = value {
                map.insert(key.to_string(), Value::Number(number.clone()));
            }
        }
        if let Some(unique) = self.unique_items {
            map.insert("uniqueItems".to_string(), Value::Bool(unique));
        }

        self.render_bound(
            &mut map,
            "minimum",
            "exclusiveMinimum",
            &self.minimum,
            &self.exclusive_minimum,
        );
        self.render_bound(
            &mut map,
            "maximum",
            "exclusiveMaximum",
            &self.maximum,
            &self.exclusive_maximum,
        );

        for (key, value) in &self.extra {
            map.insert(key.clone(), value.clone());
        }

        // A nullable `oneOf`/`anyOf` gains a `{type: null}` member
        // instead of an outer wrap: typify folds it into a unit `Null`
        // variant (or an Option), whereas the wrap would name the inner
        // union after the definition itself and collide — GitHub's
        // `nullable-secret-scanning-first-detected-location` is a
        // nullable oneOf.
        if self.nullable && self.ty.is_none() {
            for key in ["oneOf", "anyOf"] {
                if let Some(Value::Array(members)) = map.get_mut(key) {
                    if !members.iter().any(|member| {
                        member.get("type").and_then(Value::as_str) == Some("null")
                    }) {
                        members.push(serde_json::json!({ "type": "null" }));
                    }
                    return Value::Object(map);
                }
            }
        }

        // Untyped `nullable: true` nodes ($ref siblings, allOf) render
        // as `anyOf [T, null]` so typify wraps the type in `Option` —
        // unless the node is otherwise empty (matching the historical
        // lowering). Typed nodes rendered `type: [T, "null"]` above.
        if self.nullable && self.ty.is_none() && !map.is_empty() {
            let inner = std::mem::take(&mut map);
            map.insert(
                "anyOf".to_string(),
                Value::Array(vec![
                    Value::Object(inner),
                    serde_json::json!({ "type": "null" }),
                ]),
            );
        }

        Value::Object(map)
    }

    /// Render a `minimum`/`maximum` + exclusivity pair in draft-07's
    /// numeric form. OpenAPI 3.0's `exclusive*: true` flag converts the
    /// bound; numeric exclusive bounds (draft-07 / 3.1 dialect) pass
    /// through as-is — the historical lowering silently *deleted* those,
    /// which was a bug (decision D10 in docs/MIGRATION.md).
    fn render_bound(
        &self,
        map: &mut Map<String, Value>,
        bound_key: &str,
        exclusive_key: &str,
        bound: &Option<serde_json::Number>,
        exclusive: &Option<BoolOrNumber>,
    ) {
        match (bound, exclusive) {
            (Some(bound), Some(BoolOrNumber::Bool(true))) => {
                map.insert(exclusive_key.to_string(), Value::Number(bound.clone()));
            }
            (bound, exclusive) => {
                if let Some(bound) = bound {
                    map.insert(bound_key.to_string(), Value::Number(bound.clone()));
                }
                match exclusive {
                    Some(BoolOrNumber::Number(number)) => {
                        map.insert(exclusive_key.to_string(), Value::Number(number.clone()));
                    }
                    // `exclusive*: false` (or `true` without a bound)
                    // renders nothing, matching the historical lowering.
                    Some(BoolOrNumber::Bool(_)) | None => {}
                }
            }
        }
    }
}

fn number_at(value: &Value, origin: &Origin) -> Result<serde_json::Number> {
    match value {
        Value::Number(number) => Ok(number.clone()),
        other => bail!("expected a number at {origin}, found {other}"),
    }
}

fn bool_or_number(value: &Value, origin: &Origin) -> Result<BoolOrNumber> {
    match value {
        Value::Bool(flag) => Ok(BoolOrNumber::Bool(*flag)),
        Value::Number(number) => Ok(BoolOrNumber::Number(number.clone())),
        other => bail!("expected a boolean or number at {origin}, found {other}"),
    }
}

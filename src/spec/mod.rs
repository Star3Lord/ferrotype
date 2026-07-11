//! The owned, typed spec model (migration step 1; see docs/MIGRATION.md).
//!
//! [`Spec::from_value`] normalizes a parsed-and-patched OpenAPI document
//! (Swagger-2.0-converted and 3.0.x today, with a seam for 3.1) into a
//! typed model. Everything downstream of loading consumes this model:
//!
//! - the typify engine renders it back down to a draft-07
//!   [`RootSchema`](schemars::schema::RootSchema) via
//!   [`Spec::to_draft07_root`] — byte-identical to the historical in-place
//!   `Value` lowering (guarded by `tests/spec_model.rs`);
//! - future consumers (operations/client generation) read it directly.
//!
//! Unlike the old string surgery, normalization is *structural*: `$ref`
//! rewrites and metadata stripping happen by field, not by key name, so a
//! property that happens to be called `nullable` or `format` can never be
//! mangled. Information the old lowering destroyed — `discriminator`,
//! `example`/`examples` — is preserved in the model (dropped only in the
//! draft-07 render, which feeds a strict parser). Constructs the model
//! cannot represent are **loud errors** carrying the schema's [`Origin`],
//! never silent drops: the byte-parity guarantee is only meaningful if
//! nothing unrepresentable slips through.

mod operations;
mod schema;

pub use operations::{Body, OperationSpec, Param, ParamLocation, ResponseSpec};
pub use schema::{AdditionalProperties, BoolOrNumber, Schema, TypeHint};

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde_json::Value;

use crate::Result;

/// A JSON pointer into the *patched* OpenAPI document: the addressing
/// spine for overrides, diagnostics, and provenance. The default is the
/// document root.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin(pub String);

impl Origin {
    pub(crate) fn root() -> Self {
        Origin(String::new())
    }

    /// The origin of `key` under `self`, with RFC 6901 token escaping.
    pub(crate) fn child(&self, key: &str) -> Self {
        let token = key.replace('~', "~0").replace('/', "~1");
        Origin(format!("{}/{}", self.0, token))
    }

    pub(crate) fn index(&self, index: usize) -> Self {
        Origin(format!("{}/{}", self.0, index))
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("#")
        } else {
            write!(f, "#{}", self.0)
        }
    }
}

/// The non-null inner definition to hoist out of a rendered nullable
/// schema, for the two shapes where typify would otherwise name the
/// inner after the definition itself (see
/// [`Spec::to_draft07_definitions`]); `None` when the rendered schema is
/// fine as-is. The second tuple element is a description for the
/// wrapper, for the shape whose source node carried its metadata at the
/// level the wrapper replaces.
fn hoistable_nullable_inner(schema: &Schema, rendered: &Value) -> Option<(Value, Option<Value>)> {
    // Shape 1: the untyped `nullable: true` wrap,
    // `{"anyOf": [inner, {"type": "null"}]}` with nothing else on the
    // node (metadata lives inside the inner, where it stays). A pure
    // `$ref` inner generates no named type and stays put.
    if schema.nullable && schema.ty.is_none() {
        let map = rendered.as_object()?;
        if map.len() != 1 {
            return None;
        }
        let members = map.get("anyOf")?.as_array()?;
        let [inner, null_member] = members.as_slice() else {
            return None;
        };
        if null_member.get("type").and_then(Value::as_str) != Some("null") {
            return None;
        }
        let inner_map = inner.as_object()?;
        let pure_ref = inner_map.len() == 1 && inner_map.contains_key("$ref");
        return (!pure_ref).then(|| (inner.clone(), None));
    }

    // Shape 2: a single-typed string enum with a literal `null` member
    // (rendered with `type: "string"`, not a type array): typify prunes
    // the null into an Option and needs a distinct name for the variant
    // enum. The hoisted inner drops the null member; the wrap supplies
    // the nullability, and inherits the node's description so the
    // wrapper newtype keeps its doc. (Nullable *typed* enums render as
    // `type: [T, "null"]` arrays, which typify already inner-names.)
    let map = rendered.as_object()?;
    if map.get("type").and_then(Value::as_str) != Some("string") {
        return None;
    }
    let members = map.get("enum")?.as_array()?;
    if !members.iter().any(Value::is_null) {
        return None;
    }
    let mut inner = map.clone();
    inner.insert(
        "enum".to_string(),
        Value::Array(members.iter().filter(|m| !m.is_null()).cloned().collect()),
    );
    Some((Value::Object(inner), map.get("description").cloned()))
}

/// Document-level metadata.
#[derive(Debug, Clone, Default)]
pub struct SpecMeta {
    /// `info.title`.
    pub title: Option<String>,
    /// `info.version`.
    pub version: Option<String>,
    /// The `openapi` (or `swagger`) version string, e.g. `3.0.0`.
    pub openapi: Option<String>,
}

/// The owned, version-normalized view of an OpenAPI document.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    pub meta: SpecMeta,
    /// `components.schemas`, keyed by the verbatim schema name.
    pub schemas: BTreeMap<String, Schema>,
    /// Every operation (paths × methods), flattened, in document order.
    /// Captured as data at load (decision D8 in docs/MIGRATION.md);
    /// consumed by the client emitter ([`crate::Generator::client`]).
    pub operations: Vec<OperationSpec>,
    /// `components.securitySchemes`, kept as raw values; the client
    /// emitter types the subset it generates providers for.
    pub security_schemes: BTreeMap<String, Value>,
    /// `servers[*].url`, in document order. The client emitter reads
    /// `servers[0]` as the default base URL.
    pub servers: Vec<String>,
}

impl Spec {
    /// Parse and normalize a loaded (and patched) OpenAPI document.
    pub fn from_value(document: &Value) -> Result<Self> {
        let root = document
            .as_object()
            .context("OpenAPI document is not an object")?;

        let meta = SpecMeta {
            title: document
                .pointer("/info/title")
                .and_then(Value::as_str)
                .map(str::to_string),
            version: document
                .pointer("/info/version")
                .and_then(Value::as_str)
                .map(str::to_string),
            openapi: root
                .get("openapi")
                .or_else(|| root.get("swagger"))
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        if let Some(version) = &meta.openapi
            && !(version.starts_with("3.0") || version.starts_with("3.1"))
        {
            bail!(
                "unsupported OpenAPI version {version:?}; convert Swagger 2.0 documents \
                 to 3.0.x first (3.1 documents are accepted; dialect differences are \
                 normalized here)",
            );
        }

        let mut schemas = BTreeMap::new();
        let schemas_origin = Origin::root().child("components").child("schemas");
        let raw_schemas = document
            .pointer("/components/schemas")
            .context("OpenAPI spec is missing /components/schemas")?
            .as_object()
            .context("/components/schemas is not an object")?;
        for (name, raw) in raw_schemas {
            let origin = schemas_origin.child(name);
            let schema = Schema::from_value(raw, origin.clone())
                .with_context(|| format!("in schema {origin}"))?;
            schemas.insert(name.clone(), schema);
        }

        let operations = operations::parse_operations(document)?;

        let security_schemes = document
            .pointer("/components/securitySchemes")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let servers = document
            .get("servers")
            .and_then(Value::as_array)
            .map(|servers| {
                servers
                    .iter()
                    .filter_map(|server| server.get("url").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Spec {
            meta,
            schemas,
            operations,
            security_schemes,
            servers,
        })
    }

    /// Render `components.schemas` as a draft-07 `definitions` map — the
    /// bridge feeding the typify engine. Reproduces the historical
    /// lowering: `#/components/schemas/` refs become `#/definitions/`,
    /// `nullable: true` becomes an `anyOf` with `null`, missing `type` is
    /// inferred from `format`, boolean exclusive bounds take draft-07's
    /// numeric form, and OpenAPI-only metadata
    /// (`example`/`examples`/`xml`/`externalDocs`/`discriminator`) is
    /// absent (the model keeps `discriminator` and examples; the render
    /// drops them for the strict draft-07 parser).
    ///
    /// Two nullable shapes are additionally restructured, because typify
    /// hands the definition's own name to the inner subschema of the
    /// `Option` it builds, colliding with the `Option`'s newtype wrapper
    /// (`X(Option<X>)`, E0428). Each hoists the non-null inner into a
    /// synthetic `{name}Inner` definition, mirroring the distinct naming
    /// typify gives the typed `type: [T, "null"]` form
    /// (`X(Option<XInner>)`):
    ///
    /// - a **named** untyped `nullable: true` wrapper (Plaid's nullable
    ///   `allOf` compositions), rendered `anyOf [inner, null]` — pure
    ///   `$ref` inners generate no named type and stay inline;
    /// - a **named** single-typed string enum whose members include a
    ///   literal `null` without `nullable: true` (Plaid's
    ///   `PayFrequencyValue` family) — typify prunes the null member
    ///   into an `Option` around the variant enum.
    ///
    /// A spec that already defines `{name}Inner` skips the hoist (typify
    /// then reports the collision loudly).
    pub fn to_draft07_definitions(&self) -> serde_json::Map<String, Value> {
        let mut definitions = serde_json::Map::new();
        for (name, schema) in &self.schemas {
            let rendered = schema.to_draft07();
            let inner_name = format!("{name}Inner");
            let inner = (!self.schemas.contains_key(&inner_name))
                .then(|| hoistable_nullable_inner(schema, &rendered))
                .flatten();
            match inner {
                Some((inner, wrapper_description)) => {
                    let mut wrapper = serde_json::Map::new();
                    if let Some(description) = wrapper_description {
                        wrapper.insert("description".to_string(), description);
                    }
                    wrapper.insert(
                        "anyOf".to_string(),
                        serde_json::json!([
                            { "$ref": format!("#/definitions/{inner_name}") },
                            { "type": "null" },
                        ]),
                    );
                    definitions.insert(inner_name, inner);
                    definitions.insert(name.clone(), Value::Object(wrapper));
                }
                None => {
                    definitions.insert(name.clone(), rendered);
                }
            }
        }
        definitions
    }

    /// [`Self::to_draft07_definitions`] wrapped as the
    /// [`RootSchema`](schemars::schema::RootSchema) that typify consumes.
    pub fn to_draft07_root(&self) -> Result<schemars::schema::RootSchema> {
        let root: schemars::schema::RootSchema = serde_json::from_value(serde_json::json!({
            "definitions": Value::Object(self.to_draft07_definitions()),
        }))
        .context("failed to deserialize the rendered schemas as a JSON Schema root")?;
        Ok(root)
    }
}

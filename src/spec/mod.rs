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
//! - the IR engine (step 2) lowers it into the IR directly.
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
    /// Captured as data for the future client emitter; nothing consumes
    /// these yet (decision D8 in docs/MIGRATION.md).
    pub operations: Vec<OperationSpec>,
    /// `components.securitySchemes`, kept opaque until the client
    /// emitter needs them typed.
    pub security_schemes: BTreeMap<String, Value>,
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

        Ok(Spec {
            meta,
            schemas,
            operations,
            security_schemes,
        })
    }

    /// Render `components.schemas` as a draft-07 `definitions` map — the
    /// bridge feeding the typify engine. Reproduces the historical
    /// lowering exactly: `#/components/schemas/` refs become
    /// `#/definitions/`, `nullable: true` becomes an `anyOf` with `null`,
    /// missing `type` is inferred from `format`, boolean exclusive bounds
    /// take draft-07's numeric form, and OpenAPI-only metadata
    /// (`example`/`examples`/`xml`/`externalDocs`/`discriminator`) is
    /// absent (the model keeps `discriminator` and examples; the render
    /// drops them for the strict draft-07 parser).
    pub fn to_draft07_definitions(&self) -> serde_json::Map<String, Value> {
        self.schemas
            .iter()
            .map(|(name, schema)| (name.clone(), schema.to_draft07()))
            .collect()
    }

    /// [`Self::to_draft07_definitions`] wrapped as the
    /// [`RootSchema`](schemars::schema::RootSchema) that typify consumes.
    pub fn to_draft07_root(&self) -> Result<schemars::schema::RootSchema> {
        let root: schemars::schema::RootSchema =
            serde_json::from_value(serde_json::json!({
                "definitions": Value::Object(self.to_draft07_definitions()),
            }))
            .context("failed to deserialize the rendered schemas as a JSON Schema root")?;
        Ok(root)
    }
}

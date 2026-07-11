//! Operations captured as data (decision D8 in docs/MIGRATION.md).
//!
//! Partitioning still walks the raw document (its reachability walk is
//! keyed by raw `$ref` strings); the client emitter ([`crate::client`])
//! consumes this model — one method per [`OperationSpec`], parameters
//! from [`Param`], request/response types from [`Body`].

use anyhow::Context;
use serde_json::Value;

use super::{Origin, Schema};
use crate::Result;

/// Where a parameter lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    Path,
    Query,
    Header,
    Cookie,
}

/// One operation parameter.
#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub location: ParamLocation,
    pub required: bool,
    pub schema: Option<Schema>,
    pub description: Option<String>,
    pub origin: Origin,
}

/// A request or response body: content-type → schema.
#[derive(Debug, Clone)]
pub struct Body {
    pub content_type: String,
    pub schema: Option<Schema>,
    pub origin: Origin,
}

/// One response arm.
#[derive(Debug, Clone)]
pub struct ResponseSpec {
    /// The raw status key: `"200"`, `"4XX"`, `"default"`, …
    pub status: String,
    pub description: Option<String>,
    pub bodies: Vec<Body>,
    pub origin: Origin,
}

/// One operation (path × method), flattened.
#[derive(Debug, Clone)]
pub struct OperationSpec {
    /// The raw `operationId`, when present.
    pub operation_id: Option<String>,
    pub method: String,
    pub path: String,
    pub description: Option<String>,
    pub summary: Option<String>,
    pub tags: Vec<String>,
    pub params: Vec<Param>,
    /// `$ref`-to-`components.parameters` entries, kept opaque as the raw
    /// reference strings. Nothing resolves them; the client emitter
    /// errors loudly when one appears on an operation it generates.
    pub ref_params: Vec<String>,
    pub request: Vec<Body>,
    pub responses: Vec<ResponseSpec>,
    /// Names of the security schemes required, per requirement set.
    pub security: Vec<Vec<String>>,
    pub origin: Origin,
}

const METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

pub(super) fn parse_operations(document: &Value) -> Result<Vec<OperationSpec>> {
    let Some(paths) = document.pointer("/paths").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };

    let paths_origin = Origin::root().child("paths");
    let mut operations = Vec::new();
    for (path, path_item) in paths {
        let Value::Object(path_methods) = path_item else {
            continue;
        };
        let path_origin = paths_origin.child(path);
        for method in METHODS {
            let Some(operation) = path_methods.get(*method) else {
                continue;
            };
            let origin = path_origin.child(method);
            operations.push(
                parse_operation(path, method, operation, origin.clone())
                    .with_context(|| format!("in operation {origin}"))?,
            );
        }
    }
    Ok(operations)
}

fn parse_operation(
    path: &str,
    method: &str,
    operation: &Value,
    origin: Origin,
) -> Result<OperationSpec> {
    let string_at = |pointer: &str| {
        operation
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    let mut params = Vec::new();
    let mut ref_params = Vec::new();
    if let Some(raw_params) = operation.get("parameters").and_then(Value::as_array) {
        let params_origin = origin.child("parameters");
        for (index, raw) in raw_params.iter().enumerate() {
            let param_origin = params_origin.index(index);
            // `$ref`-to-components-parameters entries are kept opaque:
            // recorded so the client emitter can refuse them loudly,
            // resolved by nothing.
            if let Some(reference) = raw.get("$ref").and_then(Value::as_str) {
                ref_params.push(reference.to_string());
                continue;
            }
            let name = raw
                .get("name")
                .and_then(Value::as_str)
                .with_context(|| format!("parameter at {param_origin} has no name"))?;
            let location = match raw.get("in").and_then(Value::as_str) {
                Some("path") => ParamLocation::Path,
                Some("query") => ParamLocation::Query,
                Some("header") => ParamLocation::Header,
                Some("cookie") => ParamLocation::Cookie,
                other => anyhow::bail!(
                    "parameter {name} at {param_origin} has unsupported location {other:?}",
                ),
            };
            let schema = raw
                .get("schema")
                .map(|schema| Schema::from_value(schema, param_origin.child("schema")))
                .transpose()?;
            params.push(Param {
                name: name.to_string(),
                location,
                required: raw
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                schema,
                description: raw
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                origin: param_origin,
            });
        }
    }

    let request = parse_content(
        operation.pointer("/requestBody/content"),
        origin.child("requestBody").child("content"),
    )?;

    let mut responses = Vec::new();
    if let Some(raw_responses) = operation.get("responses").and_then(Value::as_object) {
        let responses_origin = origin.child("responses");
        for (status, raw) in raw_responses {
            let response_origin = responses_origin.child(status);
            responses.push(ResponseSpec {
                status: status.clone(),
                description: raw
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                bodies: parse_content(raw.get("content"), response_origin.child("content"))?,
                origin: response_origin,
            });
        }
    }

    let security = operation
        .get("security")
        .and_then(Value::as_array)
        .map(|requirements| {
            requirements
                .iter()
                .filter_map(Value::as_object)
                .map(|requirement| requirement.keys().cloned().collect())
                .collect()
        })
        .unwrap_or_default();

    Ok(OperationSpec {
        operation_id: string_at("/operationId"),
        method: method.to_string(),
        path: path.to_string(),
        description: string_at("/description"),
        summary: string_at("/summary"),
        tags: operation
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        params,
        ref_params,
        request,
        responses,
        security,
        origin,
    })
}

fn parse_content(content: Option<&Value>, origin: Origin) -> Result<Vec<Body>> {
    let Some(content) = content.and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut bodies = Vec::new();
    for (content_type, media) in content {
        let body_origin = origin.child(content_type);
        let schema = media
            .get("schema")
            .map(|schema| Schema::from_value(schema, body_origin.child("schema")))
            .transpose()?;
        bodies.push(Body {
            content_type: content_type.clone(),
            schema,
            origin: body_origin,
        });
    }
    Ok(bodies)
}

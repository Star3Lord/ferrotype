//! Client-plan resolution: validate the spec's operations against the
//! v1 client boundaries and precompute everything emission needs.
//!
//! Resolution happens in
//! [`LoweredSchema::build_types`](crate::LoweredSchema::build_types) —
//! the one point where the typed spec model, the resolved style, the
//! (possibly user-edited) partition, and the populated
//! [`typify::TypeSpace`] all exist — so every unsupported construct
//! fails loudly there, carrying its [`Origin`], before any output is
//! rendered. Emission ([`super::emit`]) is infallible from a resolved
//! plan.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, bail};
use heck::ToSnakeCase;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use serde_json::Value;

use crate::Result;
use crate::config::StyleConfig;
use crate::partition::Partition;
use crate::spec::{Origin, ParamLocation, Schema, Spec, TypeHint};

/// Everything the client emitter needs, resolved and validated.
pub(crate) struct ClientPlan {
    /// `info.title`, for the `Client` doc comment.
    pub title: Option<String>,
    /// `info.version`, for the `Client` doc comment.
    pub version: Option<String>,
    /// `servers[0]`: the baked-in default base URL. `None` makes
    /// `base_url` a required `ClientBuilder::new` argument.
    pub default_base_url: Option<String>,
    /// One per operation, in document order.
    pub operations: Vec<OperationPlan>,
    /// Which auth providers to emit.
    pub auth: AuthPlan,
}

/// One generated client method.
pub(crate) struct OperationPlan {
    /// The method name: the `operationId`, snake-cased.
    pub method_name: syn::Ident,
    /// The verbatim `operationId`, for [`OperationInfo`] and errors.
    pub operation_id: String,
    /// Uppercase HTTP method.
    pub http_method: String,
    /// The path template, e.g. `/pets/{petId}`.
    pub path: String,
    /// Doc text: summary and/or description from the spec.
    pub doc: Option<String>,
    /// Path, query, and header parameters, in spec order (auth headers
    /// already folded away).
    pub params: Vec<ParamPlan>,
    /// The JSON request body type, when the operation has one.
    pub body: Option<TokenStream>,
    /// The success response type; `None` means every 2xx arm is
    /// body-less and the method returns `()`.
    pub response: Option<TokenStream>,
}

/// One method parameter.
pub(crate) struct ParamPlan {
    /// Snake-cased parameter name.
    pub rust_name: syn::Ident,
    /// The wire name, verbatim.
    pub wire_name: String,
    pub location: ParamLocation,
    pub required: bool,
    pub ty: ParamType,
}

/// How a scalar parameter is passed and formatted.
pub(crate) enum ParamType {
    /// Plain string: passed as `&str`.
    String,
    /// A `Copy` primitive (`i32`/`i64`/`f32`/`f64`/`bool`): by value.
    Copy(TokenStream),
    /// A mapped type (chrono/uuid/custom `[style.formats]` entry):
    /// passed as `&T`, rendered onto the wire via `Display`.
    Display(TokenStream),
}

/// Which auth providers the `auth` module carries. The `AuthProvider`
/// trait, `NoAuth`, and `StaticBearer` are always emitted; the rest
/// follow the spec's `securitySchemes`.
#[derive(Default)]
pub(crate) struct AuthPlan {
    /// An `http`/`basic` scheme exists.
    pub basic: bool,
    /// An `apiKey` scheme (header or query) exists.
    pub api_key: bool,
    /// An `oauth2` scheme with a `clientCredentials` flow exists.
    pub oauth2: Option<OAuth2Plan>,
}

/// The generated `OAuth2ClientCredentials` provider's spec defaults.
pub(crate) struct OAuth2Plan {
    /// The scheme's name in `securitySchemes`, for docs.
    pub scheme_name: String,
    /// The `clientCredentials` flow's `tokenUrl`.
    pub token_url: String,
    /// The spec's `x-base64-encode-client-credentials` extension:
    /// base64-encode id and secret individually before the standard
    /// basic-auth encoding of `id:secret`.
    pub base64_encode_credentials: bool,
}

impl ClientPlan {
    /// Resolve the plan or fail loudly on the first construct outside
    /// the v1 boundaries (non-JSON bodies, inline body schemas,
    /// multiple success schemas, non-scalar or `$ref` parameters —
    /// each error carries the spec [`Origin`] and, where a patch can
    /// help, says so).
    pub(crate) fn resolve(
        spec: &Spec,
        style: &StyleConfig,
        partition: Option<&Partition>,
        type_space: &typify::TypeSpace,
    ) -> Result<Self> {
        let auth = resolve_auth(spec)?;
        let auth_header_names = auth_header_names(spec);

        let rust_names: BTreeMap<String, String> =
            type_space.definition_rust_names().into_iter().collect();
        let modules =
            partition.map(|partition| crate::pipeline::resolved_rust_partition(partition, type_space, style));

        let mut operations = Vec::with_capacity(spec.operations.len());
        let mut seen_names: BTreeMap<String, String> = BTreeMap::new();
        for operation in &spec.operations {
            let plan = resolve_operation(
                operation,
                style,
                &rust_names,
                modules.as_ref(),
                &auth_header_names,
            )
            .with_context(|| format!("in operation {}", operation.origin))?;
            if let Some(previous) = seen_names
                .insert(plan.method_name.to_string(), plan.operation_id.clone())
            {
                bail!(
                    "operationIds {previous:?} and {:?} both map to client method \
                     `{}`; rename one (via a patch if needed)",
                    plan.operation_id,
                    plan.method_name,
                );
            }
            operations.push(plan);
        }

        Ok(ClientPlan {
            title: spec.meta.title.clone(),
            version: spec.meta.version.clone(),
            default_base_url: spec.servers.first().cloned(),
            operations,
            auth,
        })
    }
}

/// Parse `securitySchemes` into the set of providers to emit. Schemes
/// outside the supported set are loud errors — the escape hatches
/// (patch the scheme away, or pass a custom `AuthProvider` from `ext/`)
/// are named in the message.
fn resolve_auth(spec: &Spec) -> Result<AuthPlan> {
    let schemes_origin = Origin::root().child("components").child("securitySchemes");
    let mut plan = AuthPlan::default();
    for (name, raw) in &spec.security_schemes {
        let origin = schemes_origin.child(name);
        let scheme_type = raw
            .get("type")
            .and_then(Value::as_str)
            .with_context(|| format!("security scheme {name} at {origin} has no type"))?;
        match scheme_type {
            "http" => match raw.get("scheme").and_then(Value::as_str) {
                // Bearer maps onto the always-emitted StaticBearer.
                Some(scheme) if scheme.eq_ignore_ascii_case("bearer") => {}
                Some(scheme) if scheme.eq_ignore_ascii_case("basic") => plan.basic = true,
                other => bail!(
                    "security scheme {name} at {origin} has unsupported http scheme \
                     {other:?}; the client generator supports bearer and basic — \
                     patch the scheme or pass a custom AuthProvider from ext/",
                ),
            },
            "apiKey" => match raw.get("in").and_then(Value::as_str) {
                Some("header") | Some("query") => plan.api_key = true,
                other => bail!(
                    "security scheme {name} at {origin} is an apiKey in {other:?}; \
                     the client generator supports header and query keys only",
                ),
            },
            "oauth2" => {
                let flows = raw
                    .get("flows")
                    .and_then(Value::as_object)
                    .with_context(|| format!("oauth2 scheme {name} at {origin} has no flows"))?;
                let Some(client_credentials) = flows.get("clientCredentials") else {
                    let declared: Vec<&String> = flows.keys().collect();
                    bail!(
                        "oauth2 scheme {name} at {origin} declares flows {declared:?}; \
                         the client generator supports the clientCredentials flow only — \
                         patch the scheme or pass a custom AuthProvider from ext/",
                    );
                };
                let token_url = client_credentials
                    .get("tokenUrl")
                    .and_then(Value::as_str)
                    .with_context(|| {
                        format!("oauth2 scheme {name} at {origin} has no clientCredentials.tokenUrl")
                    })?;
                if plan.oauth2.is_some() {
                    bail!(
                        "multiple oauth2 security schemes are not supported; keep one \
                         and patch the others away (scheme {name} at {origin})",
                    );
                }
                plan.oauth2 = Some(OAuth2Plan {
                    scheme_name: name.clone(),
                    token_url: token_url.to_string(),
                    base64_encode_credentials: raw
                        .get("x-base64-encode-client-credentials")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
            other => bail!(
                "security scheme {name} at {origin} has unsupported type {other:?}; \
                 the client generator supports http (bearer/basic), apiKey \
                 (header/query), and oauth2 (clientCredentials) — patch the scheme \
                 or pass a custom AuthProvider from ext/",
            ),
        }
    }
    Ok(plan)
}

/// The header names the auth provider owns (lowercased): `authorization`
/// always, plus every `apiKey`-in-header scheme's header name. Under
/// `suppress-auth-headers` (the default), spec'd header parameters with
/// these names are folded out of method signatures.
fn auth_header_names(spec: &Spec) -> Vec<String> {
    let mut names = vec!["authorization".to_string()];
    for raw in spec.security_schemes.values() {
        if raw.get("type").and_then(Value::as_str) == Some("apiKey")
            && raw.get("in").and_then(Value::as_str) == Some("header")
            && let Some(name) = raw.get("name").and_then(Value::as_str)
        {
            names.push(name.to_ascii_lowercase());
        }
    }
    names
}

/// Local names the generated method bodies bind; a parameter
/// snake-casing to one of these would shadow them.
const RESERVED_PARAM_NAMES: &[&str] = &["url", "request", "query", "response", "body", "hook"];

fn resolve_operation(
    operation: &crate::spec::OperationSpec,
    style: &StyleConfig,
    rust_names: &BTreeMap<String, String>,
    modules: Option<&HashMap<String, String>>,
    auth_header_names: &[String],
) -> Result<OperationPlan> {
    let operation_id = operation.operation_id.clone().with_context(|| {
        format!(
            "operation {} {} has no operationId; the client generator derives \
             method names from it",
            operation.method, operation.path,
        )
    })?;
    let method_name = make_ident(&operation_id.to_snake_case())?;

    if !operation.ref_params.is_empty() {
        bail!(
            "operation {operation_id} uses $ref parameters ({}); the client \
             generator supports inline parameters only — inline them via a patch",
            operation.ref_params.join(", "),
        );
    }

    let mut params = Vec::with_capacity(operation.params.len());
    for param in &operation.params {
        if param.location == ParamLocation::Header
            && style.client.suppress_auth_headers
            && auth_header_names.contains(&param.name.to_ascii_lowercase())
        {
            // The auth provider owns this header; see `[client]
            // suppress-auth-headers`.
            continue;
        }
        if param.location == ParamLocation::Cookie {
            bail!(
                "parameter {} at {} is a cookie parameter; the client generator \
                 does not support cookies",
                param.name,
                param.origin,
            );
        }
        if param.location == ParamLocation::Path && !param.required {
            bail!(
                "path parameter {} at {} is not marked required",
                param.name,
                param.origin,
            );
        }
        let schema = param.schema.as_ref().with_context(|| {
            format!("parameter {} at {} has no schema", param.name, param.origin)
        })?;
        let ty = param_type(schema, style, &param.origin, &param.name)?;
        let rust_name = param.name.to_snake_case();
        if RESERVED_PARAM_NAMES.contains(&rust_name.as_str()) {
            bail!(
                "parameter {} at {} snake-cases to `{rust_name}`, which collides \
                 with a generated client local; rename it via a patch",
                param.name,
                param.origin,
            );
        }
        params.push(ParamPlan {
            rust_name: make_ident(&rust_name)?,
            wire_name: param.name.clone(),
            location: param.location,
            required: param.required,
            ty,
        });
    }

    check_path_template(&operation.path, &params, &operation.origin)?;

    let body = resolve_body(operation, rust_names, modules)?;
    let response = resolve_response(operation, &operation_id, rust_names, modules)?;

    let doc = match (&operation.summary, &operation.description) {
        (Some(summary), Some(description)) => Some(format!("{summary}\n\n{description}")),
        (Some(summary), None) => Some(summary.clone()),
        (None, Some(description)) => Some(description.clone()),
        (None, None) => None,
    };

    Ok(OperationPlan {
        method_name,
        operation_id,
        http_method: operation.method.to_uppercase(),
        path: operation.path.clone(),
        doc,
        params,
        body,
        response,
    })
}

/// Every `{name}` in the path template must name a path parameter and
/// vice versa.
fn check_path_template(path: &str, params: &[ParamPlan], origin: &Origin) -> Result<()> {
    let template_params = template_param_names(path)
        .with_context(|| format!("in the path template of {origin}"))?;
    for name in &template_params {
        if !params
            .iter()
            .any(|param| param.location == ParamLocation::Path && &param.wire_name == name)
        {
            bail!(
                "path template {path:?} at {origin} references parameter \
                 {name:?}, which is not declared as a path parameter",
            );
        }
    }
    for param in params {
        if param.location == ParamLocation::Path && !template_params.contains(&param.wire_name) {
            bail!(
                "path parameter {} at {origin} does not appear in the path \
                 template {path:?}",
                param.wire_name,
            );
        }
    }
    Ok(())
}

/// The `{name}` placeholders of a path template, in order.
pub(crate) fn template_param_names(path: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            bail!("unbalanced `{{` in path template {path:?}");
        };
        names.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    if rest.contains('}') {
        bail!("unbalanced `}}` in path template {path:?}");
    }
    Ok(names)
}

/// Resolve the request body: at most one JSON content type whose schema
/// is a `$ref` to a named schema.
fn resolve_body(
    operation: &crate::spec::OperationSpec,
    rust_names: &BTreeMap<String, String>,
    modules: Option<&HashMap<String, String>>,
) -> Result<Option<TokenStream>> {
    let Some(body) = operation.request.first() else {
        return Ok(None);
    };
    if operation.request.len() > 1 {
        let types: Vec<&str> = operation
            .request
            .iter()
            .map(|body| body.content_type.as_str())
            .collect();
        bail!(
            "request body at {} declares multiple content types ({}); the client \
             generator supports a single application/json body — patch the others away",
            body.origin,
            types.join(", "),
        );
    }
    if !is_json(&body.content_type) {
        bail!(
            "request body at {} has content type {}; the client generator \
             supports application/json only",
            body.origin,
            body.content_type,
        );
    }
    let schema = body
        .schema
        .as_ref()
        .with_context(|| format!("request body at {} has no schema", body.origin))?;
    Ok(Some(schema_type_ref(
        schema,
        &body.origin.child("schema"),
        rust_names,
        modules,
    )?))
}

/// Resolve the success response type: every 2xx arm must carry the same
/// `$ref` JSON schema (→ that type) or no content at all (→ `()`).
fn resolve_response(
    operation: &crate::spec::OperationSpec,
    operation_id: &str,
    rust_names: &BTreeMap<String, String>,
    modules: Option<&HashMap<String, String>>,
) -> Result<Option<TokenStream>> {
    let success: Vec<&crate::spec::ResponseSpec> = operation
        .responses
        .iter()
        .filter(|response| is_success_status(&response.status))
        .collect();
    if success.is_empty() {
        bail!(
            "operation {operation_id} at {} declares no 2xx response; the client \
             generator needs one to type the return value",
            operation.origin,
        );
    }

    let mut schema_refs: BTreeMap<String, Origin> = BTreeMap::new();
    let mut no_content = false;
    for response in &success {
        if response.bodies.is_empty() {
            no_content = true;
            continue;
        }
        for body in &response.bodies {
            if !is_json(&body.content_type) {
                bail!(
                    "response at {} has content type {}; the client generator \
                     supports application/json only",
                    body.origin,
                    body.content_type,
                );
            }
            let schema = body
                .schema
                .as_ref()
                .with_context(|| format!("response at {} has no schema", body.origin))?;
            let schema_origin = body.origin.child("schema");
            let name = ref_schema_name(schema, &schema_origin)?;
            schema_refs.entry(name).or_insert(schema_origin);
        }
    }

    match (schema_refs.len(), no_content) {
        (0, _) => Ok(None),
        (1, false) => {
            let (name, origin) = schema_refs.iter().next().expect("len checked");
            Ok(Some(named_type_ref(name, origin, rust_names, modules)?))
        }
        (1, true) => bail!(
            "operation {operation_id} at {} mixes a 2xx response schema with a \
             body-less 2xx response; the client generator supports one success \
             shape per operation — patch the spec to unify them",
            operation.origin,
        ),
        _ => {
            let names: Vec<&String> = schema_refs.keys().collect();
            bail!(
                "operation {operation_id} at {} declares multiple distinct 2xx \
                 response schemas ({names:?}); the client generator supports one \
                 success type per operation — patch the spec to unify them",
                operation.origin,
            )
        }
    }
}

/// `true` for `200`–`299` and the `2XX` wildcard.
fn is_success_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("2xx")
        || status
            .parse::<u16>()
            .is_ok_and(|code| (200..300).contains(&code))
}

/// `true` for `application/json` (parameters allowed) and `+json` types.
fn is_json(content_type: &str) -> bool {
    content_type == "application/json"
        || content_type.starts_with("application/json;")
        || content_type.ends_with("+json")
}

/// The named schema a body schema must `$ref`; inline schemas are the
/// loud v1 boundary with the patch mechanism as the escape.
fn ref_schema_name(schema: &Schema, origin: &Origin) -> Result<String> {
    let Some(reference) = &schema.reference else {
        bail!(
            "schema at {origin} is inline; the client generator needs a $ref to a \
             named schema — patch it into components.schemas and $ref it",
        );
    };
    let Some(name) = reference.strip_prefix("#/components/schemas/") else {
        bail!("schema at {origin} has non-components $ref {reference:?}");
    };
    Ok(name.to_string())
}

/// [`ref_schema_name`] + [`named_type_ref`] in one step.
fn schema_type_ref(
    schema: &Schema,
    origin: &Origin,
    rust_names: &BTreeMap<String, String>,
    modules: Option<&HashMap<String, String>>,
) -> Result<TokenStream> {
    let name = ref_schema_name(schema, origin)?;
    named_type_ref(&name, origin, rust_names, modules)
}

/// The `super::…` path of the generated Rust type for schema `name`,
/// as referenced from inside `pub mod client`: the partition supplies
/// the module (`super::cancel_booking::request::CancelBookingRequest`
/// in split mode, `super::shared::Pet` in flat-partitioned mode) and
/// unpartitioned output references root items (`super::Pet`).
fn named_type_ref(
    name: &str,
    origin: &Origin,
    rust_names: &BTreeMap<String, String>,
    modules: Option<&HashMap<String, String>>,
) -> Result<TokenStream> {
    let rust_name = rust_names.get(name).with_context(|| {
        format!(
            "schema {name:?} (referenced at {origin}) has no generated type — is \
             it replaced via [types] replace? The client generator needs the \
             generated type to exist",
        )
    })?;
    let ident = make_ident(rust_name)?;
    match modules.and_then(|modules| modules.get(rust_name)) {
        Some(module) => {
            let segments = module
                .split('/')
                .map(|segment| format_ident!("{segment}"));
            Ok(quote! { super::#(#segments::)*#ident })
        }
        None => Ok(quote! { super::#ident }),
    }
}

/// Map a scalar parameter schema to its Rust type, honoring the
/// resolved style's format mappings exactly as the type generation
/// does: `[style.formats."<type>/<format>"]` first, then the
/// `date`/`date-time`/`uuid` sugar keys, then typify's built-in
/// defaults.
fn param_type(
    schema: &Schema,
    style: &StyleConfig,
    origin: &Origin,
    name: &str,
) -> Result<ParamType> {
    if schema.reference.is_some() {
        bail!(
            "parameter {name} at {origin} has a $ref schema; the client generator \
             supports inline scalar parameter schemas only — inline it via a patch",
        );
    }
    let ty = schema.ty.with_context(|| {
        format!(
            "parameter {name} at {origin} has no schema type; the client \
             generator supports scalar parameters only",
        )
    })?;
    let format = schema.format.as_deref();
    let mapped = |instance_type: &str| {
        format.and_then(|format| {
            style
                .formats
                .get(&format!("{instance_type}/{format}"))
                .map(|mapping| mapping.type_path().to_string())
        })
    };
    match ty {
        TypeHint::String => {
            if let Some(path) = mapped("string") {
                return param_type_for_path(&path, origin, name);
            }
            let sugar = match format {
                Some("date") => Some(
                    style
                        .date
                        .clone()
                        .unwrap_or_else(|| "::chrono::naive::NaiveDate".to_string()),
                ),
                Some("date-time") => Some(style.date_time.clone().unwrap_or_else(|| {
                    "::chrono::DateTime<::chrono::offset::Utc>".to_string()
                })),
                Some("uuid") => Some(
                    style
                        .uuid
                        .clone()
                        .unwrap_or_else(|| "::uuid::Uuid".to_string()),
                ),
                // Unknown string formats are annotations (typify treats
                // them as plain strings too).
                _ => None,
            };
            match sugar {
                Some(path) => param_type_for_path(&path, origin, name),
                None => Ok(ParamType::String),
            }
        }
        TypeHint::Integer => {
            if let Some(path) = mapped("integer") {
                return param_type_for_path(&path, origin, name);
            }
            Ok(ParamType::Copy(match format {
                Some("int32") => quote!(i32),
                _ => quote!(i64),
            }))
        }
        TypeHint::Number => {
            if let Some(path) = mapped("number") {
                return param_type_for_path(&path, origin, name);
            }
            Ok(ParamType::Copy(match format {
                Some("float") => quote!(f32),
                _ => quote!(f64),
            }))
        }
        TypeHint::Boolean => Ok(ParamType::Copy(quote!(bool))),
        TypeHint::Object | TypeHint::Array | TypeHint::Null => bail!(
            "parameter {name} at {origin} has type {}; the client generator \
             supports scalar parameters only",
            ty.as_str(),
        ),
    }
}

/// A mapped parameter type: plain-`String` mappings collapse back to
/// `&str` parameters; anything else is passed by reference and rendered
/// via `Display` (which must produce the wire format).
fn param_type_for_path(path: &str, origin: &Origin, name: &str) -> Result<ParamType> {
    if matches!(path, "::std::string::String" | "std::string::String" | "String") {
        return Ok(ParamType::String);
    }
    let tokens: TokenStream = path.parse().map_err(|error| {
        anyhow::anyhow!(
            "mapped type {path:?} for parameter {name} at {origin} does not parse \
             as a Rust type: {error}",
        )
    })?;
    Ok(ParamType::Display(tokens))
}

/// Parse `name` as an identifier, falling back to the raw form for
/// keywords (`type` → `r#type`).
fn make_ident(name: &str) -> Result<syn::Ident> {
    syn::parse_str::<syn::Ident>(name)
        .or_else(|_| syn::parse_str::<syn::Ident>(&format!("r#{name}")))
        .map_err(|_| anyhow::anyhow!("cannot form a Rust identifier from {name:?}"))
}

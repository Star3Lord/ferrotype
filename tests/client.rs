//! The generated API client (docs/MIGRATION.md D22): emitter shape,
//! parameter mapping, auth-from-securitySchemes, the loud v1
//! boundaries, and the eject / ext ownership mechanics.
//!
//! Shape assertions run on the generated source text — reqwest and
//! friends are deliberately not dependencies of this crate. Compilation
//! of client output is proven by the verify gate (exercised in the
//! examples workspace) and the `via-cli-client` wiremock suite.

use openapi_codegen::{Generator, StyleProfile};

/// Write `document` (an OpenAPI JSON document) to a temp spec file and
/// return a client-enabled generator for it.
fn generator_for(name: &str, document: serde_json::Value) -> Generator {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("client_specs");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    Generator::new(path).profile(StyleProfile::ApiClient).client(true)
}

/// A minimal spec: one schema, one POST taking/returning it, plus
/// whatever `paths` / `securitySchemes` the caller splices in.
fn spec_with(
    paths: serde_json::Value,
    security_schemes: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut components = serde_json::json!({
        "schemas": {
            "Thing": {
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string" } }
            }
        }
    });
    if let Some(schemes) = security_schemes {
        components["securitySchemes"] = schemes;
    }
    serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "Things API", "version": "9.1" },
        "servers": [ { "url": "https://things.example.com/v1" } ],
        "paths": paths,
        "components": components,
    })
}

/// One POST /things operation with a JSON $ref body and response.
fn thing_paths() -> serde_json::Value {
    serde_json::json!({
        "/things": {
            "post": {
                "operationId": "createThing",
                "requestBody": {
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                },
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    })
}

// ─── Config surface ──────────────────────────────────────────────────────────

/// The `[client]` codegen.toml table: kebab-case keys, per-key defaults
/// on partial tables, unknown keys as hard errors.
#[test]
fn client_config_table_parses() {
    use openapi_codegen::StyleConfig;

    let config = StyleConfig::from_toml_str(
        "[client]\nenabled = true\nsuppress-auth-headers = false\n",
        StyleConfig::api_client(),
    )
    .unwrap();
    assert!(config.client.enabled);
    assert!(!config.client.suppress_auth_headers);
    assert!(config.client.ext_module, "unset keys keep their defaults");

    let off = StyleConfig::from_toml_str("", StyleConfig::api_client()).unwrap();
    assert!(!off.client.enabled, "client generation is off by default");

    let error = format!(
        "{:?}",
        StyleConfig::from_toml_str("[client]\nmock = true\n", StyleConfig::plain())
            .unwrap_err(),
    );
    assert!(error.contains("mock"), "{error}");
}

// ─── Client shape ────────────────────────────────────────────────────────────

/// The generated client surface: builder with the spec's server as the
/// default base URL, `From`-based client injection, hooks, and the
/// per-operation method wiring auth → hooks → send → text-first decode.
#[test]
fn client_module_shape() {
    let out = generator_for("shape", spec_with(thing_paths(), None))
        .generate_to_string()
        .unwrap();

    // Builder defaults: servers[0] baked in, Default impl present.
    assert!(out.contains(r#"base_url: "https://things.example.com/v1".to_string()"#), "{out}");
    assert!(out.contains("impl ::std::default::Default for ClientBuilder"), "{out}");
    // Client + doc title from info.
    assert!(out.contains("Client for Things API (v9.1)"), "{out}");
    // Middleware client with `From<reqwest::Client>` accepted.
    assert!(out.contains("::reqwest_middleware::ClientWithMiddleware"), "{out}");
    // The operation method: name, OperationInfo, hook loop, decode.
    assert!(out.contains("pub async fn create_thing("), "{out}");
    assert!(out.contains(r#"operation_id: "createThing""#), "{out}");
    assert!(out.contains("for hook in &self.body_hooks"), "{out}");
    assert!(out.contains("support::decode_json(OP.operation_id, response).await"), "{out}");
    // The support surface: text-first decode with line/column + raw body.
    assert!(out.contains("Error::Decode { op, source, body }"), "{out}");
    assert!(out.contains("line = source.line()"), "{out}");
}

/// Without `servers`, the builder requires the base URL as an argument
/// and no `Default` impl is emitted.
#[test]
fn missing_servers_makes_base_url_required() {
    let mut spec = spec_with(thing_paths(), None);
    spec.as_object_mut().unwrap().remove("servers");
    let out = generator_for("no_servers", spec).generate_to_string().unwrap();

    assert!(
        out.contains("pub fn new(base_url: impl ::std::convert::Into<::std::string::String>)"),
        "{out}",
    );
    assert!(!out.contains("impl ::std::default::Default for ClientBuilder"), "{out}");
}

/// Parameter mapping honors the resolved style: uuid/date-time map to
/// plain `&str` under the api-client preset (String mappings collapse),
/// scalars stay `Copy` by value, optionals wrap in `Option`, path
/// params percent-encode, query params skip `None`.
#[test]
fn params_map_through_the_style() {
    let paths = serde_json::json!({
        "/things/{thingId}": {
            "get": {
                "operationId": "getThing",
                "parameters": [
                    { "name": "thingId", "in": "path", "required": true,
                      "schema": { "type": "string", "format": "uuid" } },
                    { "name": "limit", "in": "query",
                      "schema": { "type": "integer", "format": "int32" } },
                    { "name": "verbose", "in": "query", "required": true,
                      "schema": { "type": "boolean" } },
                    { "name": "X-Request-Id", "in": "header",
                      "schema": { "type": "string" } }
                ],
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    });
    let out = generator_for("params", spec_with(paths, None))
        .generate_to_string()
        .unwrap();

    // api-client maps uuid → String → `&str` parameter.
    assert!(out.contains("thing_id: &str"), "{out}");
    assert!(out.contains("let thing_id = support::encode_path(thing_id);"), "{out}");
    assert!(out.contains("limit: ::std::option::Option<i32>"), "{out}");
    assert!(out.contains("verbose: bool"), "{out}");
    assert!(out.contains(r#"query.push(("verbose", verbose.to_string()))"#), "{out}");
    assert!(out.contains("if let ::std::option::Option::Some(value) = limit"), "{out}");
    assert!(out.contains("x_request_id: ::std::option::Option<&str>"), "{out}");
    assert!(out.contains(r#"request.header("X-Request-Id", value)"#), "{out}");
}

/// Under the plain profile the same uuid parameter maps to
/// `&uuid::Uuid` (typify's default), rendered via `Display`.
#[test]
fn plain_profile_keeps_typify_param_defaults() {
    let paths = serde_json::json!({
        "/things/{thingId}": {
            "get": {
                "operationId": "getThing",
                "parameters": [
                    { "name": "thingId", "in": "path", "required": true,
                      "schema": { "type": "string", "format": "uuid" } }
                ],
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("client_specs");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plain_params.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec_with(paths, None)).unwrap(),
    )
    .unwrap();
    let out = Generator::new(path)
        .profile(StyleProfile::Typify)
        .client(true)
        .generate_to_string()
        .unwrap();

    assert!(out.contains("thing_id: &::uuid::Uuid"), "{out}");
    assert!(
        out.contains("let thing_id = support::encode_path(&thing_id.to_string());"),
        "{out}",
    );
}

// ─── Auth from securitySchemes ───────────────────────────────────────────────

/// With no securitySchemes only the trait, NoAuth, and StaticBearer are
/// emitted.
#[test]
fn no_schemes_emits_baseline_providers() {
    let out = generator_for("auth_baseline", spec_with(thing_paths(), None))
        .generate_to_string()
        .unwrap();

    assert!(out.contains("pub trait AuthProvider"), "{out}");
    assert!(out.contains("pub struct NoAuth"), "{out}");
    assert!(out.contains("pub struct StaticBearer"), "{out}");
    assert!(!out.contains("pub struct BasicAuth"), "{out}");
    assert!(!out.contains("pub struct ApiKey"), "{out}");
    assert!(!out.contains("OAuth2ClientCredentials"), "{out}");
    assert!(!out.contains("::base64"), "{out}");
}

/// basic + apiKey schemes bring in their providers; bearer rides the
/// always-emitted StaticBearer.
#[test]
fn declared_schemes_bring_their_providers() {
    let schemes = serde_json::json!({
        "basic_auth": { "type": "http", "scheme": "basic" },
        "key_auth": { "type": "apiKey", "in": "header", "name": "X-API-Key" },
        "bearer_auth": { "type": "http", "scheme": "bearer" }
    });
    let out = generator_for("auth_declared", spec_with(thing_paths(), Some(schemes)))
        .generate_to_string()
        .unwrap();

    assert!(out.contains("pub struct BasicAuth"), "{out}");
    assert!(out.contains("pub struct ApiKey"), "{out}");
    assert!(out.contains("pub fn header("), "{out}");
    assert!(out.contains("pub fn query("), "{out}");
    assert!(!out.contains("OAuth2ClientCredentials"), "{out}");
}

/// The OAuth2 client-credentials provider bakes the spec's token URL
/// and the `x-base64-encode-client-credentials` default, caches under
/// `std::sync::Mutex` (no tokio), and sends the double-encoded Basic
/// header.
#[test]
fn oauth2_provider_reproduces_the_reference_client() {
    let schemes = serde_json::json!({
        "oauth2_authentication": {
            "type": "oauth2",
            "x-base64-encode-client-credentials": true,
            "flows": {
                "clientCredentials": {
                    "tokenUrl": "https://auth.example.com/v2/token",
                    "scopes": {}
                }
            }
        }
    });
    let out = generator_for("auth_oauth2", spec_with(thing_paths(), Some(schemes)))
        .generate_to_string()
        .unwrap();

    assert!(
        out.contains(r#"DEFAULT_TOKEN_URL: &'static str = "https://auth.example.com/v2/token""#),
        "{out}",
    );
    assert!(out.contains("base64_encode_client_credentials: true"), "{out}");
    assert!(out.contains("::std::sync::Mutex<::std::option::Option<CachedToken>>"), "{out}");
    assert!(!out.contains("tokio"), "{out}");
    assert!(out.contains(r#"form(&[("grant_type", "client_credentials")])"#), "{out}");
    // The double encoding: id and secret individually, then the pair.
    assert!(out.contains("let id = engine.encode(&self.client_id);"), "{out}");
    assert!(out.contains(r#"::std::format!("{id}:{secret}")"#), "{out}");
    assert!(out.contains("let encoded = engine.encode(credentials);"), "{out}");
    // The lock discipline (guard never held across an await) is
    // documented on the cache accessor.
    assert!(out.contains("never held across an await"), "{out}");
}

/// Spec'd auth header parameters fold out of signatures by default;
/// `suppress-auth-headers = false` keeps them.
#[test]
fn auth_headers_fold_unless_opted_out() {
    let schemes = serde_json::json!({
        "key_auth": { "type": "apiKey", "in": "header", "name": "X-API-Key" }
    });
    let paths = serde_json::json!({
        "/things": {
            "post": {
                "operationId": "createThing",
                "parameters": [
                    { "name": "Authorization", "in": "header", "required": true,
                      "schema": { "type": "string" } },
                    { "name": "X-API-Key", "in": "header", "required": true,
                      "schema": { "type": "string" } }
                ],
                "requestBody": {
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                },
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    });

    let folded = generator_for("auth_fold", spec_with(paths.clone(), Some(schemes.clone())))
        .generate_to_string()
        .unwrap();
    assert!(!folded.contains("authorization: &str"), "{folded}");
    assert!(!folded.contains("x_api_key"), "{folded}");

    let kept = generator_for("auth_fold_off", spec_with(paths, Some(schemes)))
        .style(|style| style.client.suppress_auth_headers = false)
        .generate_to_string()
        .unwrap();
    assert!(kept.contains("authorization: &str"), "{kept}");
    assert!(kept.contains("x_api_key: &str"), "{kept}");
}

// ─── v1 boundaries: loud errors with origins and patch hints ────────────────

fn expect_error(name: &str, document: serde_json::Value, needles: &[&str]) {
    let error = format!(
        "{:?}",
        generator_for(name, document).generate_to_string().unwrap_err(),
    );
    for needle in needles {
        assert!(error.contains(needle), "missing {needle:?} in error:\n{error}");
    }
}

/// Inline (non-$ref) body schemas error with the origin and the patch
/// hint.
#[test]
fn inline_body_schema_errors() {
    let paths = serde_json::json!({
        "/things": {
            "post": {
                "operationId": "createThing",
                "requestBody": {
                    "content": { "application/json": { "schema": { "type": "object" } } }
                },
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    });
    expect_error(
        "inline_body",
        spec_with(paths, None),
        &[
            "is inline",
            "patch it into components.schemas",
            "#/paths/~1things/post/requestBody/content/application~1json/schema",
        ],
    );
}

/// Non-JSON content types error naming the offending type.
#[test]
fn non_json_body_errors() {
    let paths = serde_json::json!({
        "/things": {
            "post": {
                "operationId": "createThing",
                "requestBody": {
                    "content": { "application/xml": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                },
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    });
    expect_error(
        "non_json",
        spec_with(paths, None),
        &["application/xml", "application/json only"],
    );
}

/// Two distinct 2xx schemas error; same-schema arms are fine.
#[test]
fn multiple_success_schemas_error() {
    let paths = serde_json::json!({
        "/things": {
            "post": {
                "operationId": "createThing",
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    },
                    "201": {
                        "description": "made",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Other" } } }
                    }
                }
            }
        }
    });
    let mut spec = spec_with(paths, None);
    spec["components"]["schemas"]["Other"] = serde_json::json!({
        "type": "object",
        "properties": { "id": { "type": "string" } }
    });
    expect_error(
        "multi_success",
        spec,
        &["multiple distinct 2xx response schemas", "one success type"],
    );
}

/// `$ref` parameters error with the inline-it hint.
#[test]
fn ref_params_error() {
    let paths = serde_json::json!({
        "/things": {
            "post": {
                "operationId": "createThing",
                "parameters": [ { "$ref": "#/components/parameters/Common" } ],
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    });
    expect_error(
        "ref_param",
        spec_with(paths, None),
        &["$ref parameters", "#/components/parameters/Common", "inline"],
    );
}

/// Non-scalar (object/array) parameters error with the origin.
#[test]
fn non_scalar_param_errors() {
    let paths = serde_json::json!({
        "/things": {
            "get": {
                "operationId": "listThings",
                "parameters": [
                    { "name": "filter", "in": "query",
                      "schema": { "type": "array", "items": { "type": "string" } } }
                ],
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    });
    expect_error(
        "array_param",
        spec_with(paths, None),
        &["parameter filter", "scalar parameters only", "type array"],
    );
}

/// Operations without an operationId error (method names come from it).
#[test]
fn missing_operation_id_errors() {
    let paths = serde_json::json!({
        "/things": {
            "post": {
                "responses": {
                    "200": {
                        "description": "ok",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Thing" } } }
                    }
                }
            }
        }
    });
    expect_error(
        "no_op_id",
        spec_with(paths, None),
        &["no operationId"],
    );
}

/// Operations with no 2xx arm error: nothing to type the return value.
#[test]
fn missing_success_response_errors() {
    let paths = serde_json::json!({
        "/things": {
            "post": {
                "operationId": "createThing",
                "responses": {
                    "400": { "description": "bad" }
                }
            }
        }
    });
    expect_error(
        "no_success",
        spec_with(paths, None),
        &["declares no 2xx response"],
    );
}

/// Unsupported security schemes error naming the escape hatches.
#[test]
fn unsupported_security_scheme_errors() {
    let schemes = serde_json::json!({
        "oidc": { "type": "openIdConnect", "openIdConnectUrl": "https://x" }
    });
    expect_error(
        "bad_scheme",
        spec_with(thing_paths(), Some(schemes)),
        &["unsupported type", "openIdConnect", "AuthProvider from ext/"],
    );
}

/// An oauth2 scheme without a clientCredentials flow errors, naming the
/// flows it found.
#[test]
fn oauth2_without_client_credentials_errors() {
    let schemes = serde_json::json!({
        "oauth2_code": {
            "type": "oauth2",
            "flows": {
                "authorizationCode": {
                    "authorizationUrl": "https://x/auth",
                    "tokenUrl": "https://x/token",
                    "scopes": {}
                }
            }
        }
    });
    expect_error(
        "oauth2_no_cc",
        spec_with(thing_paths(), Some(schemes)),
        &["clientCredentials flow only", "authorizationCode"],
    );
}

// ─── Ejection and the ext module ─────────────────────────────────────────────

/// A fresh tree directory for one test.
fn tree_dir(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("client_trees")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A split-tree, client-enabled generator over the temp spec.
fn tree_generator(name: &str) -> Generator {
    generator_for(name, spec_with(thing_paths(), None)).split_request_response(true)
}

/// The generated tree splits the client into client/{mod,auth,support}.rs,
/// declares `pub mod ext;` from the root, and scaffolds a marker-less
/// ext/mod.rs exactly once — user edits survive regeneration.
#[test]
fn tree_layout_and_ext_write_once() {
    let dir = tree_dir("ext_once");
    tree_generator("tree_ext").generate_to_dir(&dir).unwrap();

    for file in ["client/mod.rs", "client/auth.rs", "client/support.rs"] {
        let contents = std::fs::read_to_string(dir.join(file)).unwrap();
        assert!(contents.starts_with("// @generated"), "{file} lacks the marker");
    }
    let root = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
    assert!(root.contains("pub mod client;"), "{root}");
    assert!(root.contains("pub mod ext;"), "{root}");

    let ext_path = dir.join("ext/mod.rs");
    let scaffold = std::fs::read_to_string(&ext_path).unwrap();
    assert!(
        !scaffold.lines().next().unwrap().starts_with("// @generated"),
        "ext/mod.rs must not open with the marker:\n{scaffold}",
    );

    // User takes over: edits survive regeneration byte-for-byte, and
    // extra files under ext/ are never cleaned up.
    let edited = format!("{scaffold}\npub mod hooks;\n");
    std::fs::write(&ext_path, &edited).unwrap();
    let hooks_path = dir.join("ext/hooks.rs");
    std::fs::write(&hooks_path, "pub fn install() {}\n").unwrap();

    tree_generator("tree_ext").generate_to_dir(&dir).unwrap();
    assert_eq!(std::fs::read_to_string(&ext_path).unwrap(), edited);
    assert!(hooks_path.exists(), "user files under ext/ must survive");
}

/// `ext-module = false` drops both the declaration and the scaffold.
#[test]
fn ext_module_can_be_disabled() {
    let dir = tree_dir("ext_off");
    tree_generator("tree_ext_off")
        .style(|style| style.client.ext_module = false)
        .generate_to_dir(&dir)
        .unwrap();

    let root = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
    assert!(!root.contains("pub mod ext;"), "{root}");
    assert!(!dir.join("ext").exists());
}

/// Eject rewrites the header; the ejected file is skipped on
/// regeneration (edits survive) and never deleted; deleting it and
/// regenerating restores the generated version.
#[test]
fn eject_roundtrip() {
    let dir = tree_dir("eject");
    tree_generator("tree_eject").generate_to_dir(&dir).unwrap();
    let auth_path = dir.join("client/auth.rs");

    openapi_codegen::eject_file(&auth_path).unwrap();
    let ejected = std::fs::read_to_string(&auth_path).unwrap();
    let first_line = ejected.lines().next().unwrap();
    assert_eq!(
        first_line,
        "// @ejected — was generated from specs/tree_eject.json; delete this file \
         and regenerate to restore."
            .replace("specs/tree_eject.json", &spec_display("tree_eject")),
        "{ejected}",
    );
    assert!(!ejected.contains("// Do not edit by hand."), "{ejected}");

    // User owns the file now: edits survive regeneration.
    let edited = format!("{ejected}\n// my auth tweak\n");
    std::fs::write(&auth_path, &edited).unwrap();
    tree_generator("tree_eject").generate_to_dir(&dir).unwrap();
    assert_eq!(std::fs::read_to_string(&auth_path).unwrap(), edited);

    // Un-eject: delete + regenerate restores the generated file.
    std::fs::remove_file(&auth_path).unwrap();
    tree_generator("tree_eject").generate_to_dir(&dir).unwrap();
    let restored = std::fs::read_to_string(&auth_path).unwrap();
    assert!(restored.starts_with("// @generated"), "{restored}");
    assert!(!restored.contains("my auth tweak"), "{restored}");
}

/// The display form of a temp spec path, for header assertions.
fn spec_display(name: &str) -> String {
    std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("client_specs")
        .join(format!("{name}.json"))
        .display()
        .to_string()
}

/// Ejecting a missing, unmarked, or already-ejected file is a clear
/// error.
#[test]
fn eject_rejects_non_generated_files() {
    let dir = tree_dir("eject_errors");
    std::fs::create_dir_all(&dir).unwrap();

    let missing = format!(
        "{:?}",
        openapi_codegen::eject_file(dir.join("nope.rs")).unwrap_err(),
    );
    assert!(missing.contains("does it exist"), "{missing}");

    let unmarked = dir.join("mine.rs");
    std::fs::write(&unmarked, "pub fn mine() {}\n").unwrap();
    let error = format!("{:?}", openapi_codegen::eject_file(&unmarked).unwrap_err());
    assert!(error.contains("no `// @generated` marker"), "{error}");

    let ejected = dir.join("done.rs");
    std::fs::write(&ejected, "// @ejected — was generated from x; delete this file and regenerate to restore.\n").unwrap();
    let error = format!("{:?}", openapi_codegen::eject_file(&ejected).unwrap_err());
    assert!(error.contains("already ejected"), "{error}");
}

/// Single-file mode appends `pub mod client { ... }` but never
/// declares `pub mod ext;` (there is no directory to own).
#[test]
fn single_file_mode_has_no_ext_module()
{
    let out = generator_for("single_no_ext", spec_with(thing_paths(), None))
        .generate_to_string()
        .unwrap();
    assert!(out.contains("pub mod client {"), "{out}");
    assert!(!out.contains("pub mod ext"), "{out}");
}

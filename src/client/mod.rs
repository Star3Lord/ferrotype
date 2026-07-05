//! Opt-in API client generation: a concrete
//! `reqwest_middleware`-based client emitted next to the generated
//! types (`[client] enabled = true`, `--client`, or
//! [`Generator::client`](crate::Generator::client)).
//!
//! What gets generated, per spec:
//!
//! - `pub mod client` — `Client` (holds the base URL, a
//!   [`ClientWithMiddleware`](https://docs.rs/reqwest-middleware), an
//!   `Arc<dyn AuthProvider>`, and registered body hooks), its builder,
//!   and one `pub async fn <operation_id>` per operation;
//! - `client::auth` — the `AuthProvider` trait plus provider impls
//!   derived from `securitySchemes` (`NoAuth`, `StaticBearer`, and —
//!   as declared — `BasicAuth`, `ApiKey`, `OAuth2ClientCredentials`
//!   with a token-fetch + TTL cache);
//! - `client::support` — `Error` (status/decode diagnostics carrying
//!   the raw response body), `OperationInfo`, path percent-encoding,
//!   and the text-first JSON decode helpers;
//! - in directory-tree output, a write-once user-owned `ext/` module
//!   (see the `[client] ext-module` key) — the marker-less home for
//!   hooks and impls that survive regeneration.
//!
//! Resolution ([`plan`]) and emission ([`emit`]) are split: every v1
//! boundary — JSON bodies only, `$ref` body schemas only, one success
//! schema per operation, scalar parameters only — fails loudly during
//! resolution (in
//! [`LoweredSchema::build_types`](crate::LoweredSchema::build_types)),
//! carrying the spec [`Origin`](crate::spec::Origin) and a patch hint
//! where one helps. Generated-code dependencies stay minimal: reqwest,
//! reqwest-middleware, serde, serde_json, async-trait, and base64 only
//! when an OAuth2 provider is emitted; the verify gate
//! ([`crate::verify`]) auto-declares each when the output references
//! it.

mod auth;
mod emit;
mod plan;
mod support;

pub(crate) use emit::client_tokens;
pub(crate) use plan::ClientPlan;

/// The scaffolded `ext/mod.rs` contents. Deliberately *not* starting
/// with the `// @generated` marker: the file is born user-owned, so the
/// tree writer never overwrites it and stale cleanup never deletes it.
pub(crate) fn ext_scaffold(spec_path: &std::path::Path) -> String {
    format!(
        "//! User-owned extensions for the types and client generated from\n\
         //! `{}`.\n\
         //!\n\
         //! Scaffolded once by openapi-codegen and never touched again: this file\n\
         //! carries no `// @generated` marker, so regeneration skips it and stale\n\
         //! cleanup ignores it — the same is true of anything you add under `ext/`\n\
         //! (`pub mod hooks;`, `pub mod pcc;`, …).\n\
         //!\n\
         //! This is the coherence-legal home for code that belongs next to the\n\
         //! generated output: `impl` blocks on generated types, helper types, and\n\
         //! body hooks to register via `client::ClientBuilder::body_hook`.\n",
        spec_path.display(),
    )
}

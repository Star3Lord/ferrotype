//! Generate ergonomic Rust types from OpenAPI specs.
//!
//! This crate drives the local [typify fork](../../typify/FORK_FEATURES.md)
//! end-to-end: it loads an OpenAPI 3.x document (YAML or JSON), optionally
//! applies RFC 6902 patch files, lowers the OpenAPI-specific constructs to
//! plain JSON Schema, runs typify with a configurable style profile, and
//! writes formatted Rust — as one flat module, partitioned into one
//! module per OpenAPI operation with a `shared` module for common types,
//! or — with [`Generator::split_request_response`] — partitioned into
//! per-operation `request`/`response` submodules with a role-classified
//! `shared::{request, response, enums, common}` subtree (see
//! [`Partition::compute_split`] for the classification policy).
//!
//! Output can be a single file ([`Generator::generate_to_file`]) or a
//! directory tree mirroring the module structure
//! ([`Generator::generate_to_dir`]): one file per partition module, a
//! root `mod.rs`, `// @generated` headers everywhere, idempotent writes,
//! and stale generated files cleaned up (see [`write_file_tree`]).
//!
//! # Library use (e.g. from another crate's `build.rs`)
//!
//! ```no_run
//! use openapi_codegen::{Generator, StyleProfile};
//!
//! let rust_source = Generator::new("specs/petstore.yaml")
//!     .profile(StyleProfile::ApiClient)
//!     .partition_by_operation(true)
//!     .generate_to_string()
//!     .unwrap();
//! std::fs::write("src/generated/petstore.rs", rust_source).unwrap();
//! ```
//!
//! Granular control beyond the profile presets goes through
//! [`Generator::customize`], which exposes the underlying
//! [`typify::TypeSpaceSettings`].
//!
//! # Staged pipeline (step-by-step control)
//!
//! When the builder hooks aren't enough, [`Generator::load`] runs the same
//! pipeline one checkpoint at a time, with every intermediate artifact —
//! parsed spec, operation [`Partition`], [`typify::TypeSpaceSettings`],
//! [`typify::TypeSpace`], and finally the [`syn::File`] AST — open for
//! inspection and mutation between stages:
//!
//! ```no_run
//! use openapi_codegen::{Generator, StyleProfile, render_file};
//!
//! let mut stage = Generator::new("specs/petstore.yaml")
//!     .profile(StyleProfile::ApiClient)
//!     .partition_by_operation(true)
//!     .load()?;                                    // spec parsed + patched
//! stage.spec_mut()["info"]["title"] = "Renamed".into();
//!
//! let mut stage = stage.lower()?;                  // partitioned + lowered
//! stage.partition_mut().unwrap().by_schema
//!     .insert("Pet".into(), "create_pet".into()); // move a type
//! stage.settings_mut().with_schema_in_docs(true); // any typify knob
//!
//! let stage = stage.build_types()?;                // typify has run
//! let names = stage.type_space().definition_rust_names();
//!
//! let mut file = stage.into_file()?;               // post-processed AST
//! file.items.push(syn::parse_quote! { pub const GENERATED: bool = true; });
//!
//! let source = render_file(&file, "specs/petstore.yaml");
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! With no between-stage edits the staged path and
//! [`Generator::generate_to_string`] produce byte-identical output — the
//! one-shot method is implemented as exactly this sequence.
//!
//! # CLI use
//!
//! ```text
//! openapi-codegen generate --spec specs/petstore.yaml --profile api-client \
//!     --partition-by-operation --output src/generated/petstore.rs
//!
//! # request/response splitting + folder-tree output
//! openapi-codegen generate --spec specs/petstore.yaml --profile api-client \
//!     --split-request-response --output-dir src/generated/petstore
//! ```
//!
//! # Macro use
//!
//! `typify::import_types!` consumes JSON Schema, not OpenAPI. The `lower`
//! subcommand bridges the gap: it writes the lowered
//! `{"definitions": {...}}` document that `import_types!` (with the fork's
//! macro knobs) can consume directly.
//!
//! ```text
//! openapi-codegen lower --spec specs/petstore.yaml --output petstore.schema.json
//! ```

mod generate;
mod load;
mod lower;
mod partition;
mod pipeline;
mod postprocess;
mod profile;
mod tree;

pub use generate::Generator;
pub use load::{apply_patches_dir, load_spec};
pub use lower::{lower_to_json_schema, lowered_root_schema};
pub use partition::{
    Partition, SHARED_COMMON_MODULE, SHARED_ENUMS_MODULE, SHARED_MODULE, SHARED_REQUEST_MODULE,
    SHARED_RESPONSE_MODULE,
};
pub use pipeline::{GeneratedTypes, LoadedSpec, LoweredSchema, render_file};
pub use profile::StyleProfile;
pub use tree::write_file_tree;

/// Everything here can fail with a plain [`anyhow::Error`]; codegen is a
/// build-time tool and callers want the full context chain, not a typed
/// error to match on.
pub type Result<T> = anyhow::Result<T>;

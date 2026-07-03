//! The staged pipeline: run codegen one checkpoint at a time.
//!
//! [`Generator::generate_to_string`](crate::Generator::generate_to_string)
//! runs load → partition → lower → typify → post-process → format in one
//! shot; the builder hooks cover spec and settings edits, but nothing in
//! between is reachable. [`Generator::load`](crate::Generator::load) runs
//! the same pipeline stopping after every stage, handing back the
//! intermediate artifact for inspection or mutation before the next stage
//! consumes it:
//!
//! ```text
//! Generator::load()  → LoadedSpec      (parsed spec; patches + hooks applied)
//!   .lower()         → LoweredSchema   (partition, JSON Schema, typify settings)
//!   .build_types()   → GeneratedTypes  (populated TypeSpace)
//!   .into_file()     → syn::File       (post-processed AST)
//! render_file(&file, spec_path)        (formatted source + header)
//! ```
//!
//! Every stage owns its data — no lifetimes tie it back to the
//! [`Generator`](crate::Generator) — and with no between-stage mutations
//! the final output is byte-identical to the one-shot path, which is
//! itself implemented as exactly this sequence.

use std::path::{Path, PathBuf};

use anyhow::Context;
use proc_macro2::TokenStream;
use quote::quote;
use schemars::schema::RootSchema;
use serde_json::Value;
use typify::{TypeSpace, TypeSpaceSettings};

use crate::partition::Partition;
use crate::profile::StyleProfile;
use crate::spec::Spec;
use crate::{Result, postprocess};

/// Pipeline checkpoint after loading: the spec is parsed and patch files
/// plus [`patch_spec_with`](crate::Generator::patch_spec_with) hooks have
/// been applied, but nothing has been lowered yet. Created by
/// [`Generator::load`](crate::Generator::load).
///
/// The full staged flow:
///
/// ```no_run
/// use openapi_codegen::{Generator, StyleProfile, render_file};
///
/// let mut stage = Generator::new("specs/petstore.yaml")
///     .profile(StyleProfile::ApiClient)
///     .partition_by_operation(true)
///     .load()?;                          // LoadedSpec: spec open for edits
/// stage.spec_mut()["info"]["title"] = "Renamed".into();
///
/// let mut stage = stage.lower()?;        // LoweredSchema: partition + settings
/// stage.partition_mut().unwrap().by_schema
///     .insert("Pet".into(), "create_pet".into());
/// stage.settings_mut().with_schema_in_docs(true);
///
/// let stage = stage.build_types()?;      // GeneratedTypes: populated TypeSpace
/// let names = stage.type_space().definition_rust_names();
///
/// let mut file = stage.into_file()?;     // syn::File, post-processing applied
/// file.items.push(syn::parse_quote! { pub const GENERATED: bool = true; });
///
/// let source = render_file(&file, "specs/petstore.yaml");
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct LoadedSpec {
    pub(crate) spec: Value,
    pub(crate) settings: TypeSpaceSettings,
    pub(crate) profile: StyleProfile,
    pub(crate) partition_by_operation: bool,
    pub(crate) split_request_response: bool,
    pub(crate) spec_path: PathBuf,
}

impl LoadedSpec {
    /// The parsed OpenAPI document.
    pub fn spec(&self) -> &Value {
        &self.spec
    }

    /// Mutable access to the parsed OpenAPI document — the last chance to
    /// edit the spec (still in vanilla OpenAPI shape, `$ref`s keyed by
    /// `#/components/schemas/`) before partitioning and lowering read it.
    pub fn spec_mut(&mut self) -> &mut Value {
        &mut self.spec
    }

    /// Compute the operation [`Partition`] (when
    /// [`partition_by_operation`](crate::Generator::partition_by_operation)
    /// or
    /// [`split_request_response`](crate::Generator::split_request_response)
    /// was enabled), normalize the document into the typed [`Spec`]
    /// model, and render the JSON Schema [`RootSchema`] the typify
    /// engine consumes. The partition reads the raw document (its
    /// reachability walk is keyed by `#/components/schemas/` refs); the
    /// schema comes from [`Spec::to_draft07_root`], byte-identical to
    /// the historical in-place lowering.
    pub fn lower(self) -> Result<LoweredSchema> {
        let partition = if self.split_request_response {
            let partition = Partition::compute_split(&self.spec)?;
            partition.log_summary(&spec_label(&self.spec_path));
            Some(partition)
        } else if self.partition_by_operation {
            let partition = Partition::compute(&self.spec)?;
            partition.log_summary(&spec_label(&self.spec_path));
            Some(partition)
        } else {
            None
        };

        let spec_model = Spec::from_value(&self.spec)?;
        let schema = spec_model.to_draft07_root()?;
        Ok(LoweredSchema {
            schema,
            partition,
            settings: self.settings,
            profile: self.profile,
            spec_path: self.spec_path,
        })
    }
}

/// Pipeline checkpoint after lowering: the JSON Schema typify will
/// consume, the operation partition (if enabled), and the
/// [`TypeSpaceSettings`] — pre-populated by the profile and
/// [`customize`](crate::Generator::customize) hooks — are all open for
/// inspection and mutation before typify runs.
pub struct LoweredSchema {
    schema: RootSchema,
    partition: Option<Partition>,
    settings: TypeSpaceSettings,
    profile: StyleProfile,
    spec_path: PathBuf,
}

impl LoweredSchema {
    /// The lowered JSON Schema (draft-07, `definitions`-keyed) that
    /// [`Self::build_types`] will feed to typify.
    pub fn schema(&self) -> &RootSchema {
        &self.schema
    }

    /// Mutable access to the lowered schema; replace it wholesale with
    /// `*stage.schema_mut() = other` or edit definitions in place.
    pub fn schema_mut(&mut self) -> &mut RootSchema {
        &mut self.schema
    }

    /// The computed operation partition, or `None` when
    /// [`partition_by_operation`](crate::Generator::partition_by_operation)
    /// was not enabled.
    pub fn partition(&self) -> Option<&Partition> {
        self.partition.as_ref()
    }

    /// Mutable access to the partition. Reassigning
    /// [`Partition::by_schema`] entries moves the corresponding types
    /// between modules in the final output. When pointing a schema at a
    /// brand-new module (not an existing operation module), also add the
    /// name to [`Partition::op_modules`] so the module receives the
    /// standard import preamble.
    pub fn partition_mut(&mut self) -> Option<&mut Partition> {
        self.partition.as_mut()
    }

    /// The typify settings, as left by the style profile and
    /// [`customize`](crate::Generator::customize) hooks.
    pub fn settings(&self) -> &TypeSpaceSettings {
        &self.settings
    }

    /// Mutable access to the typify settings — every knob of the fork is
    /// reachable here, after the profile has been applied.
    pub fn settings_mut(&mut self) -> &mut TypeSpaceSettings {
        &mut self.settings
    }

    /// Run typify: build a [`TypeSpace`] from the settings and populate it
    /// from the lowered schema.
    pub fn build_types(self) -> Result<GeneratedTypes> {
        let mut type_space = TypeSpace::new(&self.settings);
        type_space
            .add_root_schema(self.schema)
            .context("typify type generation failed")?;
        Ok(GeneratedTypes {
            type_space,
            partition: self.partition,
            profile: self.profile,
            spec_path: self.spec_path,
        })
    }
}

/// Pipeline checkpoint after typify: the [`TypeSpace`] holds every
/// generated type. Inspect it, take raw tokens via [`Self::tokens`], or
/// continue to a post-processed [`syn::File`] via [`Self::into_file`].
pub struct GeneratedTypes {
    type_space: TypeSpace,
    partition: Option<Partition>,
    profile: StyleProfile,
    spec_path: PathBuf,
}

impl GeneratedTypes {
    /// The populated type space, e.g. for
    /// [`typify::TypeSpace::definition_rust_names`] or
    /// [`typify::TypeSpace::iter_types`].
    pub fn type_space(&self) -> &TypeSpace {
        &self.type_space
    }

    /// The operation partition this stage will emit with, or `None` when
    /// partitioning was not enabled. Useful together with
    /// [`Self::type_space`] to inspect the final module assignment
    /// (e.g. via [`Partition::to_rust_partition`], which is also where
    /// split-mode simple-enum routing to `shared/enums` is resolved).
    pub fn partition(&self) -> Option<&Partition> {
        self.partition.as_ref()
    }

    /// The raw generated tokens — the escape hatch below [`Self::into_file`].
    ///
    /// Honors the partition as it stands now (module placement is resolved
    /// here, so earlier [`LoweredSchema::partition_mut`] edits apply) and
    /// includes the profile's trait imports, but no post-processing: that
    /// only happens in [`Self::into_file`].
    pub fn tokens(&self) -> TokenStream {
        match &self.partition {
            Some(partition) => {
                let rust_partition = partition.to_rust_partition(&self.type_space);
                let imports = partition.module_imports(&self.profile.trait_imports());
                self.type_space.to_stream_partitioned(
                    &rust_partition,
                    partition.default_module(),
                    &imports,
                )
            }
            None => {
                let trait_imports = self.profile.trait_imports();
                let body = self.type_space.to_stream();
                quote! {
                    #trait_imports
                    #body
                }
            }
        }
    }

    /// Parse the generated tokens into a [`syn::File`] and apply the
    /// profile's default post-processing (`impl Default` synthesis for
    /// enums typify can't default, under
    /// [`StyleProfile::ApiClient`]). Edit the returned AST freely, then
    /// finish with [`render_file`]. To skip the default post-processing,
    /// parse [`Self::tokens`] yourself instead.
    pub fn into_file(self) -> Result<syn::File> {
        self.build_file()
    }

    /// Format and render in one step; equivalent to [`Self::into_file`]
    /// followed by [`render_file`] with the generator's spec path.
    pub fn render(self) -> Result<String> {
        let source = render_file(&self.build_file()?, &self.spec_path);
        Ok(source)
    }

    /// Split the generated module tree into a directory of files rooted
    /// at `dir` and write them; equivalent to [`Self::into_file`]
    /// followed by [`write_file_tree`](crate::write_file_tree) with the
    /// generator's spec path. This is the staged-pipeline counterpart of
    /// [`Generator::generate_to_dir`](crate::Generator::generate_to_dir).
    pub fn render_to_dir(self, dir: impl AsRef<Path>) -> Result<()> {
        let file = self.build_file()?;
        crate::tree::write_file_tree(&file, &self.spec_path, dir)
    }

    fn build_file(&self) -> Result<syn::File> {
        let mut file = syn::parse_file(&self.tokens().to_string())
            .context("generated tokens failed to parse as a Rust file")?;
        if self.profile.synthesize_enum_defaults() {
            postprocess::synthesize_enum_defaults(&mut file);
        }
        Ok(file)
    }
}

/// The marker opening every generated file's first line. The stale-file
/// cleanup in [`write_file_tree`](crate::write_file_tree) only ever
/// deletes files whose first line starts with this marker.
pub(crate) const GENERATED_MARKER: &str = "// @generated";

/// The `// @generated` header naming `spec_path` as the source document.
pub(crate) fn generated_header(spec_path: impl AsRef<Path>) -> String {
    format!(
        "{GENERATED_MARKER} by openapi-codegen from {}\n// Do not edit by hand.\n\n",
        spec_path.as_ref().display(),
    )
}

/// Format `file` with prettyplease and prepend the `// @generated` header
/// naming `spec_path` as the source document. This is the exact final step
/// of [`Generator::generate_to_string`](crate::Generator::generate_to_string),
/// so an unedited [`GeneratedTypes::into_file`] result rendered here is
/// byte-identical to the one-shot output.
pub fn render_file(file: &syn::File, spec_path: impl AsRef<Path>) -> String {
    let body = prettyplease::unparse(file);
    let header = generated_header(spec_path);
    format!("{header}{body}")
}

/// Human-readable spec name for log lines: the file stem, falling back to
/// the whole path.
fn spec_label(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

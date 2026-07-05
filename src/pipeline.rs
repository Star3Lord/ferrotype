//! The staged pipeline: run codegen one checkpoint at a time.
//!
//! [`Generator::generate_to_string`](crate::Generator::generate_to_string)
//! runs load → partition → lower → typify → post-process → format in one
//! shot; the builder hooks cover spec, style, and settings edits, but
//! nothing in between is reachable. [`Generator::load`](crate::Generator::load)
//! runs the same pipeline stopping after every stage, handing back the
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

use crate::config::{EmitStyle, StyleConfig};
use crate::overrides::Overrides;
use crate::partition::Partition;
use crate::spec::Spec;
use crate::{Result, condense, postprocess};

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
    pub(crate) style: StyleConfig,
    pub(crate) overrides: Overrides,
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

    /// The resolved [`StyleConfig`] driving this run.
    pub fn style(&self) -> &StyleConfig {
        &self.style
    }

    /// Compute the operation [`Partition`] (when
    /// [`partition_by_operation`](crate::Generator::partition_by_operation)
    /// or
    /// [`split_request_response`](crate::Generator::split_request_response)
    /// was enabled), normalize the document into the typed [`Spec`]
    /// model, and render the JSON Schema [`RootSchema`] the typify
    /// engine consumes. The partition reads the raw document (its
    /// reachability walk is keyed by `#/components/schemas/` refs); the
    /// schema comes from [`Spec::to_draft07_root`].
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
        // The `[[rules]]` tier resolves here — the one point where the
        // typed spec model (format provenance) and the partition
        // (module placement) both exist. Type-level patch decisions
        // flow into the override plan's patchability immediately.
        let field_rules =
            crate::rules::FieldRules::resolve(&self.style, &spec_model, partition.as_ref())?;
        let mut overrides = self.overrides;
        overrides.set_rule_patchability(field_rules.patch_overrides().clone());
        Ok(LoweredSchema {
            schema,
            partition,
            settings: self.settings,
            style: self.style,
            overrides,
            field_rules,
            spec_model,
            spec_path: self.spec_path,
        })
    }
}

/// Pipeline checkpoint after lowering: the JSON Schema typify will
/// consume, the operation partition (if enabled), and the
/// [`TypeSpaceSettings`] — pre-populated by the resolved style and
/// [`customize`](crate::Generator::customize) hooks — are all open for
/// inspection and mutation before typify runs.
pub struct LoweredSchema {
    schema: RootSchema,
    partition: Option<Partition>,
    settings: TypeSpaceSettings,
    style: StyleConfig,
    overrides: Overrides,
    field_rules: crate::rules::FieldRules,
    /// Carried through to [`LoweredSchema::build_types`], where the
    /// client emitter (when enabled) reads `Spec::operations` /
    /// `Spec::security_schemes` / `Spec::servers`.
    spec_model: Spec,
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

    /// The resolved [`StyleConfig`] driving this run.
    pub fn style(&self) -> &StyleConfig {
        &self.style
    }

    /// The typify settings, as left by the resolved style and
    /// [`customize`](crate::Generator::customize) hooks.
    pub fn settings(&self) -> &TypeSpaceSettings {
        &self.settings
    }

    /// Mutable access to the typify settings — every knob of the fork is
    /// reachable here, after the style has been applied.
    pub fn settings_mut(&mut self) -> &mut TypeSpaceSettings {
        &mut self.settings
    }

    /// Run typify: build a [`TypeSpace`] from the settings and populate it
    /// from the lowered schema.
    pub fn build_types(mut self) -> Result<GeneratedTypes> {
        // Rule-tier deep-patch and type-level patch decisions feed
        // typify's generation-time filter (the load-time install
        // predates rules resolution); re-install it augmented only
        // when rules force something, preserving any `customize`-hook
        // filter otherwise.
        if !self.field_rules.deep_patch_overrides().is_empty()
            || !self.field_rules.patch_overrides().is_empty()
        {
            self.settings.with_deep_patch_filter(
                self.overrides
                    .deep_patch_filter_with_rules(self.field_rules.deep_patch_overrides().clone()),
            );
        }
        let mut type_space = TypeSpace::new(&self.settings);
        type_space
            .add_root_schema(self.schema)
            .context("typify type generation failed")?;
        // Client-plan resolution happens here — the one point where the
        // typed spec model, the resolved style, the (possibly
        // user-edited) partition, and the populated TypeSpace all
        // exist — so every v1 client boundary fails loudly before any
        // output is rendered.
        let client_plan = if self.style.client.enabled {
            Some(
                crate::client::ClientPlan::resolve(
                    &self.spec_model,
                    &self.style,
                    self.partition.as_ref(),
                    &type_space,
                )
                .context("client generation failed")?,
            )
        } else {
            None
        };
        Ok(GeneratedTypes {
            type_space,
            partition: self.partition,
            style: self.style,
            overrides: self.overrides,
            field_rules: self.field_rules,
            client_plan,
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
    style: StyleConfig,
    overrides: Overrides,
    field_rules: crate::rules::FieldRules,
    client_plan: Option<crate::client::ClientPlan>,
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
    /// here, so earlier [`LoweredSchema::partition_mut`] edits and
    /// `[types.*] module` overrides apply) and includes the style's
    /// trait imports, but no post-processing: that only happens in
    /// [`Self::into_file`]. With the client enabled, the generated
    /// `pub mod client` follows the type modules.
    pub fn tokens(&self) -> TokenStream {
        let trait_imports = parse_imports(&self.style.imports);
        let types = match &self.partition {
            Some(partition) => {
                let rust_partition =
                    resolved_rust_partition(partition, &self.type_space, &self.style);
                let imports = partition.module_imports(&trait_imports);
                self.type_space.to_stream_partitioned(
                    &rust_partition,
                    partition.default_module(),
                    &imports,
                )
            }
            None => {
                let body = self.type_space.to_stream();
                quote! {
                    #trait_imports
                    #body
                }
            }
        };
        match &self.client_plan {
            Some(plan) => {
                let client = crate::client::client_tokens(plan);
                quote! {
                    #types
                    #client
                }
            }
            None => types,
        }
    }

    /// Parse the generated tokens into a [`syn::File`] and apply the
    /// style's post-processing: per-type/per-field overrides and patch
    /// stripping (with selector validation), `impl Default` synthesis
    /// for enums typify can't default (under
    /// [`untagged_enum_defaults`](StyleConfig::untagged_enum_defaults)),
    /// and the condensed emit style (under
    /// [`emit_style`](StyleConfig::emit_style)). Edit the returned AST
    /// freely, then finish with [`render_file`]. To skip the
    /// post-processing, parse [`Self::tokens`] yourself instead.
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
    /// generator's spec path — plus, when the client's `ext` module is
    /// enabled, a `pub mod ext;` declaration in the root `mod.rs` and a
    /// write-once `ext/mod.rs` scaffold. This is the staged-pipeline
    /// counterpart of
    /// [`Generator::generate_to_dir`](crate::Generator::generate_to_dir).
    pub fn render_to_dir(self, dir: impl AsRef<Path>) -> Result<()> {
        let dir = dir.as_ref();
        let (file, ext, spec_path) = self.tree_parts()?;
        crate::tree::write_file_tree(&file, &spec_path, dir)?;
        if let Some(contents) = ext {
            crate::tree::write_ext_scaffold(dir, &contents)?;
        }
        Ok(())
    }

    /// The directory-tree output artifacts: the post-processed AST —
    /// with a `pub mod ext;` declaration appended when the client's
    /// user-owned ext module is enabled — plus the `ext/mod.rs`
    /// scaffold contents (written once, only if absent) and the spec
    /// path for headers.
    pub(crate) fn tree_parts(self) -> Result<(syn::File, Option<String>, PathBuf)> {
        // The ext module only exists in tree output: a single rendered
        // file has no directory to host user-owned files, so
        // single-file mode never declares `pub mod ext;`.
        let ext = (self.client_plan.is_some() && self.style.client.ext_module)
            .then(|| crate::client::ext_scaffold(&self.spec_path));
        let spec_path = self.spec_path.clone();
        let mut file = self.build_file()?;
        if ext.is_some() {
            file.items.push(syn::parse_quote! {
                /// User-owned extensions; scaffolded once, never regenerated.
                pub mod ext;
            });
        }
        Ok((file, ext, spec_path))
    }

    /// The resolved `[verify]` configuration of this run.
    pub(crate) fn verify_config(&self) -> &crate::config::VerifyConfig {
        &self.style.verify
    }

    fn build_file(&self) -> Result<syn::File> {
        let mut file = syn::parse_file(&self.tokens().to_string())
            .context("generated tokens failed to parse as a Rust file")?;
        self.overrides.apply_to_file(&mut file)?;
        // The per-field decision layer: `[style.formats]` / `replace`
        // mapping defaults, overridden by `[[rules]]` in order, then
        // the `[fields]` tier — materialized once and applied by the
        // mappings machinery (attrs, rule type overrides, capability
        // pruning); enums whose Default synthesis would not compile
        // are skipped below.
        let plans = self.field_rules.field_plans(&file, &self.style)?;
        let mappings = crate::mappings::Mappings::resolve(&self.style)?;
        let skip_defaults =
            mappings.apply_to_file(&mut file, self.style.untagged_enum_defaults, &plans)?;
        if self.style.untagged_enum_defaults {
            postprocess::synthesize_enum_defaults(&mut file, &skip_defaults);
        }
        if self.style.emit_style == EmitStyle::Condensed {
            condense::condense_file(&mut file)?;
        }
        Ok(file)
    }
}

/// The final Rust-type-name → module map for partitioned output:
/// [`Partition::to_rust_partition`] (where split-mode simple-enum
/// routing resolves) with `[types."Name"] module = "..."` overrides
/// applied on top. Shared by [`GeneratedTypes::tokens`] and the client
/// plan's type-reference resolution so both see identical placement.
/// Selector existence is validated in [`GeneratedTypes::into_file`].
pub(crate) fn resolved_rust_partition(
    partition: &Partition,
    type_space: &TypeSpace,
    style: &StyleConfig,
) -> std::collections::HashMap<String, String> {
    let mut rust_partition = partition.to_rust_partition(type_space);
    for (selector, override_) in &style.types {
        if let Some(module) = &override_.module {
            rust_partition.insert(typify::rust_type_ident(selector), module.clone());
        }
    }
    rust_partition
}

/// Parse the style's `use ...;` statements into one preamble stream.
/// Invalid statements are a programming error in the style data; they
/// fail loudly at generation time.
fn parse_imports(imports: &[String]) -> TokenStream {
    imports
        .iter()
        .map(|statement| {
            statement
                .parse::<TokenStream>()
                .unwrap_or_else(|error| panic!("style import {statement:?} failed to parse: {error}"))
        })
        .collect()
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
///
/// (Two token-preserving text fix-ups run over the prettyplease output:
/// output containing the condensed style's `impl_string_enum` macro gets
/// its macro definition and invocations re-formatted readably — a no-op
/// for everything else — and adjacent items are separated with a blank
/// line. See `condense::polish_rendered` and `render::space_rendered`.)
pub fn render_file(file: &syn::File, spec_path: impl AsRef<Path>) -> String {
    let body = crate::render::render_body(file);
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

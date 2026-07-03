//! The end-to-end pipeline: load → patch → partition → lower → engine
//! (typify or IR) → format → write.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::Value;
use typify::TypeSpaceSettings;

use crate::config::StyleConfig;
use crate::partition::Partition;
use crate::pipeline::LoadedSpec;
use crate::profile::StyleProfile;
use crate::spec::Spec;
use crate::{Result, ir, load};

/// A registered [`Generator::customize`] hook.
type SettingsHook = Box<dyn Fn(&mut TypeSpaceSettings)>;
/// A registered [`Generator::patch_spec_with`] hook.
type SpecHook = Box<dyn Fn(&mut Value)>;
/// A registered [`Generator::style`] hook.
type StyleHook = Box<dyn Fn(&mut StyleConfig)>;

/// Which generation engine runs the back half of the pipeline.
///
/// Both engines share loading, patching, and partitioning; they diverge
/// after the typed [`Spec`](crate::spec::Spec) model is built.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Engine {
    /// The typify-fork engine (the default): `Spec` rendered to draft-07
    /// JSON Schema, compiled by the frozen local typify fork, styled via
    /// [`typify::TypeSpaceSettings`] knobs.
    #[default]
    Typify,
    /// The owned IR engine (migration step 2, docs/MIGRATION.md):
    /// `Spec → Ir → ordered passes → emitter`, styled via
    /// [`StyleConfig`] data. Opt-in until the parity gate flips the
    /// default. Supports the `api-client` profile; the `typify` profile
    /// *means* the typify engine and is rejected here (decision D3).
    Ir,
}

/// Builder for a single codegen run. See the crate docs for an example.
pub struct Generator {
    spec_path: PathBuf,
    patches_dir: Option<PathBuf>,
    profile: StyleProfile,
    engine: Engine,
    config_file: Option<PathBuf>,
    partition_by_operation: bool,
    split_request_response: bool,
    customize: Vec<SettingsHook>,
    patch_spec: Vec<SpecHook>,
    style_hooks: Vec<StyleHook>,
    ir_passes: Vec<Box<dyn ir::passes::Pass>>,
}

impl Generator {
    /// Start a run for the OpenAPI document at `spec_path` (YAML or JSON).
    pub fn new(spec_path: impl Into<PathBuf>) -> Self {
        Self {
            spec_path: spec_path.into(),
            patches_dir: None,
            profile: StyleProfile::default(),
            engine: Engine::default(),
            config_file: None,
            partition_by_operation: false,
            split_request_response: false,
            customize: Vec::new(),
            patch_spec: Vec::new(),
            style_hooks: Vec::new(),
            ir_passes: Vec::new(),
        }
    }

    /// Select the generation engine (default: [`Engine::Typify`]).
    pub fn engine(mut self, engine: Engine) -> Self {
        self.engine = engine;
        self
    }

    /// Load a `codegen.toml` over the profile's preset (IR engine only).
    /// See [`StyleConfig::from_toml_file`] for the merge rules.
    pub fn config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Tweak the IR engine's [`StyleConfig`] after the profile preset
    /// and any [`Self::config_file`] have been applied. May be called
    /// multiple times; hooks run in registration order. This is the IR
    /// engine's code escape hatch, the counterpart of
    /// [`Self::customize`].
    pub fn style(mut self, hook: impl Fn(&mut StyleConfig) + 'static) -> Self {
        self.style_hooks.push(Box::new(hook));
        self
    }

    /// Append a custom IR pass after the built-in pipeline (IR engine
    /// only).
    pub fn ir_pass(mut self, pass: impl ir::passes::Pass + 'static) -> Self {
        self.ir_passes.push(Box::new(pass));
        self
    }

    /// Apply every RFC 6902 patch file under `dir` (lexicographic order)
    /// to the parsed spec before any other processing. See
    /// [`crate::apply_patches_dir`] for the file format.
    pub fn patches_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.patches_dir = Some(dir.into());
        self
    }

    /// Select the style profile (default: [`StyleProfile::Typify`]).
    pub fn profile(mut self, profile: StyleProfile) -> Self {
        self.profile = profile;
        self
    }

    /// When `true`, group generated types into one `pub mod
    /// <snake_operation_id>` block per OpenAPI operation, with types
    /// reachable from several operations in `pub mod shared`. When `false`
    /// (the default) everything is emitted as a flat stream of items.
    pub fn partition_by_operation(mut self, enabled: bool) -> Self {
        self.partition_by_operation = enabled;
        self
    }

    /// When `true`, partition each operation's types further into
    /// `request` / `response` submodules (`pub mod <op> { pub mod request
    /// { ... } pub mod response { ... } }`), with cross-operation types
    /// classified into `shared::{request, response, enums, common}` —
    /// see [`crate::Partition::compute_split`] for the exact policy.
    /// Implies [`Self::partition_by_operation`]. Combine with
    /// [`Self::generate_to_dir`] to write the module tree as a directory
    /// of files.
    pub fn split_request_response(mut self, enabled: bool) -> Self {
        self.split_request_response = enabled;
        self
    }

    /// Tweak the [`TypeSpaceSettings`] after the profile has been applied.
    /// May be called multiple times; hooks run in registration order. This
    /// is the granular escape hatch — every knob of the typify fork is
    /// reachable here.
    pub fn customize(mut self, hook: impl Fn(&mut TypeSpaceSettings) + 'static) -> Self {
        self.customize.push(Box::new(hook));
        self
    }

    /// Mutate the parsed spec in Rust, after file patches but before any
    /// lowering. The escape hatch for edits RFC 6902 can't express cleanly
    /// (e.g. renaming a schema and rewriting every `$ref` to it).
    pub fn patch_spec_with(mut self, hook: impl Fn(&mut Value) + 'static) -> Self {
        self.patch_spec.push(Box::new(hook));
        self
    }

    /// Start the staged pipeline: parse the spec and apply patch files and
    /// [`Self::patch_spec_with`] hooks, then stop. The returned
    /// [`LoadedSpec`] exposes the parsed document for arbitrary edits and
    /// continues via [`LoadedSpec::lower`] →
    /// [`LoweredSchema::build_types`](crate::LoweredSchema::build_types) →
    /// [`GeneratedTypes::into_file`](crate::GeneratedTypes::into_file) /
    /// [`render`](crate::GeneratedTypes::render), with every intermediate
    /// artifact (spec, partition, settings, type space, AST) open for
    /// inspection and mutation between stages. See [`LoadedSpec`] for the
    /// stage-by-stage walkthrough.
    pub fn load(&self) -> Result<LoadedSpec> {
        let mut spec = load::load_spec(&self.spec_path)?;

        if let Some(dir) = &self.patches_dir {
            load::apply_patches_dir(&mut spec, dir)?;
        }
        for hook in &self.patch_spec {
            hook(&mut spec);
        }

        let mut settings = TypeSpaceSettings::default();
        self.profile.apply(&mut settings);
        for hook in &self.customize {
            hook(&mut settings);
        }

        Ok(LoadedSpec {
            spec,
            settings,
            profile: self.profile,
            partition_by_operation: self.partition_by_operation,
            split_request_response: self.split_request_response,
            spec_path: self.spec_path.clone(),
        })
    }

    /// Run the pipeline and return formatted Rust source.
    ///
    /// On the typify engine this is equivalent to driving the staged
    /// pipeline straight through with no between-stage customization.
    pub fn generate_to_string(&self) -> Result<String> {
        match self.engine {
            Engine::Typify => self.load()?.lower()?.build_types()?.render(),
            Engine::Ir => {
                let file = self.generate_ir_file()?;
                Ok(crate::pipeline::render_file(&file, &self.spec_path))
            }
        }
    }

    /// Run the pipeline and write the result to `path`, creating parent
    /// directories as needed. The write is idempotent: when the file
    /// already holds identical content its bytes and mtime are left
    /// untouched, so downstream builds don't churn.
    pub fn generate_to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let contents = self.generate_to_string()?;
        crate::tree::write_if_changed(path.as_ref(), &contents)
    }

    /// Run the pipeline and write the result as a directory tree rooted
    /// at `dir`: one file per partition module (`<mod>.rs`, or
    /// `<mod>/mod.rs` plus one file per nested partition when
    /// [`Self::split_request_response`] is on) and a root `mod.rs`
    /// declaring them. See [`crate::write_file_tree`] for the exact
    /// splitting, header, idempotency, and stale-file-cleanup rules.
    pub fn generate_to_dir(&self, dir: impl AsRef<Path>) -> Result<()> {
        let file = match self.engine {
            Engine::Typify => self.load()?.lower()?.build_types()?.into_file()?,
            Engine::Ir => self.generate_ir_file()?,
        };
        crate::tree::write_file_tree(&file, &self.spec_path, dir)
    }

    /// Run the pipeline up to the post-processed [`syn::File`] AST —
    /// the artifact [`Self::generate_to_dir`] plans its file tree from.
    /// Works on both engines; on the typify engine it is equivalent to
    /// the staged pipeline's
    /// [`into_file`](crate::GeneratedTypes::into_file).
    pub fn generate_to_syn_file(&self) -> Result<syn::File> {
        match self.engine {
            Engine::Typify => self.load()?.lower()?.build_types()?.into_file(),
            Engine::Ir => self.generate_ir_file(),
        }
    }

    /// The IR engine's back half: `Spec → Ir → passes → syn::File`.
    pub(crate) fn generate_ir_file(&self) -> Result<syn::File> {
        let mut document = load::load_spec(&self.spec_path)?;
        if let Some(dir) = &self.patches_dir {
            load::apply_patches_dir(&mut document, dir)?;
        }
        for hook in &self.patch_spec {
            hook(&mut document);
        }

        let partition = if self.split_request_response {
            Some(Partition::compute_split(&document)?)
        } else if self.partition_by_operation {
            Some(Partition::compute(&document)?)
        } else {
            None
        };

        let style = self.resolved_style()?;
        style.validate_supported()?;

        let spec = Spec::from_value(&document)?;
        let mut lowered = ir::lower_spec(&spec, &style)?;

        let cx = ir::passes::PassCx {
            style: &style,
            partition: partition.as_ref(),
        };
        let pipeline = ir::passes::standard_pipeline();
        ir::passes::run_pipeline(&pipeline, &mut lowered, &cx)?;
        ir::passes::run_pipeline(&self.ir_passes, &mut lowered, &cx)?;

        ir::emit_single_file(&lowered)
    }

    /// The [`StyleConfig`] this run resolves to: profile preset →
    /// `codegen.toml` → [`Self::style`] hooks.
    fn resolved_style(&self) -> Result<StyleConfig> {
        let preset = match self.profile {
            StyleProfile::ApiClient => StyleConfig::api_client(),
            StyleProfile::Typify => bail!(
                "the `typify` profile means \"whatever the typify engine emits\" and is \
                 not supported by the IR engine; use `Engine::Typify` (the default) or \
                 the `api-client` profile (docs/MIGRATION.md, decision D3)",
            ),
        };
        let mut style = match &self.config_file {
            Some(path) => StyleConfig::from_toml_file(path, preset)
                .with_context(|| format!("loading config {}", path.display()))?,
            None => preset,
        };
        for hook in &self.style_hooks {
            hook(&mut style);
        }
        Ok(style)
    }
}

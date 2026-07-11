//! The end-to-end pipeline: load → patch → partition → lower → typify →
//! post-process → format → write.

use std::path::{Path, PathBuf};

use serde_json::Value;
use typify::TypeSpaceSettings;

use crate::config::StyleConfig;
use crate::overrides::Overrides;
use crate::pipeline::LoadedSpec;
use crate::profile::StyleProfile;
use crate::{Result, load};

/// A registered [`Generator::customize`] hook.
type SettingsHook = Box<dyn Fn(&mut TypeSpaceSettings)>;
/// A registered [`Generator::patch_spec_with`] hook.
type SpecHook = Box<dyn Fn(&mut Value)>;
/// A registered [`Generator::style`] hook.
type StyleHook = Box<dyn Fn(&mut StyleConfig)>;

/// Builder for a single codegen run. See the crate docs for an example.
pub struct Generator {
    spec_path: PathBuf,
    patches_dir: Option<PathBuf>,
    profile: StyleProfile,
    config_file: Option<PathBuf>,
    partition_by_operation: bool,
    split_request_response: bool,
    customize: Vec<SettingsHook>,
    patch_spec: Vec<SpecHook>,
    style_hooks: Vec<StyleHook>,
}

impl Generator {
    /// Start a run for the OpenAPI document at `spec_path` (YAML or JSON).
    pub fn new(spec_path: impl Into<PathBuf>) -> Self {
        Self {
            spec_path: spec_path.into(),
            patches_dir: None,
            profile: StyleProfile::default(),
            config_file: None,
            partition_by_operation: false,
            split_request_response: false,
            customize: Vec::new(),
            patch_spec: Vec::new(),
            style_hooks: Vec::new(),
        }
    }

    /// Load a `codegen.toml` over the profile's preset. See
    /// [`StyleConfig::from_toml_file`] for the merge rules.
    pub fn config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Tweak the [`StyleConfig`] after the profile preset and any
    /// [`Self::config_file`] have been applied. May be called multiple
    /// times; hooks run in registration order. This is the programmatic
    /// form of `codegen.toml` — for knobs below the data layer, see
    /// [`Self::customize`].
    pub fn style(mut self, hook: impl Fn(&mut StyleConfig) + 'static) -> Self {
        self.style_hooks.push(Box::new(hook));
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

    /// Tweak the [`TypeSpaceSettings`] after the resolved style has been
    /// applied. May be called multiple times; hooks run in registration
    /// order. This is the granular escape hatch below the [`StyleConfig`]
    /// data layer — every knob of the typify fork is reachable here.
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

    /// Gate generation on the output compiling: after rendering (and
    /// before any file is written), `cargo check` the output in a
    /// scratch crate and fail with the compiler output on error. The
    /// programmatic form of the `[verify]` config table — extra scratch
    /// dependencies come from `[verify] dependencies`. See
    /// [`crate::verify`] for the mechanism and its requirements.
    pub fn verify_compile(self, enabled: bool) -> Self {
        self.style(move |style| style.verify.enabled = enabled)
    }

    /// Generate the API client alongside the types: a `client` module
    /// with a concrete `reqwest_middleware`-based `Client`, auth
    /// providers from `securitySchemes`, and — in directory-tree
    /// output — the user-owned `ext` module. The programmatic form of
    /// the `[client]` config table (`enabled` here; the other keys via
    /// [`Self::style`]). See [`crate::client`].
    pub fn client(self, enabled: bool) -> Self {
        self.style(move |style| style.client.enabled = enabled)
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

        let style = self.resolved_style()?;
        let overrides = Overrides::resolve(&style)?;

        let mut settings = TypeSpaceSettings::default();
        style.apply_to_settings(&mut settings);
        for hook in &self.customize {
            hook(&mut settings);
        }

        Ok(LoadedSpec {
            spec,
            settings,
            style,
            overrides,
            partition_by_operation: self.partition_by_operation,
            split_request_response: self.split_request_response,
            spec_path: self.spec_path.clone(),
        })
    }

    /// Run the pipeline and return formatted Rust source.
    ///
    /// Equivalent to driving the staged pipeline straight through with no
    /// between-stage customization.
    pub fn generate_to_string(&self) -> Result<String> {
        let stage = self.load()?.lower()?.build_types()?;
        let verify = stage.verify_config().clone();
        let source = stage.render()?;
        if verify.enabled {
            crate::verify::verify_single_file(&source, &verify)?;
        }
        Ok(source)
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
    /// splitting, header, idempotency, ejection, and stale-file-cleanup
    /// rules. With the client's `ext` module enabled, the root `mod.rs`
    /// declares `pub mod ext;` and a marker-less `ext/mod.rs` scaffold
    /// is written once (never overwritten).
    pub fn generate_to_dir(&self, dir: impl AsRef<Path>) -> Result<()> {
        let stage = self.load()?.lower()?.build_types()?;
        let verify = stage.verify_config().clone();
        let (file, ext, _) = stage.tree_parts()?;
        if verify.enabled {
            let mut planned = crate::tree::plan_file_tree(&file, &self.spec_path);
            if let Some(contents) = &ext {
                // The scratch crate needs an ext module to satisfy the
                // root's `pub mod ext;`. Mount the user's real ext/ when
                // one exists — generated code may reference helpers living
                // there (config field-attrs naming `...::ext::...` paths),
                // so the gate must compile against what the user actually
                // wrote. A fresh tree falls back to the pristine scaffold.
                let user_ext = crate::tree::plan_ext_dir(dir.as_ref());
                if user_ext.is_empty() {
                    planned.insert(PathBuf::from("ext/mod.rs"), contents.clone());
                } else {
                    planned.extend(user_ext);
                }
            }
            crate::verify::verify_tree(&planned, &verify)?;
        }
        crate::tree::write_file_tree(&file, &self.spec_path, &dir)?;
        if let Some(contents) = ext {
            crate::tree::write_ext_scaffold(dir.as_ref(), &contents)?;
        }
        Ok(())
    }

    /// Run the pipeline up to the post-processed [`syn::File`] AST —
    /// the artifact [`Self::generate_to_dir`] plans its file tree from.
    pub fn generate_to_syn_file(&self) -> Result<syn::File> {
        self.load()?.lower()?.build_types()?.into_file()
    }

    /// The [`StyleConfig`] this run resolves to: profile preset →
    /// `codegen.toml` → [`Self::style`] hooks.
    fn resolved_style(&self) -> Result<StyleConfig> {
        let preset = self.profile.preset();
        let mut style = match &self.config_file {
            Some(path) => StyleConfig::from_toml_file(path, preset)?,
            None => preset,
        };
        for hook in &self.style_hooks {
            hook(&mut style);
        }
        Ok(style)
    }
}

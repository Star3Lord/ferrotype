//! Style as data: the configuration consumed by the IR engine's passes.
//!
//! A [`StyleConfig`] is the declarative form of a style profile. The
//! built-in presets ([`StyleConfig::api_client`]) reproduce the
//! [`crate::StyleProfile`] knob recipes as data; a `codegen.toml` file
//! ([`StyleConfig::from_toml_str`]) can override any of it, plus add
//! per-type and per-field overrides. The standing rule (R3 in
//! ARCHITECTURE.md): every key here is consumed by exactly one named pass
//! — the doc comment on each field names it.
//!
//! The typify engine does not read this; its style surface remains
//! [`typify::TypeSpaceSettings`] via [`crate::StyleProfile::apply`].

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::Result;

/// Which generated type kinds an attribute/derive entry applies to.
/// (`newtypes` is accepted for config-compat but nothing emits newtype
/// shapes yet; see docs/MIGRATION.md D5.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KindFilter {
    All,
    Structs,
    Enums,
    Newtypes,
}

impl KindFilter {
    pub(crate) fn matches_struct(self) -> bool {
        matches!(self, KindFilter::All | KindFilter::Structs)
    }
    pub(crate) fn matches_enum(self) -> bool {
        matches!(self, KindFilter::All | KindFilter::Enums)
    }
}

/// Position of an attribute relative to the main `#[derive(...)]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttrPosition {
    #[default]
    BeforeDerive,
    AfterDerive,
}

/// An unconditional attribute line. Consumed by `DeriveAttrPass`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttrEntry {
    /// The attribute body, without `#[...]`, e.g.
    /// `serde_with::skip_serializing_none` or
    /// `patch(attribute(serde(default)))`.
    pub attr: String,
    #[serde(default)]
    pub position: AttrPosition,
    #[serde(default = "kind_all")]
    pub kinds: KindFilter,
}

/// A `#[cfg_attr(feature = <feature>, derive(<derive>))]` line.
/// Consumed by `DeriveAttrPass`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondDeriveEntry {
    pub feature: String,
    pub derive: String,
    #[serde(default = "kind_all")]
    pub kinds: KindFilter,
}

/// A `#[cfg_attr(feature = <feature>, <attr>)]` line. Consumed by
/// `DeriveAttrPass`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondAttrEntry {
    pub feature: String,
    pub attr: String,
    #[serde(default)]
    pub position: AttrPosition,
    #[serde(default = "kind_all")]
    pub kinds: KindFilter,
}

fn kind_all() -> KindFilter {
    KindFilter::All
}

/// When is a non-required field `Option<T>`? Consumed by
/// `OptionalityPass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OptionalFields {
    /// Every non-required field is `Option<T>` — schema defaults do not
    /// collapse the wrapper (the house style).
    #[default]
    AlwaysOption,
    /// Typify-flavored bare shapes (`Vec<T>` + `serde(default)`,
    /// defaulted scalars bare with `defaults::` helper fns) — requires
    /// helper-fn synthesis the IR emitter does not implement yet.
    /// Selecting it is a loud error (docs/MIGRATION.md D4).
    Bare,
}

/// What to do with string/integer constraints. Consumed by the lowering
/// (`ir::lower`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConstraintMode {
    /// Ignore constraints: plain `String` / plain integers.
    #[default]
    Plain,
    /// Validating newtypes (upstream typify's default) — not implemented
    /// by the IR engine (docs/MIGRATION.md D3/D4); loud error.
    Validate,
}

/// `allOf` handling. Consumed by the lowering (`ir::lower`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AllOfMode {
    /// `$ref` bases become `#[serde(flatten)]` fields (single-inheritance
    /// composition); falls back to merge when the shape doesn't compose.
    #[default]
    Compose,
    /// Merge subschema properties into one flat struct.
    Merge,
}

/// Enum `Default` synthesis. Consumed by `ImplSynthPass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnumDefaultMode {
    /// Enums without a schema-level default get `impl Default` selecting
    /// the first unit variant; schema-level defaults are always honored.
    #[default]
    FirstUnitVariant,
    /// Only schema-level defaults produce `impl Default`.
    SchemaOnly,
}

/// Deep-patch annotation policy. Consumed by `DeepPatchPass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeepPatchMode {
    /// Annotate every `Option<Struct>` (incl. `Option<Box<Struct>>`)
    /// field with `#[patch(name = "Option<InnerPatch>")]`.
    #[default]
    AllOptionStructs,
    Off,
}

/// Ordered derive lists per type kind. Consumed by `DeriveAttrPass`.
/// An empty list means "upstream base set" (`::serde::Serialize`,
/// `::serde::Deserialize`, `Debug`, `Clone`, lexicographically sorted).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct DeriveLists {
    #[serde(default)]
    pub structs: Vec<String>,
    #[serde(default)]
    pub enums: Vec<String>,
    #[serde(default)]
    pub newtypes: Vec<String>,
}

/// Per-type override, keyed by schema name. Consumed by
/// `DeriveAttrPass` (`derives_add`), `PartitionPass` (`module`), and
/// `PatchabilityPass` (`patch`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TypeOverride {
    /// Extra derives appended after the profile's ordered list.
    #[serde(default)]
    pub derives_add: Vec<String>,
    /// Force the type into this module (slash-separated path).
    pub module: Option<String>,
    /// Override the style-level [`StyleConfig::patch`] baseline for this
    /// type: `false` strips the type's `struct_patch` machinery, `true`
    /// re-enables it under a global `patch = false`. Struct types only —
    /// targeting anything else is a hard error.
    pub patch: Option<bool>,
}

/// Per-field override, keyed by `Type.field` (schema name + wire name).
/// Consumed by `DeepPatchPass` (`deep_patch`) and `TypeOverridePass`
/// (`type`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct FieldOverride {
    pub deep_patch: Option<bool>,
    /// Replace the field's Rust type with this path, verbatim.
    #[serde(rename = "type")]
    pub type_path: Option<String>,
}

/// The declarative style configuration driving the IR engine.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct StyleConfig {
    /// `OptionalityPass`.
    pub optional_fields: OptionalFields,
    /// `ir::lower`: constrained strings.
    pub constrained_strings: ConstraintMode,
    /// `ir::lower`: integer bounds (`NonZero*` mapping upstream).
    pub integers: ConstraintMode,
    /// `ir::lower`: Rust type for `format: date` strings.
    pub date: Option<String>,
    /// `ir::lower`: Rust type for `format: date-time` strings.
    pub date_time: Option<String>,
    /// `ir::lower`: Rust type for `format: uuid` strings.
    pub uuid: Option<String>,
    /// `SerdeSurfacePass`: struct-level `rename_all` case, with
    /// covered per-field renames elided.
    pub rename_all: Option<String>,
    /// `ir::lower`.
    pub allof: AllOfMode,
    /// `ImplSynthPass`.
    pub enum_default: EnumDefaultMode,
    /// `SerdeSurfacePass`: drop the `default` +
    /// `skip_serializing_if = "Option::is_none"` pair on `Option<T>`
    /// fields (a struct-level `skip_serializing_none` attr is assumed).
    pub elide_option_defaults: bool,
    /// `PatchabilityPass`: the `struct_patch` baseline. `true` (the
    /// default) lets structs carry whatever patch machinery the style
    /// data declares (the `Patch` derive and `patch(...)` attrs in
    /// [`Self::derives`]/[`Self::attrs`]/[`Self::conditional_attrs`],
    /// plus `DeepPatchPass` annotations); `false` strips all of it —
    /// per-type `[types."Name"] patch = true|false` overrides the
    /// baseline either way. See docs/MIGRATION.md D13.
    pub patch: bool,
    /// `DeepPatchPass`.
    pub deep_patch: DeepPatchMode,
    /// `ImplSynthPass`: synthesize `impl Default` for untagged enums
    /// with no unit variant (the `postprocess.rs` behavior).
    pub untagged_enum_defaults: bool,
    /// `DeriveAttrPass`.
    pub derives: DeriveLists,
    /// `DeriveAttrPass`.
    #[serde(default)]
    pub attrs: Vec<AttrEntry>,
    /// `DeriveAttrPass`.
    #[serde(default)]
    pub conditional_derives: Vec<CondDeriveEntry>,
    /// `DeriveAttrPass`.
    #[serde(default)]
    pub conditional_attrs: Vec<CondAttrEntry>,
    /// `ImportsPass`: full `use ...;` statements injected at the top of
    /// every generated module so bare derive paths resolve.
    #[serde(default)]
    pub imports: Vec<String>,
    /// Per-type overrides, keyed by schema name. Unmatched keys are
    /// hard errors at generation time.
    #[serde(default)]
    pub types: BTreeMap<String, TypeOverride>,
    /// Per-field overrides, keyed by `SchemaName.wireName`. Unmatched
    /// keys are hard errors at generation time.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldOverride>,
}

impl Default for StyleConfig {
    /// The plain baseline: upstream-ish shapes with no house styling.
    /// (Presets and `codegen.toml` build on top of this.)
    fn default() -> Self {
        StyleConfig {
            optional_fields: OptionalFields::AlwaysOption,
            constrained_strings: ConstraintMode::Plain,
            integers: ConstraintMode::Plain,
            date: None,
            date_time: None,
            uuid: None,
            rename_all: None,
            allof: AllOfMode::Compose,
            enum_default: EnumDefaultMode::SchemaOnly,
            elide_option_defaults: false,
            patch: true,
            deep_patch: DeepPatchMode::Off,
            untagged_enum_defaults: false,
            derives: DeriveLists::default(),
            attrs: Vec::new(),
            conditional_derives: Vec::new(),
            conditional_attrs: Vec::new(),
            imports: vec!["use ::serde::{Deserialize, Serialize};".to_string()],
            types: BTreeMap::new(),
            fields: BTreeMap::new(),
        }
    }
}

impl StyleConfig {
    /// The `api-client` preset: the data form of
    /// [`crate::StyleProfile::ApiClient`] (see `profile.rs` for the
    /// knob-based original this reproduces).
    pub fn api_client() -> Self {
        let struct_derives = [
            "Debug",
            "Clone",
            "Default",
            "PartialEq",
            "Serialize",
            "Deserialize",
            "Patch",
        ];
        let enum_derives = ["Debug", "Clone", "PartialEq", "Serialize", "Deserialize"];

        StyleConfig {
            optional_fields: OptionalFields::AlwaysOption,
            constrained_strings: ConstraintMode::Plain,
            integers: ConstraintMode::Plain,
            date: Some("::std::string::String".to_string()),
            date_time: Some("::std::string::String".to_string()),
            uuid: Some("::std::string::String".to_string()),
            rename_all: Some("camelCase".to_string()),
            allof: AllOfMode::Compose,
            enum_default: EnumDefaultMode::FirstUnitVariant,
            elide_option_defaults: true,
            patch: true,
            deep_patch: DeepPatchMode::AllOptionStructs,
            untagged_enum_defaults: true,
            derives: DeriveLists {
                structs: struct_derives.iter().map(|s| s.to_string()).collect(),
                enums: enum_derives.iter().map(|s| s.to_string()).collect(),
                newtypes: Vec::new(),
            },
            attrs: vec![
                AttrEntry {
                    attr: "serde_with::skip_serializing_none".to_string(),
                    position: AttrPosition::BeforeDerive,
                    kinds: KindFilter::Structs,
                },
                AttrEntry {
                    attr: "patch(attribute(serde_with::skip_serializing_none))".to_string(),
                    position: AttrPosition::AfterDerive,
                    kinds: KindFilter::Structs,
                },
                AttrEntry {
                    attr: "patch(attribute(derive(Debug, Clone, Default, PartialEq, \
                           Serialize, Deserialize)))"
                        .to_string(),
                    position: AttrPosition::AfterDerive,
                    kinds: KindFilter::Structs,
                },
                AttrEntry {
                    attr: "patch(attribute(serde(default, rename_all = \"camelCase\")))"
                        .to_string(),
                    position: AttrPosition::AfterDerive,
                    kinds: KindFilter::Structs,
                },
            ],
            conditional_derives: vec![CondDeriveEntry {
                feature: "schemars".to_string(),
                derive: "schemars::JsonSchema".to_string(),
                kinds: KindFilter::All,
            }],
            conditional_attrs: vec![CondAttrEntry {
                feature: "schemars".to_string(),
                attr: "patch(attribute(derive(schemars::JsonSchema)))".to_string(),
                position: AttrPosition::AfterDerive,
                kinds: KindFilter::Structs,
            }],
            imports: vec![
                "use ::serde::{Deserialize, Serialize};".to_string(),
                "use ::struct_patch::Patch;".to_string(),
            ],
            types: BTreeMap::new(),
            fields: BTreeMap::new(),
        }
    }

    /// Parse a `codegen.toml` document. The file overrides the given
    /// base preset key-by-key: scalar keys replace, list/table keys
    /// replace wholesale when present (no per-element merging), and
    /// `[types]`/`[fields]` tables extend the preset's.
    pub fn from_toml_str(raw: &str, base: StyleConfig) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "kebab-case")]
        struct ConfigFile {
            /// Named preset merged underneath the `[style]` table:
            /// `api-client` or `plain`.
            profile: Option<String>,
            style: Option<toml::Table>,
            #[serde(default)]
            types: BTreeMap<String, TypeOverride>,
            #[serde(default)]
            fields: BTreeMap<String, FieldOverride>,
        }

        let parsed: ConfigFile = toml::from_str(raw).context("failed to parse codegen.toml")?;

        let mut config = match parsed.profile.as_deref() {
            Some("api-client") => StyleConfig::api_client(),
            Some("plain") => StyleConfig::default(),
            Some(other) => bail!("unknown profile {other:?} in codegen.toml"),
            None => base,
        };

        if let Some(style) = parsed.style {
            // Deserialize the [style] table over the preset: serialize
            // the preset's fields that the table doesn't mention would
            // require per-key merging; instead we deserialize the table
            // into a full StyleConfig using the preset as serde defaults
            // via a manual merge of present keys.
            config = merge_style_table(config, style)?;
        }
        config.types.extend(parsed.types);
        config.fields.extend(parsed.fields);
        Ok(config)
    }

    /// Load a `codegen.toml` from disk over `base`.
    pub fn from_toml_file(path: &std::path::Path, base: StyleConfig) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        Self::from_toml_str(&raw, base).with_context(|| format!("in {}", path.display()))
    }

    /// Validate mode combinations the IR engine does not implement yet
    /// (docs/MIGRATION.md D4). Called once by the engine before lowering.
    pub(crate) fn validate_supported(&self) -> Result<()> {
        if self.optional_fields == OptionalFields::Bare {
            bail!(
                "style key `optional-fields = \"bare\"` requires defaults-helper \
                 synthesis the IR engine does not implement yet; use the typify \
                 engine for bare-field shapes (docs/MIGRATION.md D4)",
            );
        }
        for (key, mode) in [
            ("constrained-strings", self.constrained_strings),
            ("integers", self.integers),
        ] {
            if mode == ConstraintMode::Validate {
                bail!(
                    "style key `{key} = \"validate\"` (validating newtypes) is not \
                     implemented by the IR engine; use the typify engine \
                     (docs/MIGRATION.md D3/D4)",
                );
            }
        }
        Ok(())
    }
}

/// Merge a raw `[style]` TOML table over a preset, key by key. Only keys
/// present in the table change; each is deserialized into the matching
/// typed field. Unknown keys are hard errors.
fn merge_style_table(mut config: StyleConfig, table: toml::Table) -> Result<StyleConfig> {
    for (key, value) in table {
        macro_rules! set {
            ($field:ident) => {{
                config.$field = value
                    .try_into()
                    .with_context(|| format!("invalid value for style key `{key}`"))?;
            }};
        }
        match key.as_str() {
            "optional-fields" => set!(optional_fields),
            "constrained-strings" => set!(constrained_strings),
            "integers" => set!(integers),
            "date" => set!(date),
            "date-time" => set!(date_time),
            "uuid" => set!(uuid),
            "rename-all" => set!(rename_all),
            "allof" => set!(allof),
            "enum-default" => set!(enum_default),
            "elide-option-defaults" => set!(elide_option_defaults),
            "patch" => set!(patch),
            "deep-patch" => set!(deep_patch),
            "untagged-enum-defaults" => set!(untagged_enum_defaults),
            "derives" => set!(derives),
            "attrs" => set!(attrs),
            "conditional-derives" => set!(conditional_derives),
            "conditional-attrs" => set!(conditional_attrs),
            "imports" => set!(imports),
            other => bail!("unknown style key `{other}` in codegen.toml"),
        }
    }
    Ok(config)
}

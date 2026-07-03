//! Style as data: the declarative configuration that drives generation.
//!
//! A [`StyleConfig`] is the data form of a style profile. The built-in
//! presets ([`StyleConfig::api_client`], [`StyleConfig::plain`])
//! reproduce the [`crate::StyleProfile`] recipes; a `codegen.toml` file
//! ([`StyleConfig::from_toml_str`]) can override any of it and add
//! per-type / per-field overrides.
//!
//! Everything here is applied to the typify fork through
//! [`StyleConfig::apply_to_settings`] (the knob mapping is documented on
//! each field) plus a handful of post-generation AST passes for the
//! decisions typify has no knob for (per-type patch stripping, per-field
//! type overrides, condensed emission — see [`crate::postprocess`] and
//! [`crate::condense`]). [`crate::Generator::customize`] remains the
//! escape hatch below this layer.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde::Deserialize;
use typify::TypeSpaceSettings;

use crate::Result;

/// Which generated type kinds an attribute/derive entry applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KindFilter {
    All,
    Structs,
    Enums,
    Newtypes,
}

impl KindFilter {
    fn to_typify(self) -> typify::TypeKindFilter {
        match self {
            KindFilter::All => typify::TypeKindFilter::ALL,
            KindFilter::Structs => typify::TypeKindFilter::STRUCTS,
            KindFilter::Enums => typify::TypeKindFilter::ENUMS,
            KindFilter::Newtypes => typify::TypeKindFilter::NEWTYPES,
        }
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

impl AttrPosition {
    fn to_typify(self) -> typify::AttrPosition {
        match self {
            AttrPosition::BeforeDerive => typify::AttrPosition::BeforeDerive,
            AttrPosition::AfterDerive => typify::AttrPosition::AfterDerive,
        }
    }
}

/// An unconditional attribute line, mapped to
/// [`TypeSpaceSettings::with_unconditional_attr_at`].
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

/// A `#[cfg_attr(feature = <feature>, derive(<derive>))]` line, mapped
/// to [`TypeSpaceSettings::with_conditional_derive_for`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondDeriveEntry {
    pub feature: String,
    pub derive: String,
    #[serde(default = "kind_all")]
    pub kinds: KindFilter,
}

/// A `#[cfg_attr(feature = <feature>, <attr>)]` line, mapped to
/// [`TypeSpaceSettings::with_conditional_attr_at`].
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

/// When is a non-required field `Option<T>`? Mapped to the fork's three
/// optionality knobs (`with_array_optionality`,
/// `with_default_bool_optionality`, `with_defaulted_field_optionality`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OptionalFields {
    /// Upstream typify shapes (the default): non-required arrays stay
    /// `Vec<T>` + `serde(default)`, defaulted scalars stay bare with
    /// `defaults::` helper fns.
    #[default]
    Bare,
    /// Every non-required field is `Option<T>` — schema defaults do not
    /// collapse the wrapper (the house style).
    AlwaysOption,
}

/// What to do with string/integer constraints. Mapped to
/// `with_unconstrained_string` / `with_unconstrained_int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConstraintMode {
    /// Validating newtypes / `NonZero*` mapping (upstream typify's
    /// default).
    #[default]
    Validate,
    /// Ignore constraints: plain `String` / plain integers.
    Plain,
}

/// `allOf` handling. Mapped to `with_allof_strategy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AllOfMode {
    /// Merge subschema properties into one flat struct (upstream
    /// typify's default).
    #[default]
    Merge,
    /// `$ref` bases become `#[serde(flatten)]` fields (single-inheritance
    /// composition); falls back to merge when the shape doesn't compose.
    Compose,
}

/// Enum `Default` synthesis. Mapped to
/// `with_enum_first_variant_default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnumDefaultMode {
    /// Only schema-level defaults produce `impl Default` (upstream).
    #[default]
    SchemaOnly,
    /// Enums without a schema-level default get `impl Default` selecting
    /// the first unit variant; schema-level defaults are always honored.
    FirstUnitVariant,
}

/// Deep-patch annotation policy, resolved together with the `patch`
/// keys into the fork's `with_deep_patch_filter` closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeepPatchMode {
    #[default]
    Off,
    /// Annotate every `Option<Struct>` (incl. `Option<Box<Struct>>`)
    /// field with `#[patch(name = "Option<InnerPatch>")]`.
    AllOptionStructs,
}

/// How mechanical impls and shared helpers are laid out in the output.
/// Consumed by [`crate::condense`] as a post-generation AST pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmitStyle {
    /// Every impl written out per type and an `error` module duplicated
    /// into every partition module — typify's native shape. The default.
    #[default]
    Expanded,
    /// One `support` module per generation unit holding the shared
    /// `error` module and an `impl_string_enum!` macro; each string
    /// enum's conversion-impl ladder becomes a single macro invocation
    /// and per-module `error` mods become one-line re-exports. Same
    /// trait surface, same paths, dramatically shorter files.
    Condensed,
}

/// Ordered derive lists per type kind, mapped to
/// `with_unconditional_derive_for` (insertion order preserved — this is
/// how derive-list ordering is controlled). An empty list means the
/// upstream base set (`::serde::Serialize`, `::serde::Deserialize`,
/// `Debug`, `Clone`, lexicographically sorted).
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

/// A trait the replacement type named by [`TypeOverride::replace`]
/// provides, mapped onto [`typify::TypeSpaceImpl`]. Advisory: it informs
/// typify's impl bookkeeping (consumed by downstream tooling), not the
/// emitted code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplaceImpl {
    FromStr,
    FromStringIrrefutable,
    Display,
    Default,
}

impl ReplaceImpl {
    fn to_typify(self) -> typify::TypeSpaceImpl {
        match self {
            ReplaceImpl::FromStr => typify::TypeSpaceImpl::FromStr,
            ReplaceImpl::FromStringIrrefutable => typify::TypeSpaceImpl::FromStringIrrefutable,
            ReplaceImpl::Display => typify::TypeSpaceImpl::Display,
            ReplaceImpl::Default => typify::TypeSpaceImpl::Default,
        }
    }
}

/// Per-type override, keyed by schema name (the
/// `components.schemas` / `definitions` key; the generated Rust name is
/// accepted too).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TypeOverride {
    /// Extra derives appended after the profile's ordered list (mapped
    /// to the fork's per-type `with_patch` mechanism).
    #[serde(default)]
    pub derives_add: Vec<String>,
    /// Force the type into this module (slash-separated path) in
    /// partitioned output.
    pub module: Option<String>,
    /// Override the style-level [`StyleConfig::patch`] baseline for this
    /// type: `false` strips the type's `struct_patch` machinery, `true`
    /// re-enables it under a global `patch = false`. Struct types only —
    /// targeting anything else is a hard error.
    pub patch: Option<bool>,
    /// Replace this schema's generated type with an existing Rust type,
    /// verbatim (mapped to the fork's `with_replacement`): nothing is
    /// generated for the schema and every reference names the given
    /// path instead. The type must implement `Serialize`/`Deserialize`
    /// for the schema's wire shape. Cannot be combined with `patch` /
    /// `derives-add` / `module` on the same selector — nothing is
    /// generated to patch, derive on, or place.
    pub replace: Option<String>,
    /// Traits the [`Self::replace`] type provides (kebab-case:
    /// `"from-str"`, `"from-string-irrefutable"`, `"display"`,
    /// `"default"`). Empty or omitted means none are assumed. Only
    /// meaningful together with `replace`.
    #[serde(default)]
    pub replace_impls: Vec<ReplaceImpl>,
}

/// Per-field override, keyed by `Type.field` (schema name + wire name;
/// generated Rust names are accepted too).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct FieldOverride {
    /// Force the deep-patch annotation on (`true`) or off (`false`) for
    /// this field, overriding [`StyleConfig::deep_patch`]. Forcing `true`
    /// on a field that cannot carry the annotation (not an
    /// `Option<Struct>`, or the inner type's Patch companion is stripped)
    /// is a hard error.
    pub deep_patch: Option<bool>,
    /// Replace the field's Rust type with this path, verbatim (the
    /// `Option<...>` wrapper, when present, is preserved).
    #[serde(rename = "type")]
    pub type_path: Option<String>,
}

/// The declarative style configuration. Field defaults mean "upstream
/// typify behavior"; [`StyleConfig::api_client`] is the house-style
/// preset.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct StyleConfig {
    /// → optionality knobs (see [`OptionalFields`]).
    pub optional_fields: OptionalFields,
    /// → `with_unconstrained_string`.
    pub constrained_strings: ConstraintMode,
    /// → `with_unconstrained_int`.
    pub integers: ConstraintMode,
    /// → `with_date_type`; `None` keeps upstream's `chrono` mapping.
    pub date: Option<String>,
    /// → `with_date_time_type`.
    pub date_time: Option<String>,
    /// → `with_uuid_type`; `None` keeps upstream's `uuid` mapping.
    pub uuid: Option<String>,
    /// → `with_format_type`: map `"<instance-type>/<format>"` keys
    /// (instance types `string`, `integer`, `number` — the instance
    /// type keeps `"string/int64"` distinct from `"integer/int64"`) to
    /// Rust type paths emitted verbatim, e.g.
    /// `"string/decimal" = "::rust_decimal::Decimal"`. An entry wins
    /// over typify's built-in format handling and over the
    /// [`Self::date`] / [`Self::date_time`] / [`Self::uuid`] sugar keys
    /// for the same format. Mapped types must implement
    /// `Serialize`/`Deserialize` for the wire format. Malformed keys
    /// (no `/`) are hard errors at generation time.
    #[serde(default)]
    pub formats: BTreeMap<String, String>,
    /// → `with_struct_rename_all`: struct-level `rename_all` case, with
    /// covered per-field renames elided.
    pub rename_all: Option<String>,
    /// → `with_allof_strategy`.
    pub allof: AllOfMode,
    /// → `with_enum_first_variant_default`.
    pub enum_default: EnumDefaultMode,
    /// → `with_elide_option_field_defaults`: drop the `default` +
    /// `skip_serializing_if = "Option::is_none"` pair on `Option<T>`
    /// fields (a struct-level `skip_serializing_none` attr is assumed).
    pub elide_option_defaults: bool,
    /// → `with_schema_in_docs`: embed the full JSON Schema `<details>`
    /// block in doc comments (upstream default `true`).
    pub schema_in_docs: bool,
    /// → `with_string_newtype_conveniences`: `AsRef<str>` / `Display` /
    /// `From<&str>` on string newtypes.
    pub string_newtype_conveniences: bool,
    /// The `struct_patch` baseline. `true` (the default) lets structs
    /// carry whatever patch machinery the style data declares (the
    /// `Patch` derive and `patch(...)` attrs in [`Self::derives`] /
    /// [`Self::attrs`] / [`Self::conditional_attrs`], plus deep-patch
    /// annotations); `false` strips all of it — per-type
    /// `[types."Name"] patch = true|false` overrides the baseline either
    /// way. Resolved by [`crate::patch_plan::PatchPlan`].
    pub patch: bool,
    /// Deep-patch annotation policy (see [`DeepPatchMode`]); resolved
    /// into the fork's `with_deep_patch_filter` closure together with
    /// the `patch` keys.
    pub deep_patch: DeepPatchMode,
    /// Output layout: expanded (typify's native shape) or condensed
    /// (macro + shared `support` module); see [`crate::condense`].
    pub emit_style: EmitStyle,
    /// Synthesize `impl Default` for untagged enums with no unit
    /// variant (post-generation AST pass; required whenever structs
    /// derive `Default` and may hold such an enum in a required field).
    pub untagged_enum_defaults: bool,
    /// → `with_unconditional_derive_for`, in order, per kind.
    pub derives: DeriveLists,
    /// → `with_unconditional_attr_at`.
    #[serde(default)]
    pub attrs: Vec<AttrEntry>,
    /// → `with_conditional_derive_for`.
    #[serde(default)]
    pub conditional_derives: Vec<CondDeriveEntry>,
    /// → `with_conditional_attr_at`.
    #[serde(default)]
    pub conditional_attrs: Vec<CondAttrEntry>,
    /// Full `use ...;` statements injected at the top of every generated
    /// module so bare derive paths resolve (the flat-output preamble and
    /// the partition-mode per-module imports).
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
    /// The `plain` baseline: upstream typify behavior, no house styling.
    /// (Presets and `codegen.toml` build on top of this.)
    fn default() -> Self {
        StyleConfig {
            optional_fields: OptionalFields::Bare,
            constrained_strings: ConstraintMode::Validate,
            integers: ConstraintMode::Validate,
            date: None,
            date_time: None,
            uuid: None,
            formats: BTreeMap::new(),
            rename_all: None,
            allof: AllOfMode::Merge,
            enum_default: EnumDefaultMode::SchemaOnly,
            elide_option_defaults: false,
            schema_in_docs: true,
            string_newtype_conveniences: false,
            patch: true,
            deep_patch: DeepPatchMode::Off,
            emit_style: EmitStyle::Expanded,
            untagged_enum_defaults: false,
            derives: DeriveLists::default(),
            attrs: Vec::new(),
            conditional_derives: Vec::new(),
            conditional_attrs: Vec::new(),
            imports: Vec::new(),
            types: BTreeMap::new(),
            fields: BTreeMap::new(),
        }
    }
}

impl StyleConfig {
    /// The `plain` preset: upstream typify output, unchanged. Alias of
    /// [`Default::default`], named for symmetry with
    /// [`Self::api_client`].
    pub fn plain() -> Self {
        Self::default()
    }

    /// The `api-client` preset: the ergonomic hand-written-client shape
    /// (see [`crate::StyleProfile::ApiClient`] for the full description).
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
        // Newtypes come out of typify for named scalar definitions.
        // `Default` and `PartialEq` let structs holding them as required
        // fields derive `Default` / `PartialEq` themselves. Registering
        // any unconditional newtype derive switches typify to the
        // verbatim-order path, so the historical base set is restated.
        let newtype_derives = [
            "Debug",
            "Clone",
            "Default",
            "PartialEq",
            "::serde::Serialize",
            "::serde::Deserialize",
        ];

        StyleConfig {
            optional_fields: OptionalFields::AlwaysOption,
            constrained_strings: ConstraintMode::Plain,
            integers: ConstraintMode::Plain,
            date: Some("::std::string::String".to_string()),
            date_time: Some("::std::string::String".to_string()),
            uuid: Some("::std::string::String".to_string()),
            formats: BTreeMap::new(),
            rename_all: Some("camelCase".to_string()),
            allof: AllOfMode::Compose,
            enum_default: EnumDefaultMode::FirstUnitVariant,
            elide_option_defaults: true,
            schema_in_docs: false,
            string_newtype_conveniences: true,
            patch: true,
            deep_patch: DeepPatchMode::AllOptionStructs,
            emit_style: EmitStyle::Expanded,
            untagged_enum_defaults: true,
            derives: DeriveLists {
                structs: struct_derives.iter().map(|s| s.to_string()).collect(),
                enums: enum_derives.iter().map(|s| s.to_string()).collect(),
                newtypes: newtype_derives.iter().map(|s| s.to_string()).collect(),
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

    /// Apply every typify-knob-backed key to `settings`. The keys typify
    /// has no knob for — the `patch`/`deep-patch` resolution, `emit-style`,
    /// `untagged-enum-defaults`, `imports`, and the `[types]`/`[fields]`
    /// overrides — are consumed by [`crate::patch_plan::PatchPlan`] and
    /// the post-generation passes instead.
    pub fn apply_to_settings(&self, settings: &mut TypeSpaceSettings) {
        if self.optional_fields == OptionalFields::AlwaysOption {
            settings
                .with_array_optionality(typify::ArrayOptionality::OptionalIfNotRequired)
                .with_default_bool_optionality(typify::DefaultBoolOptionality::AlwaysOption)
                .with_defaulted_field_optionality(typify::DefaultedFieldOptionality::AlwaysOption);
        }
        if self.constrained_strings == ConstraintMode::Plain {
            settings.with_unconstrained_string(true);
        }
        if self.integers == ConstraintMode::Plain {
            settings.with_unconstrained_int(true);
        }
        if let Some(date) = &self.date {
            settings.with_date_type(date);
        }
        if let Some(date_time) = &self.date_time {
            settings.with_date_time_type(date_time);
        }
        if let Some(uuid) = &self.uuid {
            settings.with_uuid_type(uuid);
        }
        // Key shape is validated with a clean error in
        // `Overrides::resolve` (always run by `Generator::load`); a
        // malformed key reaching this point is a programming error in
        // directly-supplied style data and fails loudly, like
        // `imports` statements do.
        for (key, rust_type) in &self.formats {
            let (instance_type, format) = key.split_once('/').unwrap_or_else(|| {
                panic!(
                    "style formats key {key:?} must be \"<instance-type>/<format>\", \
                     e.g. \"string/date-time\"",
                )
            });
            settings.with_format_type(instance_type, format, rust_type);
        }
        if let Some(case) = &self.rename_all {
            settings.with_struct_rename_all(case);
        }
        if self.allof == AllOfMode::Compose {
            settings.with_allof_strategy(typify::AllOfStrategy::Compose);
        }
        if self.enum_default == EnumDefaultMode::FirstUnitVariant {
            settings.with_enum_first_variant_default(true);
        }
        if self.elide_option_defaults {
            settings.with_elide_option_field_defaults(true);
        }
        settings.with_schema_in_docs(self.schema_in_docs);
        settings.with_string_newtype_conveniences(self.string_newtype_conveniences);

        for derive in &self.derives.structs {
            settings.with_unconditional_derive_for(derive, typify::TypeKindFilter::STRUCTS);
        }
        for derive in &self.derives.enums {
            settings.with_unconditional_derive_for(derive, typify::TypeKindFilter::ENUMS);
        }
        for derive in &self.derives.newtypes {
            settings.with_unconditional_derive_for(derive, typify::TypeKindFilter::NEWTYPES);
        }
        for entry in &self.attrs {
            settings.with_unconditional_attr_at(
                &entry.attr,
                entry.position.to_typify(),
                entry.kinds.to_typify(),
            );
        }
        for entry in &self.conditional_derives {
            settings.with_conditional_derive_for(
                &entry.feature,
                &entry.derive,
                entry.kinds.to_typify(),
            );
        }
        for entry in &self.conditional_attrs {
            settings.with_conditional_attr_at(
                &entry.feature,
                &entry.attr,
                entry.position.to_typify(),
                entry.kinds.to_typify(),
            );
        }
        for (selector, override_) in &self.types {
            if let Some(replace) = &override_.replace {
                settings.with_replacement(
                    typify::rust_type_ident(selector),
                    replace,
                    override_.replace_impls.iter().map(|impl_| impl_.to_typify()),
                );
            }
            if override_.derives_add.is_empty() {
                continue;
            }
            let mut patch = typify::TypeSpacePatch::default();
            for derive in &override_.derives_add {
                patch.with_derive(derive);
            }
            settings.with_patch(typify::rust_type_ident(selector), &patch);
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
            Some("plain") => StyleConfig::plain(),
            Some(other) => bail!("unknown profile {other:?} in codegen.toml"),
            None => base,
        };

        if let Some(style) = parsed.style {
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
            "formats" => set!(formats),
            "rename-all" => set!(rename_all),
            "allof" => set!(allof),
            "enum-default" => set!(enum_default),
            "elide-option-defaults" => set!(elide_option_defaults),
            "schema-in-docs" => set!(schema_in_docs),
            "string-newtype-conveniences" => set!(string_newtype_conveniences),
            "patch" => set!(patch),
            "deep-patch" => set!(deep_patch),
            "emit-style" => set!(emit_style),
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

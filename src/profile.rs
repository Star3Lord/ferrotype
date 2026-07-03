//! Style profiles: named presets of typify-fork settings.

use proc_macro2::TokenStream;
use quote::quote;
use typify::{
    AllOfStrategy, ArrayOptionality, AttrPosition, DeepPatchPolicy, DefaultBoolOptionality,
    DefaultedFieldOptionality, TypeKindFilter, TypeSpaceSettings,
};

/// A named preset of typify settings. Profiles capture the coarse "what
/// should the generated code look like" decision; per-run tweaks go through
/// [`crate::Generator::customize`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum StyleProfile {
    /// Upstream typify output, unchanged. Constrained strings become
    /// validating newtypes, non-required arrays stay `Vec<T>`, `allOf`
    /// merges base fields into the derived struct, and so on.
    #[default]
    Typify,
    /// The ergonomic API-client shape:
    ///
    /// - plain `String` for constrained strings and date/date-time/uuid
    ///   formats; plain integers instead of `NonZero*`
    /// - every non-required field is `Option<T>` (arrays, defaulted bools,
    ///   defaulted fields included), with the per-field serde noise elided
    ///   under a struct-level `#[serde_with::skip_serializing_none]`
    /// - `#[serde(rename_all = "camelCase")]` on structs with redundant
    ///   per-field renames elided
    /// - `allOf` renders as `#[serde(flatten)]` composition
    /// - a fixed, ordered derive list (`Debug, Clone, Default, PartialEq,
    ///   Serialize, Deserialize, Patch` on structs) with
    ///   `struct_patch::Patch` deep-patch annotations on
    ///   `Option<Struct>` fields
    /// - `schemars::JsonSchema` derives behind a `schemars` Cargo feature
    ApiClient,
}

impl StyleProfile {
    /// Apply this profile's settings to `settings`.
    pub fn apply(&self, settings: &mut TypeSpaceSettings) {
        match self {
            StyleProfile::Typify => {}
            StyleProfile::ApiClient => api_client(settings),
        }
    }

    /// The `use` statements each generated module needs so the bare derive
    /// paths emitted under this profile resolve.
    pub fn trait_imports(&self) -> TokenStream {
        match self {
            StyleProfile::Typify => quote! {},
            StyleProfile::ApiClient => quote! {
                use ::serde::{Deserialize, Serialize};
                use ::struct_patch::Patch;
            },
        }
    }

    /// Whether the post-processing pass should synthesize `impl Default`
    /// for enums that typify could not default (no unit variant — e.g.
    /// untagged `oneOf` enums). Required whenever structs derive `Default`
    /// and may hold such an enum in a required field.
    pub fn synthesize_enum_defaults(&self) -> bool {
        matches!(self, StyleProfile::ApiClient)
    }
}

/// The `ApiClient` recipe. Every knob exists in the typify fork; see
/// `FORK_FEATURES.md` there for what each one does.
fn api_client(settings: &mut TypeSpaceSettings) {
    // Wire-shape: drop typify's "stricter than the wire" defaults so field
    // types match a hand-written client (`Option<T>` everywhere a field is
    // not required, plain scalar types).
    settings
        .with_unconstrained_string(true)
        .with_unconstrained_int(true)
        .with_array_optionality(ArrayOptionality::OptionalIfNotRequired)
        .with_default_bool_optionality(DefaultBoolOptionality::AlwaysOption)
        .with_defaulted_field_optionality(DefaultedFieldOptionality::AlwaysOption)
        .with_elide_option_field_defaults(true);

    // Doc comments carry only the schema description (IDE hovers stay
    // readable), and string newtypes get the AsRef/Display/From<&str>
    // convenience surface. Both were fork defaults once; they are plain
    // opt-in knobs now that the fork's defaults match upstream.
    settings
        .with_schema_in_docs(false)
        .with_string_newtype_conveniences(true);

    // Plain strings for date/uuid formats rather than chrono / uuid types;
    // the RFC 3339 / ISO-8601 wire format is preserved either way.
    settings
        .with_date_type("::std::string::String")
        .with_date_time_type("::std::string::String")
        .with_uuid_type("::std::string::String");

    // `allOf` inheritance renders as `#[serde(flatten)]` composition, and
    // `Option<{Struct}>` fields get deep `struct_patch` annotations.
    settings
        .with_allof_strategy(AllOfStrategy::Compose)
        .with_deep_patches(DeepPatchPolicy::AllOptionStructs);

    // Structs: fixed ordered derive list, struct-level
    // `skip_serializing_none` + camelCase rename_all, and the
    // `#[patch(attribute(...))]` block that configures the generated
    // `<Type>Patch` companion to mirror the parent's serde behavior.
    settings
        .with_unconditional_attr_for("serde_with::skip_serializing_none", TypeKindFilter::STRUCTS)
        .with_unconditional_derive_for("Debug", TypeKindFilter::STRUCTS)
        .with_unconditional_derive_for("Clone", TypeKindFilter::STRUCTS)
        .with_unconditional_derive_for("Default", TypeKindFilter::STRUCTS)
        .with_unconditional_derive_for("PartialEq", TypeKindFilter::STRUCTS)
        .with_unconditional_derive_for("Serialize", TypeKindFilter::STRUCTS)
        .with_unconditional_derive_for("Deserialize", TypeKindFilter::STRUCTS)
        .with_unconditional_derive_for("Patch", TypeKindFilter::STRUCTS)
        .with_struct_rename_all("camelCase")
        .with_unconditional_attr_at(
            "patch(attribute(serde_with::skip_serializing_none))",
            AttrPosition::AfterDerive,
            TypeKindFilter::STRUCTS,
        )
        .with_unconditional_attr_at(
            "patch(attribute(derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)))",
            AttrPosition::AfterDerive,
            TypeKindFilter::STRUCTS,
        )
        .with_unconditional_attr_at(
            "patch(attribute(serde(default, rename_all = \"camelCase\")))",
            AttrPosition::AfterDerive,
            TypeKindFilter::STRUCTS,
        );

    // Enums: minimal list. `Eq` is deliberately omitted — untagged
    // `oneOf`/`anyOf` enums can carry structs with floats, which are
    // `PartialEq` but not `Eq`. `Default` comes as a separate impl: from
    // typify for enums with a unit variant (below), from the
    // post-processing pass otherwise.
    settings
        .with_unconditional_derive_for("Debug", TypeKindFilter::ENUMS)
        .with_unconditional_derive_for("Clone", TypeKindFilter::ENUMS)
        .with_unconditional_derive_for("PartialEq", TypeKindFilter::ENUMS)
        .with_unconditional_derive_for("Serialize", TypeKindFilter::ENUMS)
        .with_unconditional_derive_for("Deserialize", TypeKindFilter::ENUMS);

    // Every enum without a schema-level default gets `impl Default`
    // pointing at its first unit variant, so structs deriving `Default`
    // can hold required enum fields.
    settings.with_enum_first_variant_default(true);

    // Newtypes (emitted for named scalar definitions): `Default` and
    // `PartialEq` so structs holding them as required fields can derive
    // `Default` / `PartialEq` themselves. Registering any unconditional
    // newtype derive switches to the verbatim-order path, so the
    // historical base set is restated here.
    settings
        .with_unconditional_derive_for("Debug", TypeKindFilter::NEWTYPES)
        .with_unconditional_derive_for("Clone", TypeKindFilter::NEWTYPES)
        .with_unconditional_derive_for("Default", TypeKindFilter::NEWTYPES)
        .with_unconditional_derive_for("PartialEq", TypeKindFilter::NEWTYPES)
        .with_unconditional_derive_for("::serde::Serialize", TypeKindFilter::NEWTYPES)
        .with_unconditional_derive_for("::serde::Deserialize", TypeKindFilter::NEWTYPES);

    // `schemars::JsonSchema` stays opt-in via a `schemars` Cargo feature,
    // both on the types and on their `<Type>Patch` companions.
    settings
        .with_conditional_derive("schemars", "schemars::JsonSchema")
        .with_conditional_attr_at(
            "schemars",
            "patch(attribute(derive(schemars::JsonSchema)))",
            AttrPosition::AfterDerive,
            TypeKindFilter::STRUCTS,
        );
}

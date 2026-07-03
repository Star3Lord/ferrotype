//! Style profiles: named presets of the declarative [`StyleConfig`].

use crate::config::StyleConfig;

/// A named preset of style configuration. Profiles capture the coarse
/// "what should the generated code look like" decision; granular control
/// goes through a `codegen.toml` file
/// ([`Generator::config_file`](crate::Generator::config_file)), the
/// [`Generator::style`](crate::Generator::style) hook, or — below the
/// data layer — [`Generator::customize`](crate::Generator::customize).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum StyleProfile {
    /// Upstream typify output, unchanged. Constrained strings become
    /// validating newtypes, non-required arrays stay `Vec<T>`, `allOf`
    /// merges base fields into the derived struct, and so on.
    /// (The `plain` preset in `codegen.toml` terms.)
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
    /// The [`StyleConfig`] preset this profile names.
    pub fn preset(&self) -> StyleConfig {
        match self {
            StyleProfile::Typify => StyleConfig::plain(),
            StyleProfile::ApiClient => StyleConfig::api_client(),
        }
    }
}

//! External type mappings, applied to the generated AST: the
//! field-attribute attachment and capability-aware derive pruning
//! behind `[style.formats]` table entries and `[types] replace`
//! overrides (docs/MIGRATION.md D19).
//!
//! A mapped type (`"string/date-time" = "::time::OffsetDateTime"`, or a
//! schema replaced with `::my_crate::Money`) is external: codegen knows
//! nothing about it beyond what the config declares. Two consequences
//! are handled here, both as AST passes over the post-processed
//! [`syn::File`]:
//!
//! - **field attributes** — table-form mappings can attach attribute
//!   bodies to every struct field of the mapped type
//!   (`field-attrs` on required fields, `optional-field-attrs` on
//!   `Option<...>`-wrapped ones — kept strictly separate: a
//!   `serde(with = "...")` module for `T` does not handle `Option<T>`,
//!   so a missing `optional-field-attrs` list means optional fields
//!   get nothing);
//! - **capability pruning** — the mapping's `impls` list declares what
//!   the external type provides (`Serialize`/`Deserialize` plus
//!   `Debug`/`Clone` are always assumed; everything else defaults to
//!   absent). Generated types whose derives an external type cannot
//!   satisfy have the offending derive removed, transitively: a struct
//!   deriving `Default` with a *required* field of a no-`default`
//!   mapped type loses `Default`, and so does any struct requiring
//!   *it*, to a fixpoint. `Option`/`Vec`/map-wrapped fields don't
//!   constrain `Default` (they default to empty), but do constrain the
//!   equality family (`Option<T>: PartialEq` needs `T: PartialEq`).
//!   Patch companions keep `Default` (their fields are all `Option`)
//!   but share the equality-family pruning. Every removal emits a
//!   stderr warning naming the type, the derive, and the causing
//!   field/chain.
//!
//! `Vec<T>` fields never receive mapping attributes (out of scope; the
//! wrapped type still participates in capability analysis).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail};
use quote::ToTokens;
use syn::parse::Parser as _;

use crate::Result;
use crate::config::{Capability, StyleConfig};

/// The derive-list capabilities the pruning analysis manages, in
/// processing order.
const MANAGED: &[Capability] = &[
    Capability::Default,
    Capability::PartialEq,
    Capability::Eq,
    Capability::Hash,
    Capability::Ord,
];

/// One resolved external mapping.
struct MappingEntry {
    /// Where the entry came from, for error/warning messages
    /// (`[style.formats] "string/date-time"` or `[types."Money"] replace`).
    origin: String,
    /// The configured path, for messages.
    display_path: String,
    /// Token-normalized type (leading `::` stripped) for matching.
    type_tokens: String,
    field_attrs: Vec<syn::Attribute>,
    optional_field_attrs: Vec<syn::Attribute>,
    capabilities: BTreeSet<Capability>,
}

/// Every external mapping of one run, resolved and validated.
pub(crate) struct Mappings {
    entries: Vec<MappingEntry>,
}

impl Mappings {
    /// Collect the `[style.formats]` entries and `[types] replace`
    /// overrides; parse and validate attribute bodies and type paths
    /// (hard errors naming the config key).
    pub(crate) fn resolve(style: &StyleConfig) -> Result<Self> {
        let mut entries = Vec::new();
        for (key, mapping) in &style.formats {
            let origin = format!("[style.formats] {key:?}");
            entries.push(MappingEntry::build(
                origin,
                mapping.type_path(),
                mapping.field_attrs(),
                mapping.optional_field_attrs(),
                mapping.capabilities(),
            )?);
        }
        for (selector, override_) in &style.types {
            let Some(replace) = &override_.replace else {
                continue;
            };
            let origin = format!("[types.{selector:?}] replace");
            entries.push(MappingEntry::build(
                origin,
                replace,
                &override_.field_attrs,
                &override_.optional_field_attrs,
                &override_.replace_impls,
            )?);
        }
        Ok(Mappings { entries })
    }

    /// Attach the configured field attributes (mapping defaults,
    /// overridden per field by the resolved `plans` — the rules and
    /// `[fields]` tiers), apply rule-driven field type replacements,
    /// and prune derives the mapped types cannot satisfy. Returns the
    /// names of generated enums whose `Default` synthesis must be
    /// skipped ([`crate::postprocess::synthesize_enum_defaults`]
    /// consults it).
    pub(crate) fn apply_to_file(
        &self,
        file: &mut syn::File,
        untagged_enum_defaults: bool,
        plans: &crate::rules::FieldPlans,
    ) -> Result<BTreeSet<String>> {
        if self.entries.is_empty() && plans.is_empty() {
            return Ok(BTreeSet::new());
        }

        apply_plan_types(plans, &mut file.items)?;
        attach_attrs(&self.entries, plans, &mut file.items);

        let mut nodes: BTreeMap<String, TypeNode> = BTreeMap::new();
        collect_nodes(&self.entries, plans, &file.items, &mut nodes);

        let mut skip_default_synthesis = BTreeSet::new();
        for &capability in MANAGED {
            let lost = self.lose_fixpoint(&nodes, capability, untagged_enum_defaults);
            if lost.is_empty() {
                continue;
            }
            for (name, reason) in &lost {
                if capability == Capability::Default && nodes[name].is_enum {
                    eprintln!(
                        "openapi-codegen: warning: skipping `Default` synthesis for \
                         enum `{name}`: {reason}",
                    );
                    skip_default_synthesis.insert(name.clone());
                } else {
                    eprintln!(
                        "openapi-codegen: warning: removed derive `{}` from `{name}`: {reason}",
                        capability_ident(capability),
                    );
                }
            }
            if capability == Capability::Default {
                // struct_patch's none-as-default option merging
                // materializes a `T::default()` to deep-merge into when
                // the field is `None`, so a deep-patch annotation on
                // `Option<T>` requires `T: Default` — fields naming a
                // type that just lost it fall back to whole-value
                // patching, exactly like fields of `patch = false`
                // types do.
                prune_deep_patch_annotations(&mut file.items, &lost);
            }
            prune_derives(&mut file.items, capability, &lost);
        }
        Ok(skip_default_synthesis)
    }

    /// The set of type names losing `capability`, mapped to the reason,
    /// computed to a fixpoint over the generated item graph.
    fn lose_fixpoint(
        &self,
        nodes: &BTreeMap<String, TypeNode>,
        capability: Capability,
        untagged_enum_defaults: bool,
    ) -> BTreeMap<String, String> {
        let mut lost: BTreeMap<String, String> = BTreeMap::new();
        loop {
            let mut changed = false;
            for (name, node) in nodes {
                if lost.contains_key(name) || !node.governed_by(capability, untagged_enum_defaults)
                {
                    continue;
                }
                let deps = node.constraining_deps(capability);
                let culprit = deps.iter().find_map(|dep| match &dep.target {
                    DepTarget::External {
                        display,
                        origin,
                        capabilities,
                    } => (!capabilities.contains(&capability)).then(|| {
                        format!(
                            "{} `{}` is `{display}` ({origin}), which does not declare \
                             the `{}` capability",
                            dep.description(),
                            dep.label,
                            capability_name(capability),
                        )
                    }),
                    DepTarget::Generated(target) => lost.contains_key(target).then(|| {
                        format!(
                            "{} `{}` is `{target}`, which itself lost `{}`",
                            dep.description(),
                            dep.label,
                            capability_ident(capability),
                        )
                    }),
                });
                if let Some(reason) = culprit {
                    lost.insert(name.clone(), reason);
                    changed = true;
                }
            }
            if !changed {
                return lost;
            }
        }
    }
}

impl MappingEntry {
    fn build(
        origin: String,
        type_path: &str,
        field_attrs: &[String],
        optional_field_attrs: &[String],
        capabilities: &[Capability],
    ) -> Result<Self> {
        let parsed: syn::Type = syn::parse_str(type_path)
            .with_context(|| format!("{origin}: {type_path:?} is not a valid Rust type"))?;
        Ok(MappingEntry {
            type_tokens: normalized_tokens(&parsed),
            display_path: type_path.to_string(),
            field_attrs: parse_attr_bodies(&origin, "field-attrs", field_attrs)?,
            optional_field_attrs: parse_attr_bodies(
                &origin,
                "optional-field-attrs",
                optional_field_attrs,
            )?,
            capabilities: capabilities.iter().copied().collect(),
            origin,
        })
    }
}

/// Parse attribute bodies (`serde(with = "...")`) into
/// [`syn::Attribute`]s by wrapping each in `#[...]`. Invalid bodies are
/// hard errors naming the config key.
pub(crate) fn parse_attr_bodies(
    origin: &str,
    key: &str,
    bodies: &[String],
) -> Result<Vec<syn::Attribute>> {
    let mut attrs = Vec::with_capacity(bodies.len());
    for body in bodies {
        let parsed = syn::Attribute::parse_outer
            .parse_str(&format!("#[{body}]"))
            .with_context(|| {
                format!("{origin}: `{key}` entry {body:?} is not a valid attribute body")
            })?;
        let [attr] = parsed.as_slice() else {
            bail!("{origin}: `{key}` entry {body:?} must be exactly one attribute");
        };
        attrs.push(attr.clone());
    }
    Ok(attrs)
}

/// A type's token rendering with a leading `::` stripped, so
/// `::time::OffsetDateTime` and `time::OffsetDateTime` compare equal —
/// full-path comparison, not last-segment.
fn normalized_tokens(ty: &syn::Type) -> String {
    let tokens = ty.to_token_stream().to_string();
    tokens.strip_prefix(":: ").unwrap_or(&tokens).to_string()
}

// ─── Field type replacements from the rules tier ────────────────────────────

/// Apply the resolved plans' rule-driven type replacements (the
/// `[fields]` tier's are applied earlier by `overrides`): swap the
/// field's type — preserving an `Option<...>` wrapper — and strip any
/// deep-patch annotation, which named the displaced type's companion.
fn apply_plan_types(plans: &crate::rules::FieldPlans, items: &mut [syn::Item]) -> Result<()> {
    for item in items {
        match item {
            syn::Item::Mod(module) => {
                if let Some((_, children)) = &mut module.content {
                    apply_plan_types(plans, children)?;
                }
            }
            syn::Item::Struct(item_struct) => {
                let owner = item_struct.ident.to_string();
                for field in &mut item_struct.fields {
                    let Some(ident) = &field.ident else { continue };
                    let Some(plan) = plans.get(&owner, &ident.to_string()) else {
                        continue;
                    };
                    if let Some(type_path) = &plan.replace_type {
                        crate::overrides::replace_field_type(field, type_path)?;
                        field.attrs.retain(|attr| !is_deep_patch_attr(attr));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ─── Field-attribute attachment ─────────────────────────────────────────────

/// How a struct field holds a mapped type.
enum FieldMatch {
    Required(usize),
    Optional(usize),
}

fn attach_attrs(
    entries: &[MappingEntry],
    plans: &crate::rules::FieldPlans,
    items: &mut [syn::Item],
) {
    for item in items {
        match item {
            syn::Item::Mod(module) => {
                if let Some((_, children)) = &mut module.content {
                    attach_attrs(entries, plans, children);
                }
            }
            syn::Item::Struct(item_struct) => {
                let owner = item_struct.ident.to_string();
                for field in &mut item_struct.fields {
                    // A resolved per-field decision (rules/[fields]
                    // tiers) replaces the mapping's attrs wholesale —
                    // most-specific-wins, an empty list clears.
                    if let Some(plan) = field
                        .ident
                        .as_ref()
                        .and_then(|ident| plans.get(&owner, &ident.to_string()))
                        && let Some(attrs) = &plan.attrs
                    {
                        field.attrs.extend(attrs.iter().cloned());
                        continue;
                    }
                    match match_field(entries, &field.ty) {
                        Some(FieldMatch::Required(index)) => {
                            field.attrs.extend(entries[index].field_attrs.iter().cloned());
                        }
                        Some(FieldMatch::Optional(index)) => {
                            // Deliberately NOT falling back to
                            // `field_attrs`: a `serde(with = ...)`
                            // module for `T` cannot handle `Option<T>`.
                            field
                                .attrs
                                .extend(entries[index].optional_field_attrs.iter().cloned());
                        }
                        None => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// Match a field type against the mapped types: the type itself
/// (through `Box`) is a required match; `Option<T>` / `Option<Box<T>>`
/// an optional one. `Vec<T>` fields are out of scope for attributes.
fn match_field(entries: &[MappingEntry], ty: &syn::Type) -> Option<FieldMatch> {
    let find = |ty: &syn::Type| {
        let tokens = normalized_tokens(ty);
        entries.iter().position(|entry| entry.type_tokens == tokens)
    };

    if let Some(inner) = unwrap_wrapper(ty, "Option") {
        let inner = unwrap_wrapper(inner, "Box").unwrap_or(inner);
        return find(inner).map(FieldMatch::Optional);
    }
    let bare = unwrap_wrapper(ty, "Box").unwrap_or(ty);
    find(bare).map(FieldMatch::Required)
}

/// The `T` of `wrapper<T>` when `ty`'s last path segment is `wrapper`
/// with exactly one angle-bracketed type argument.
pub(crate) fn unwrap_wrapper<'a>(ty: &'a syn::Type, wrapper: &str) -> Option<&'a syn::Type> {
    let syn::Type::Path(type_path) = ty else {
        return None;
    };
    let last = type_path.path.segments.last()?;
    if last.ident != wrapper {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    match args.args.first()? {
        syn::GenericArgument::Type(inner) if args.args.len() == 1 => Some(inner),
        _ => None,
    }
}

// ─── Capability analysis ────────────────────────────────────────────────────

/// One field/variant-payload dependency of a generated type.
struct Dep {
    /// Rust field ident, or `variant \`V\`` for enum payloads.
    label: String,
    /// Inside `Option`/`Vec`/map wrappers (which provide the wrapped
    /// capability for `Default` but not for the equality family).
    wrapped: bool,
    /// Whether this sits in an enum variant payload.
    in_variant: bool,
    target: DepTarget,
}

impl Dep {
    fn description(&self) -> &'static str {
        match (self.in_variant, self.wrapped) {
            (true, _) => "variant payload",
            (false, false) => "required field",
            (false, true) => "field",
        }
    }
}

enum DepTarget {
    External {
        display: String,
        origin: String,
        capabilities: BTreeSet<Capability>,
    },
    Generated(String),
}

/// A generated struct or enum in the capability graph. `deps` holds
/// every struct field (or every enum variant payload — the equality
/// family's constraint set); `first_variant_deps` holds only the first
/// variant's payloads, the constraint set of an enum's synthesized
/// `Default` (which builds `Self::First(Default::default(), ...)`).
struct TypeNode {
    is_enum: bool,
    /// A payload-carrying enum: the `untagged-enum-defaults` synthesis
    /// candidate. Enums never *derive* `Default` in generated code —
    /// their `Default` is the synthesized impl this flag models.
    synthesis_candidate: bool,
    derives: BTreeSet<String>,
    deps: Vec<Dep>,
    first_variant_deps: Vec<Dep>,
}

impl TypeNode {
    /// Does `capability` govern this node at all?
    fn governed_by(&self, capability: Capability, untagged_enum_defaults: bool) -> bool {
        if capability == Capability::Default && self.is_enum {
            return self.synthesis_candidate && untagged_enum_defaults;
        }
        self.derives.contains(capability_ident(capability))
    }

    /// The dependencies that constrain `capability`: for `Default`,
    /// only unwrapped ones (struct required fields, or the synthesized
    /// first-variant payload — `Option`/`Vec`/maps default to empty);
    /// for the equality family, every dependency (wrappers propagate
    /// the bound: `Option<T>: PartialEq` requires `T: PartialEq`).
    fn constraining_deps(&self, capability: Capability) -> Vec<&Dep> {
        match (capability, self.is_enum) {
            (Capability::Default, true) => self
                .first_variant_deps
                .iter()
                .filter(|dep| !dep.wrapped)
                .collect(),
            (Capability::Default, false) => {
                self.deps.iter().filter(|dep| !dep.wrapped).collect()
            }
            _ => self.deps.iter().collect(),
        }
    }
}

fn collect_nodes(
    entries: &[MappingEntry],
    plans: &crate::rules::FieldPlans,
    items: &[syn::Item],
    nodes: &mut BTreeMap<String, TypeNode>,
) {
    // First pass: the set of generated type names, so dependencies on
    // generated types can be told apart from unmapped externals.
    fn generated_names(items: &[syn::Item], names: &mut BTreeSet<String>) {
        for item in items {
            match item {
                syn::Item::Mod(module) => {
                    if let Some((_, children)) = &module.content {
                        generated_names(children, names);
                    }
                }
                syn::Item::Struct(s) => {
                    names.insert(s.ident.to_string());
                }
                syn::Item::Enum(e) => {
                    names.insert(e.ident.to_string());
                }
                _ => {}
            }
        }
    }
    let mut names = BTreeSet::new();
    generated_names(items, &mut names);

    fn walk(
        entries: &[MappingEntry],
        plans: &crate::rules::FieldPlans,
        names: &BTreeSet<String>,
        items: &[syn::Item],
        nodes: &mut BTreeMap<String, TypeNode>,
    ) {
        for item in items {
            match item {
                syn::Item::Mod(module) => {
                    if let Some((_, children)) = &module.content {
                        walk(entries, plans, names, children, nodes);
                    }
                }
                syn::Item::Struct(item_struct) => {
                    let owner = item_struct.ident.to_string();
                    let mut deps = Vec::new();
                    for field in &item_struct.fields {
                        let label = field
                            .ident
                            .as_ref()
                            .map(|ident| ident.to_string())
                            .unwrap_or_else(|| "0".to_string());
                        // A per-field capability declaration (rules /
                        // [fields] tiers) overrides the mapping's
                        // per-type one for this field's dependencies.
                        let field_caps = plans
                            .get(&owner, &label)
                            .and_then(|plan| plan.capabilities.as_ref());
                        collect_deps(
                            entries, field_caps, names, &field.ty, false, false, &label, &mut deps,
                        );
                    }
                    nodes.insert(
                        owner,
                        TypeNode {
                            is_enum: false,
                            synthesis_candidate: false,
                            derives: derive_idents(&item_struct.attrs),
                            deps,
                            first_variant_deps: Vec::new(),
                        },
                    );
                }
                syn::Item::Enum(item_enum) => {
                    let mut deps = Vec::new();
                    let mut first_variant_deps = Vec::new();
                    let mut has_payload = false;
                    for (index, variant) in item_enum.variants.iter().enumerate() {
                        if matches!(variant.fields, syn::Fields::Unit) {
                            continue;
                        }
                        has_payload = true;
                        let label = format!("variant `{}`", variant.ident);
                        for field in &variant.fields {
                            collect_deps(
                                entries, None, names, &field.ty, false, true, &label, &mut deps,
                            );
                            if index == 0 {
                                collect_deps(
                                    entries,
                                    None,
                                    names,
                                    &field.ty,
                                    false,
                                    true,
                                    &label,
                                    &mut first_variant_deps,
                                );
                            }
                        }
                    }
                    nodes.insert(
                        item_enum.ident.to_string(),
                        TypeNode {
                            is_enum: true,
                            synthesis_candidate: has_payload,
                            derives: derive_idents(&item_enum.attrs),
                            deps,
                            first_variant_deps,
                        },
                    );
                }
                _ => {}
            }
        }
    }
    walk(entries, plans, &names, items, nodes);
}

/// Record every mapped-external or generated type mentioned by `ty`.
/// `field_caps`, when set, declares this field's type capabilities
/// (a rules-/`[fields]`-tier `impls` or table-form type override) and
/// wins over the per-type mapping entry.
#[allow(clippy::too_many_arguments)]
fn collect_deps(
    entries: &[MappingEntry],
    field_caps: Option<&(BTreeSet<Capability>, String)>,
    names: &BTreeSet<String>,
    ty: &syn::Type,
    wrapped: bool,
    in_variant: bool,
    label: &str,
    out: &mut Vec<Dep>,
) {
    let syn::Type::Path(type_path) = ty else {
        return;
    };
    let Some(last) = type_path.path.segments.last() else {
        return;
    };
    let ident = last.ident.to_string();
    let is_defaulting_wrapper =
        matches!(ident.as_str(), "Option" | "Vec" | "HashMap" | "BTreeMap");
    if (is_defaulting_wrapper || ident == "Box")
        && let syn::PathArguments::AngleBracketed(args) = &last.arguments
    {
        for arg in &args.args {
            if let syn::GenericArgument::Type(inner) = arg {
                collect_deps(
                    entries,
                    field_caps,
                    names,
                    inner,
                    wrapped || is_defaulting_wrapper,
                    in_variant,
                    label,
                    out,
                );
            }
        }
        return;
    }

    let tokens = normalized_tokens(ty);
    if let Some((capabilities, origin)) = field_caps {
        out.push(Dep {
            label: label.to_string(),
            wrapped,
            in_variant,
            target: DepTarget::External {
                display: tokens,
                origin: origin.clone(),
                capabilities: capabilities.clone(),
            },
        });
    } else if let Some(entry) = entries.iter().find(|entry| entry.type_tokens == tokens) {
        out.push(Dep {
            label: label.to_string(),
            wrapped,
            in_variant,
            target: DepTarget::External {
                display: entry.display_path.clone(),
                origin: entry.origin.clone(),
                capabilities: entry.capabilities.clone(),
            },
        });
    } else if type_path.path.segments.len() == 1 && names.contains(&ident) {
        out.push(Dep {
            label: label.to_string(),
            wrapped,
            in_variant,
            target: DepTarget::Generated(ident),
        });
    }
}

/// The idents in an item's main `#[derive(...)]` attribute(s)
/// (conditional `cfg_attr` derives are not managed).
fn derive_idents(attrs: &[syn::Attribute]) -> BTreeSet<String> {
    let mut idents = BTreeSet::new();
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        if let Ok(paths) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
        ) {
            for path in paths {
                if let Some(segment) = path.segments.last() {
                    idents.insert(segment.ident.to_string());
                }
            }
        }
    }
    idents
}

// ─── Derive pruning ─────────────────────────────────────────────────────────

/// Remove `capability`'s derive from every lost type: from the main
/// `#[derive(...)]` list, and — for the equality family only — from
/// `#[patch(attribute(derive(...)))]` companion lists too. Patch
/// companions keep `Default`: their fields are all `Option`-wrapped,
/// which defaults fine regardless of the inner type.
fn prune_derives(
    items: &mut [syn::Item],
    capability: Capability,
    lost: &BTreeMap<String, String>,
) {
    let ident = capability_ident(capability);
    let prune_companion = capability != Capability::Default;
    for item in items {
        match item {
            syn::Item::Mod(module) => {
                if let Some((_, children)) = &mut module.content {
                    prune_derives(children, capability, lost);
                }
            }
            syn::Item::Struct(item_struct) => {
                if lost.contains_key(&item_struct.ident.to_string()) {
                    prune_from_attrs(&mut item_struct.attrs, ident, prune_companion);
                }
            }
            syn::Item::Enum(item_enum) if lost.contains_key(&item_enum.ident.to_string()) => {
                prune_from_attrs(&mut item_enum.attrs, ident, prune_companion);
            }
            _ => {}
        }
    }
}

fn prune_from_attrs(attrs: &mut [syn::Attribute], ident: &str, prune_companion: bool) {
    for attr in attrs {
        if attr.path().is_ident("derive") {
            remove_from_derive_list(attr, ident);
        } else if prune_companion && attr.path().is_ident("patch") {
            remove_from_patch_companion_derive(attr, ident);
        }
    }
}

/// Drop `ident` from a `#[derive(...)]` attribute's list.
fn remove_from_derive_list(attr: &mut syn::Attribute, ident: &str) {
    let Ok(paths) = attr.parse_args_with(
        syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
    ) else {
        return;
    };
    let kept: Vec<syn::Path> = paths
        .into_iter()
        .filter(|path| {
            path.segments
                .last()
                .is_none_or(|segment| segment.ident != ident)
        })
        .collect();
    if let syn::Meta::List(meta) = &mut attr.meta {
        meta.tokens = quote::quote! { #(#kept),* };
    }
}

/// Drop `ident` from the derive list inside
/// `#[patch(attribute(derive(...)))]`; other `patch(...)` attributes
/// are left untouched.
fn remove_from_patch_companion_derive(attr: &mut syn::Attribute, ident: &str) {
    let syn::Meta::List(patch_meta) = &mut attr.meta else {
        return;
    };
    let Ok(syn::Meta::List(attribute_meta)) =
        syn::parse2::<syn::Meta>(patch_meta.tokens.clone())
    else {
        return;
    };
    if !attribute_meta.path.is_ident("attribute") {
        return;
    }
    let Ok(syn::Meta::List(derive_meta)) =
        syn::parse2::<syn::Meta>(attribute_meta.tokens.clone())
    else {
        return;
    };
    if !derive_meta.path.is_ident("derive") {
        return;
    }
    let Ok(paths) = syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated
        .parse2(derive_meta.tokens.clone())
    else {
        return;
    };
    let kept: Vec<syn::Path> = paths
        .into_iter()
        .filter(|path| {
            path.segments
                .last()
                .is_none_or(|segment| segment.ident != ident)
        })
        .collect();
    patch_meta.tokens = quote::quote! { attribute(derive(#(#kept),*)) };
}

/// Remove `#[patch(name = ...)]` deep-patch annotations from fields
/// whose (`Option`-wrapped, possibly boxed) type lost `Default`:
/// struct_patch's none-as-default merge needs it, and without the
/// annotation the field falls back to whole-value patching.
fn prune_deep_patch_annotations(items: &mut [syn::Item], lost: &BTreeMap<String, String>) {
    for item in items {
        match item {
            syn::Item::Mod(module) => {
                if let Some((_, children)) = &mut module.content {
                    prune_deep_patch_annotations(children, lost);
                }
            }
            syn::Item::Struct(item_struct) => {
                let owner = item_struct.ident.to_string();
                for field in &mut item_struct.fields {
                    let mut inner = &field.ty;
                    if let Some(unwrapped) = unwrap_wrapper(inner, "Option") {
                        inner = unwrapped;
                    }
                    if let Some(unwrapped) = unwrap_wrapper(inner, "Box") {
                        inner = unwrapped;
                    }
                    let target = normalized_tokens(inner);
                    if !lost.contains_key(&target)
                        || !field.attrs.iter().any(is_deep_patch_attr)
                    {
                        continue;
                    }
                    field.attrs.retain(|attr| !is_deep_patch_attr(attr));
                    let label = field
                        .ident
                        .as_ref()
                        .map(|ident| ident.to_string())
                        .unwrap_or_default();
                    eprintln!(
                        "openapi-codegen: warning: removed the deep-patch annotation from \
                         `{owner}.{label}`: `{target}` lost `Default`, which struct_patch's \
                         none-as-default option merging requires; the field patches by \
                         whole-value replacement instead",
                    );
                }
            }
            _ => {}
        }
    }
}

/// A `#[patch(name = "...")]` field annotation.
fn is_deep_patch_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("patch")
        && matches!(
            &attr.meta,
            syn::Meta::List(list) if list.tokens.to_string().starts_with("name ")
        )
}

/// The derive ident a managed capability governs.
fn capability_ident(capability: Capability) -> &'static str {
    capability
        .derive_ident()
        .expect("MANAGED capabilities all govern a derive")
}

/// The kebab-case config name of a capability, for messages.
fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::Default => "default",
        Capability::Serialize => "serialize",
        Capability::Deserialize => "deserialize",
        Capability::Display => "display",
        Capability::FromStr => "from-str",
        Capability::FromStringIrrefutable => "from-string-irrefutable",
        Capability::PartialEq => "partial-eq",
        Capability::Eq => "eq",
        Capability::Hash => "hash",
        Capability::Ord => "ord",
    }
}

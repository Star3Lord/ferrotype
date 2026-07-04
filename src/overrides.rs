//! Per-type and per-field override resolution: the consumer of the
//! `patch` / `deep-patch` / `[types]` / `[fields]` configuration keys.
//!
//! typify's style surface is global-per-kind; the granular decisions are
//! made here instead, split across the two levers this crate owns:
//!
//! - **at generation time**, the fork's `with_deep_patch_filter` closure
//!   (built by [`Overrides::deep_patch_filter`]) decides every
//!   `#[patch(name = "Option<InnerPatch>")]` annotation — consulting the
//!   per-field overrides, then per-type patchability of both the owner
//!   and the inner type, then the style-level [`DeepPatchMode`];
//! - **after generation**, [`Overrides::apply_to_file`] strips the
//!   `Patch` derive and `patch(...)` attributes from non-patchable
//!   structs, applies per-field Rust-type replacements, validates every
//!   selector matched something (unmatched keys are hard errors), and
//!   drops the `struct_patch` import when structs exist but none is
//!   patchable — so fully patch-free output does not require the
//!   dependency.
//!
//! Config keys are schema names (`components.schemas` keys and wire
//! property names); they are translated once to the Rust names that
//! generation-time hooks and the AST observe via the fork's exported
//! sanitizers ([`typify::rust_type_ident`] / [`typify::rust_field_ident`]),
//! so generated-name keys work too.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use quote::ToTokens;

use crate::Result;
use crate::config::{DeepPatchMode, StyleConfig};

/// Resolved per-type plan, keyed by Rust type name.
#[derive(Debug, Clone, Default)]
struct TypePlan {
    /// The config selector (for error messages).
    selector: String,
    patch: Option<bool>,
    /// The `replace` path: the schema generates no item; references
    /// name this type instead.
    replace: Option<String>,
}

/// Resolved per-field plan, keyed by (Rust type name, Rust field name).
#[derive(Debug, Clone, Default)]
struct FieldPlan {
    selector: String,
    deep_patch: Option<bool>,
    type_path: Option<String>,
}

/// The resolved override plan for one generation run.
#[derive(Debug, Clone)]
pub(crate) struct Overrides {
    patch_baseline: bool,
    deep_patch_all: bool,
    types: BTreeMap<String, TypePlan>,
    fields: BTreeMap<(String, String), FieldPlan>,
    /// Type-level `[[rules]] patch` decisions, layered between the
    /// exact `[types]` entries (which win) and the style baseline.
    /// Installed by [`Self::set_rule_patchability`] once the rules
    /// tier has resolved (it needs the partition, which does not exist
    /// at `Overrides::resolve` time).
    rule_patch: BTreeMap<String, bool>,
}

impl Overrides {
    /// Translate the config's schema-name-keyed overrides into Rust
    /// names and validate combinations that can never work (including
    /// the `[style] formats` key shape — this is the chokepoint every
    /// generation path passes through).
    pub(crate) fn resolve(style: &StyleConfig) -> Result<Self> {
        for key in style.formats.keys() {
            if !key.contains('/') {
                bail!(
                    "style formats key {key:?} must be \"<instance-type>/<format>\", \
                     e.g. \"string/date-time\" or \"integer/int64\"",
                );
            }
        }

        let mut types: BTreeMap<String, TypePlan> = BTreeMap::new();
        for (selector, override_) in &style.types {
            if override_.replace.is_some()
                && (override_.patch.is_some()
                    || !override_.derives_add.is_empty()
                    || override_.module.is_some())
            {
                bail!(
                    "type override {selector:?} combines `replace` with \
                     `patch`/`derives-add`/`module`; a replaced schema generates no \
                     type to patch, derive on, or place",
                );
            }
            if override_.replace.is_none() && !override_.replace_impls.is_empty() {
                bail!(
                    "type override {selector:?} sets `replace-impls` without `replace`",
                );
            }
            if override_.replace.is_none()
                && !(override_.field_attrs.is_empty() && override_.optional_field_attrs.is_empty())
            {
                bail!(
                    "type override {selector:?} sets `field-attrs`/`optional-field-attrs` \
                     without `replace`; attributes attach to fields holding the \
                     replacement type",
                );
            }
            let rust_name = typify::rust_type_ident(selector);
            let plan = types.entry(rust_name).or_default();
            if !plan.selector.is_empty() && plan.selector != *selector {
                bail!(
                    "type override selectors {:?} and {selector:?} resolve to the same \
                     Rust type",
                    plan.selector,
                );
            }
            plan.selector = selector.clone();
            plan.patch = override_.patch;
            plan.replace = override_.replace.clone();
        }

        let mut fields: BTreeMap<(String, String), FieldPlan> = BTreeMap::new();
        for (selector, override_) in &style.fields {
            let (type_part, field_part) = selector.split_once('.').with_context(|| {
                format!("field selector {selector:?} is not of the form Type.field")
            })?;
            if override_.deep_patch == Some(true) && override_.type_path.is_some() {
                bail!(
                    "field override {selector:?} combines `deep-patch = true` with a \
                     `type` replacement; a replaced type has no known Patch companion, \
                     so the annotation cannot be emitted",
                );
            }
            let key = (
                typify::rust_type_ident(type_part),
                typify::rust_field_ident(field_part),
            );
            let plan = fields.entry(key).or_default();
            if !plan.selector.is_empty() && plan.selector != *selector {
                bail!(
                    "field override selectors {:?} and {selector:?} resolve to the same \
                     Rust field",
                    plan.selector,
                );
            }
            plan.selector = selector.clone();
            plan.deep_patch = override_.deep_patch;
            plan.type_path = override_
                .type_path
                .as_ref()
                .map(|replacement| replacement.type_path().to_string());
        }

        Ok(Overrides {
            patch_baseline: style.patch,
            deep_patch_all: style.deep_patch == DeepPatchMode::AllOptionStructs,
            types,
            fields,
            rule_patch: BTreeMap::new(),
        })
    }

    /// Install the `[[rules]]` tier's type-level patchability
    /// decisions (see [`crate::rules::FieldRules::patch_overrides`]).
    pub(crate) fn set_rule_patchability(&mut self, rule_patch: BTreeMap<String, bool>) {
        self.rule_patch = rule_patch;
    }

    /// Effective patchability of a generated type: the exact `[types]`
    /// entry, then the `[[rules]]` tier, then the style baseline.
    fn is_patchable(&self, rust_type: &str) -> bool {
        self.types
            .get(rust_type)
            .and_then(|plan| plan.patch)
            .or_else(|| self.rule_patch.get(rust_type).copied())
            .unwrap_or(self.patch_baseline)
    }

    /// The predicate handed to the fork's `with_deep_patch_filter`:
    /// `(owner_struct, field, inner_struct) -> annotate?`. Field-level
    /// overrides win; then patchability of both ends (an annotation
    /// naming `{Inner}Patch` requires the inner type's companion to
    /// exist, and the owner's `Patch` derive to survive the strip);
    /// then the style-level mode. Fields with a `type` replacement are
    /// never annotated — the replacement has no known companion.
    pub(crate) fn deep_patch_filter(
        &self,
    ) -> impl Fn(&str, &str, &str) -> bool + Send + Sync + 'static {
        self.deep_patch_filter_with_rules(BTreeMap::new())
    }

    /// [`Self::deep_patch_filter`] with the `[[rules]]` tier's forced
    /// decisions layered between the `[fields]` tier (which wins) and
    /// the style-level mode.
    pub(crate) fn deep_patch_filter_with_rules(
        &self,
        rules: BTreeMap<(String, String), bool>,
    ) -> impl Fn(&str, &str, &str) -> bool + Send + Sync + 'static {
        let plan = self.clone();
        move |owner, field, inner| {
            let key = (owner.to_string(), field.to_string());
            if let Some(field_plan) = plan.fields.get(&key) {
                if field_plan.type_path.is_some() {
                    return false;
                }
                if let Some(forced) = field_plan.deep_patch {
                    return forced && plan.is_patchable(owner) && plan.is_patchable(inner);
                }
            }
            if let Some(forced) = rules.get(&key) {
                return *forced && plan.is_patchable(owner) && plan.is_patchable(inner);
            }
            plan.deep_patch_all && plan.is_patchable(owner) && plan.is_patchable(inner)
        }
    }

    /// Post-generation application: strip patch machinery from
    /// non-patchable structs, replace overridden field types, validate
    /// every selector and forced annotation, and drop the
    /// `struct_patch` import when no patchable struct remains.
    pub(crate) fn apply_to_file(&self, file: &mut syn::File) -> Result<()> {
        let mut cx = ApplyContext {
            plan: self,
            named_structs: Vec::new(),
            matched_types: BTreeMap::new(),
            matched_fields: BTreeMap::new(),
        };
        cx.walk_items(&mut file.items)?;

        // A replaced schema deliberately generates no item, so the
        // AST-match requirement below cannot apply; its validation is
        // that the replacement path actually appears in the output —
        // a replace entry no reference consumes (schema absent or
        // unreferenced) is a configuration bug like any other
        // unmatched selector.
        let rendered = self
            .types
            .values()
            .any(|plan| plan.replace.is_some())
            .then(|| file.to_token_stream().to_string());

        // Unmatched selectors are configuration bugs; refuse loudly.
        // (Every `[types]` entry lands in the plan, whatever mix of
        // overrides it carries.)
        for (rust_name, plan) in &self.types {
            if let Some(replace) = &plan.replace {
                let replacement_tokens = syn::parse_str::<syn::Type>(replace)
                    .with_context(|| {
                        format!(
                            "type override {:?}: `replace` value {replace:?} is not a \
                             valid Rust type",
                            plan.selector,
                        )
                    })?
                    .to_token_stream()
                    .to_string();
                if cx.matched_types.contains_key(rust_name) {
                    bail!(
                        "type override {:?} sets `replace`, but a type named \
                         `{rust_name}` was still generated",
                        plan.selector,
                    );
                }
                if !rendered
                    .as_deref()
                    .is_some_and(|source| source.contains(&replacement_tokens))
                {
                    bail!(
                        "type override {:?} replaces `{rust_name}` with {replace:?}, but \
                         the replacement type appears nowhere in the generated output \
                         (schema missing from the spec, or referenced by nothing)",
                        plan.selector,
                    );
                }
                continue;
            }
            let Some(kind) = cx.matched_types.get(rust_name) else {
                bail!(
                    "type override selector {:?} matched nothing (no generated type \
                     named `{rust_name}`)",
                    plan.selector,
                );
            };
            if plan.patch.is_some() && *kind != ItemKind::Struct {
                bail!(
                    "type override `patch` targets `{rust_name}` ({:?}), which is not a \
                     struct; struct_patch machinery applies to structs only",
                    kind,
                );
            }
        }
        for (key, plan) in &self.fields {
            let Some(seen) = cx.matched_fields.get(key) else {
                bail!(
                    "field override selector {:?} matched nothing (no field \
                     `{}.{}` in the generated types)",
                    plan.selector,
                    key.0,
                    key.1,
                );
            };
            if plan.deep_patch == Some(true) && !seen.annotated {
                let owner = &key.0;
                if !self.is_patchable(owner) {
                    bail!(
                        "field override {:?} forces `deep-patch = true`, but `{owner}` \
                         has patch disabled — the annotation rides on the owning \
                         struct's `Patch` derive",
                        plan.selector,
                    );
                }
                if let Some(inner) = &seen.inner_type
                    && !self.is_patchable(inner)
                {
                    bail!(
                        "field override {:?} forces `deep-patch = true`, but `{inner}` \
                         has patch disabled, so its `{inner}Patch` companion will not \
                         exist",
                        plan.selector,
                    );
                }
                bail!(
                    "field override {:?} forces `deep-patch = true`, but the field is \
                     not an `Option<{{Struct}}>` field, so the annotation cannot be \
                     emitted",
                    plan.selector,
                );
            }
        }

        // Fully patch-free output must not require the struct_patch
        // dependency: when structs exist but none kept its machinery,
        // remove the `use ::struct_patch::...` preamble lines.
        let any_patchable_struct = cx
            .named_structs
            .iter()
            .any(|name| self.is_patchable(name));
        if !cx.named_structs.is_empty() && !any_patchable_struct {
            remove_struct_patch_imports(&mut file.items);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Struct,
    Newtype,
    Enum,
    Alias,
}

/// What the walk observed about a field named by an override.
struct SeenField {
    /// Whether the field carries a `#[patch(name = ...)]` annotation
    /// after application.
    annotated: bool,
    /// The innermost path ident of an `Option<...>` type (through
    /// `Box`), for cause-specific forced-deep-patch errors.
    inner_type: Option<String>,
}

struct ApplyContext<'a> {
    plan: &'a Overrides,
    /// Rust names of every named-fields struct in the output.
    named_structs: Vec<String>,
    /// Every named top-level type seen, for selector validation.
    matched_types: BTreeMap<String, ItemKind>,
    /// Field-override keys seen.
    matched_fields: BTreeMap<(String, String), SeenField>,
}

impl ApplyContext<'_> {
    fn walk_items(&mut self, items: &mut [syn::Item]) -> Result<()> {
        for item in items.iter_mut() {
            match item {
                syn::Item::Mod(module) => {
                    if let Some((_, child_items)) = &mut module.content {
                        self.walk_items(child_items)?;
                    }
                }
                syn::Item::Struct(item_struct) => self.visit_struct(item_struct)?,
                syn::Item::Enum(item_enum) => {
                    self.matched_types
                        .insert(item_enum.ident.to_string(), ItemKind::Enum);
                }
                syn::Item::Type(item_type) => {
                    self.matched_types
                        .insert(item_type.ident.to_string(), ItemKind::Alias);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn visit_struct(&mut self, item_struct: &mut syn::ItemStruct) -> Result<()> {
        let name = item_struct.ident.to_string();
        let kind = match &item_struct.fields {
            syn::Fields::Named(_) => ItemKind::Struct,
            _ => ItemKind::Newtype,
        };
        self.matched_types.insert(name.clone(), kind);
        if kind != ItemKind::Struct {
            return Ok(());
        }
        self.named_structs.push(name.clone());

        if !self.plan.is_patchable(&name) {
            strip_patch_machinery(item_struct);
        }

        let syn::Fields::Named(fields) = &mut item_struct.fields else {
            unreachable!("checked above");
        };
        for field in &mut fields.named {
            let Some(ident) = &field.ident else { continue };
            let key = (name.clone(), ident.to_string());
            let Some(plan) = self.plan.fields.get(&key) else {
                continue;
            };
            if let Some(type_path) = &plan.type_path {
                replace_field_type(field, type_path).with_context(|| {
                    format!("applying `type` override for {:?}", plan.selector)
                })?;
            }
            self.matched_fields.insert(
                key,
                SeenField {
                    annotated: field.attrs.iter().any(is_patch_attr),
                    inner_type: option_inner_name(&field.ty),
                },
            );
        }
        Ok(())
    }
}

/// Remove the `Patch` derive and every `patch(...)` /
/// `cfg_attr(..., patch(...))` attribute (type- and field-level) from a
/// struct that resolved non-patchable.
fn strip_patch_machinery(item_struct: &mut syn::ItemStruct) {
    item_struct.attrs.retain(|attr| !is_patch_related_attr(attr));
    for attr in &mut item_struct.attrs {
        strip_patch_derive(attr);
    }
    if let syn::Fields::Named(fields) = &mut item_struct.fields {
        for field in &mut fields.named {
            field.attrs.retain(|attr| !is_patch_attr(attr));
        }
    }
}

/// A bare `#[patch(...)]` attribute.
fn is_patch_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("patch")
}

/// A `#[patch(...)]` attribute, or a `#[cfg_attr(...)]` whose payload is
/// patch-related (`patch(...)` attrs or a `struct_patch` derive).
fn is_patch_related_attr(attr: &syn::Attribute) -> bool {
    if is_patch_attr(attr) {
        return true;
    }
    if attr.path().is_ident("cfg_attr") {
        let tokens = attr.to_token_stream().to_string();
        return tokens.contains("patch (") || tokens.contains("struct_patch");
    }
    false
}

/// Remove `Patch` (or `struct_patch::Patch`) from a `#[derive(...)]`
/// list, leaving other derives untouched.
fn strip_patch_derive(attr: &mut syn::Attribute) {
    if !attr.path().is_ident("derive") {
        return;
    }
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
                .is_none_or(|segment| segment.ident != "Patch")
        })
        .collect();
    let syn::Meta::List(meta) = &mut attr.meta else {
        return;
    };
    meta.tokens = quote::quote! { #(#kept),* };
}

/// The innermost path ident of an `Option<T>` / `Option<Box<T>>` type,
/// e.g. `Category` for `::std::option::Option<Category>`.
fn option_inner_name(ty: &syn::Type) -> Option<String> {
    fn unwrap_one<'a>(ty: &'a syn::Type, wrapper: &str) -> Option<&'a syn::Type> {
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

    let mut inner = unwrap_one(ty, "Option")?;
    if let Some(boxed) = unwrap_one(inner, "Box") {
        inner = boxed;
    }
    let syn::Type::Path(type_path) = inner else {
        return None;
    };
    type_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

/// Replace a field's Rust type with `type_path`, preserving an
/// `Option<...>` wrapper when present.
pub(crate) fn replace_field_type(field: &mut syn::Field, type_path: &str) -> Result<()> {
    let new_ty: syn::Type = syn::parse_str(type_path)
        .with_context(|| format!("override type {type_path:?} is not a valid Rust type"))?;
    if let syn::Type::Path(type_path) = &mut field.ty
        && let Some(last) = type_path.path.segments.last_mut()
        && last.ident == "Option"
        && let syn::PathArguments::AngleBracketed(args) = &mut last.arguments
        && args.args.len() == 1
    {
        args.args[0] = syn::GenericArgument::Type(new_ty);
        return Ok(());
    }
    field.ty = new_ty;
    Ok(())
}

/// Remove every `use` item rooted in `struct_patch` from the file and
/// its modules (the trait import is dead once no struct derives
/// `Patch`).
fn remove_struct_patch_imports(items: &mut Vec<syn::Item>) {
    items.retain(|item| {
        !matches!(item, syn::Item::Use(item_use)
            if item_use.to_token_stream().to_string().contains("struct_patch"))
    });
    for item in items.iter_mut() {
        if let syn::Item::Mod(module) = item
            && let Some((_, child_items)) = &mut module.content
        {
            remove_struct_patch_imports(child_items);
        }
    }
}

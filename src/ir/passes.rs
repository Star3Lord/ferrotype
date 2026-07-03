//! The ordered pass pipeline: style decisions applied over the IR.
//!
//! Each pass owns one decision (docs/MIGRATION.md; R3 in
//! ARCHITECTURE.md): the config keys it consumes are named on
//! [`StyleConfig`](crate::config::StyleConfig)'s fields. Order matters
//! and is fixed by [`standard_pipeline`]; user passes can be appended
//! via [`crate::Generator::ir_pass`].

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use heck::{
    ToKebabCase, ToLowerCamelCase, ToPascalCase, ToShoutyKebabCase, ToShoutySnakeCase,
    ToSnakeCase,
};

use crate::Result;
use crate::config::{DeepPatchMode, EnumDefaultMode, KindFilter, StyleConfig};
use crate::partition::Partition;

use super::{Ir, ImplSynth, Shape, TypeRef};

/// Everything a pass may consult besides the IR itself.
pub struct PassCx<'a> {
    pub style: &'a StyleConfig,
    /// The operation partition, when a partitioned output mode is on.
    pub partition: Option<&'a Partition>,
}

/// One IR → IR transformation.
pub trait Pass {
    fn name(&self) -> &'static str;
    fn run(&self, ir: &mut Ir, cx: &PassCx<'_>) -> Result<()>;
}

/// The built-in pipeline, in order.
pub fn standard_pipeline() -> Vec<Box<dyn Pass>> {
    vec![
        Box::new(ResolveAliasPass),
        Box::new(OptionalityPass),
        Box::new(FieldOverridePass),
        Box::new(SerdeSurfacePass),
        Box::new(DeriveAttrPass),
        Box::new(ImplSynthPass),
        Box::new(DeepPatchPass),
        Box::new(PartitionPass),
        Box::new(ImportsPass),
    ]
}

/// Run `pipeline` over `ir`.
pub fn run_pipeline(
    pipeline: &[Box<dyn Pass>],
    ir: &mut Ir,
    cx: &PassCx<'_>,
) -> Result<()> {
    for pass in pipeline {
        pass.run(ir, cx)
            .with_context(|| format!("in pass {}", pass.name()))?;
    }
    Ok(())
}

/// Replace alias-typed references with their targets everywhere, so
/// later passes and the emitter see use-site types only (mirroring
/// typify, where named simple types vanish into their referents).
struct ResolveAliasPass;

impl Pass for ResolveAliasPass {
    fn name(&self) -> &'static str {
        "resolve-aliases"
    }

    fn run(&self, ir: &mut Ir, _cx: &PassCx<'_>) -> Result<()> {
        let snapshot = ir.clone();
        for def in &mut ir.types {
            match &mut def.shape {
                Shape::Struct(shape) => {
                    for field in &mut shape.fields {
                        field.ty = snapshot.resolve(&field.ty);
                    }
                }
                Shape::Untagged(shape) => {
                    for variant in &mut shape.variants {
                        variant.ty = snapshot.resolve(&variant.ty);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Wrap non-required, non-flatten struct fields in `Option<T>`
/// (`optional-fields = "always-option"`; the only emittable mode — the
/// config validator rejected `bare` before lowering).
struct OptionalityPass;

impl Pass for OptionalityPass {
    fn name(&self) -> &'static str {
        "optionality"
    }

    fn run(&self, ir: &mut Ir, _cx: &PassCx<'_>) -> Result<()> {
        for def in &mut ir.types {
            let Shape::Struct(shape) = &mut def.shape else {
                continue;
            };
            for field in &mut shape.fields {
                if !field.required && !field.flatten {
                    field.ty = std::mem::replace(&mut field.ty, TypeRef::Unit).optional();
                }
            }
        }
        Ok(())
    }
}

/// Apply `[fields."Type.wireName"] type = "..."` overrides. Runs after
/// optionality so an override replaces the *inner* type and keeps the
/// `Option` wrapper.
struct FieldOverridePass;

impl Pass for FieldOverridePass {
    fn name(&self) -> &'static str {
        "field-overrides"
    }

    fn run(&self, ir: &mut Ir, cx: &PassCx<'_>) -> Result<()> {
        for (selector, override_) in &cx.style.fields {
            let Some(type_path) = &override_.type_path else {
                continue;
            };
            let (type_name, field_name) = parse_field_selector(selector)?;
            let field = find_field(ir, &type_name, &field_name)
                .with_context(|| format!("field override selector {selector:?} matched nothing"))?;
            field.ty = match std::mem::replace(&mut field.ty, TypeRef::Unit) {
                TypeRef::Option(_) => {
                    TypeRef::Option(Box::new(TypeRef::Custom(type_path.clone())))
                }
                _ => TypeRef::Custom(type_path.clone()),
            };
        }
        Ok(())
    }
}

/// Selector `Type.field` (schema name / wire name).
fn parse_field_selector(selector: &str) -> Result<(String, String)> {
    selector
        .split_once('.')
        .map(|(ty, field)| (ty.to_string(), field.to_string()))
        .with_context(|| format!("field selector {selector:?} is not of the form Type.field"))
}

fn find_field<'a>(
    ir: &'a mut Ir,
    type_selector: &str,
    wire_name: &str,
) -> Option<&'a mut super::FieldDef> {
    let def = ir.types.iter_mut().find(|def| {
        def.schema_key.as_deref() == Some(type_selector) || def.name == type_selector
    })?;
    let Shape::Struct(shape) = &mut def.shape else {
        return None;
    };
    shape
        .fields
        .iter_mut()
        .find(|field| field.wire_name == wire_name || field.rust_name == wire_name)
}

/// Compute type-level and field-level serde attribute options:
/// `rename_all` with covered-rename elision, per-field renames,
/// `flatten`, `deny_unknown_fields`, and (when not elided) the
/// `default` / `skip_serializing_if` pair on `Option` fields.
struct SerdeSurfacePass;

impl Pass for SerdeSurfacePass {
    fn name(&self) -> &'static str {
        "serde-surface"
    }

    fn run(&self, ir: &mut Ir, cx: &PassCx<'_>) -> Result<()> {
        let rename_all = cx.style.rename_all.as_deref();
        for def in &mut ir.types {
            match &mut def.shape {
                Shape::Struct(shape) => {
                    if let Some(case) = rename_all {
                        def.serde_options.push(format!("rename_all = \"{case}\""));
                    }
                    if shape.deny_unknown_fields {
                        def.serde_options.push("deny_unknown_fields".to_string());
                    }
                    for field in &mut shape.fields {
                        if field.flatten {
                            field.serde_options.push("flatten".to_string());
                            continue;
                        }
                        let covered = rename_all.is_some_and(|case| {
                            rename_all_covers(&field.rust_name, &field.wire_name, case)
                        });
                        if field.rust_name != field.wire_name && !covered {
                            field
                                .serde_options
                                .push(format!("rename = \"{}\"", field.wire_name));
                        }
                        if matches!(field.ty, TypeRef::Option(_))
                            && !cx.style.elide_option_defaults
                        {
                            field.serde_options.push("default".to_string());
                            field.serde_options.push(
                                "skip_serializing_if = \"::std::option::Option::is_none\""
                                    .to_string(),
                            );
                        }
                    }
                }
                Shape::Untagged(_) => {
                    def.serde_options.push("untagged".to_string());
                }
                Shape::StringEnum(_) | Shape::Alias(_) => {}
            }
        }
        Ok(())
    }
}

/// Port of typify's `rename_all_covers_rename`: is the per-field rename
/// redundant under the struct-level `rename_all` case?
fn rename_all_covers(rust_field: &str, wire_name: &str, case: &str) -> bool {
    let transformed = match case {
        "lowercase" => rust_field.to_lowercase().replace('_', ""),
        "UPPERCASE" => rust_field.to_uppercase().replace('_', ""),
        "PascalCase" => rust_field.to_pascal_case(),
        "camelCase" => rust_field.to_lower_camel_case(),
        "snake_case" => rust_field.to_snake_case(),
        "SCREAMING_SNAKE_CASE" => rust_field.to_shouty_snake_case(),
        "kebab-case" => rust_field.to_kebab_case(),
        "SCREAMING-KEBAB-CASE" => rust_field.to_shouty_kebab_case(),
        _ => return false,
    };
    transformed == wire_name
}

/// Fill ordered derive lists and positioned attribute lines from the
/// config (`[style.derives]`, `[style.attrs]`, conditionals, and
/// per-type `derives-add`).
struct DeriveAttrPass;

impl Pass for DeriveAttrPass {
    fn name(&self) -> &'static str {
        "derives-attrs"
    }

    fn run(&self, ir: &mut Ir, cx: &PassCx<'_>) -> Result<()> {
        // Per-type extra derives, keyed by schema name; track matches so
        // unmatched selectors fail loudly.
        let mut unmatched: Vec<&str> = Vec::new();
        let mut extras: BTreeMap<&str, &[String]> = BTreeMap::new();
        for (selector, override_) in &cx.style.types {
            if override_.derives_add.is_empty() {
                continue;
            }
            let matched = ir.types.iter().any(|def| {
                def.schema_key.as_deref() == Some(selector) || def.name == *selector
            });
            if matched {
                extras.insert(selector.as_str(), &override_.derives_add);
            } else {
                unmatched.push(selector);
            }
        }
        if !unmatched.is_empty() {
            bail!("type override selectors matched nothing: {unmatched:?}");
        }

        for def in &mut ir.types {
            let is_struct = matches!(def.shape, Shape::Struct(_));
            let is_enum = matches!(def.shape, Shape::StringEnum(_) | Shape::Untagged(_));
            if !is_struct && !is_enum {
                continue;
            }
            let matches_kind = |kinds: KindFilter| {
                (is_struct && kinds.matches_struct()) || (is_enum && kinds.matches_enum())
            };

            let configured = if is_struct {
                &cx.style.derives.structs
            } else {
                &cx.style.derives.enums
            };
            let mut derives: Vec<String> = if configured.is_empty() {
                // Upstream base set, lexicographically sorted.
                let mut base = vec![
                    "::serde::Deserialize".to_string(),
                    "::serde::Serialize".to_string(),
                    "Clone".to_string(),
                    "Debug".to_string(),
                ];
                base.sort();
                base
            } else {
                configured.clone()
            };
            for (selector, extra) in &extras {
                let applies = def.schema_key.as_deref() == Some(*selector)
                    || def.name == **selector;
                if applies {
                    for derive in extra.iter() {
                        if !derives.contains(derive) {
                            derives.push(derive.clone());
                        }
                    }
                }
            }
            def.derives = derives;

            for entry in &cx.style.attrs {
                if !matches_kind(entry.kinds) {
                    continue;
                }
                match entry.position {
                    crate::config::AttrPosition::BeforeDerive => {
                        def.attrs_pre.push(entry.attr.clone())
                    }
                    crate::config::AttrPosition::AfterDerive => {
                        def.attrs_post.push(entry.attr.clone())
                    }
                }
            }
            for entry in &cx.style.conditional_derives {
                if matches_kind(entry.kinds) {
                    def.cond_derives
                        .push((entry.feature.clone(), entry.derive.clone()));
                }
            }
            for entry in &cx.style.conditional_attrs {
                if !matches_kind(entry.kinds) {
                    continue;
                }
                match entry.position {
                    crate::config::AttrPosition::BeforeDerive => def
                        .cond_attrs_pre
                        .push((entry.feature.clone(), entry.attr.clone())),
                    crate::config::AttrPosition::AfterDerive => def
                        .cond_attrs_post
                        .push((entry.feature.clone(), entry.attr.clone())),
                }
            }
        }
        Ok(())
    }
}

/// Attach synthesized impls: string-enum conversion blocks, `Default`
/// selection (schema default wins; `first-unit-variant` fills the rest),
/// and the untagged-enum `Default` that used to be `postprocess.rs`.
struct ImplSynthPass;

impl Pass for ImplSynthPass {
    fn name(&self) -> &'static str {
        "impl-synth"
    }

    fn run(&self, ir: &mut Ir, cx: &PassCx<'_>) -> Result<()> {
        for def in &mut ir.types {
            match &def.shape {
                Shape::StringEnum(shape) => {
                    def.impls.push(ImplSynth::SimpleEnumConversions);
                    if let Some(default_raw) = &shape.schema_default {
                        let variant = shape
                            .variants
                            .iter()
                            .find(|variant| &variant.raw_name == default_raw)
                            .expect("lowering validated the enum default");
                        def.impls
                            .push(ImplSynth::DefaultSchemaVariant(variant.ident_name.clone()));
                    } else if cx.style.enum_default == EnumDefaultMode::FirstUnitVariant
                        && let Some(first) = shape.variants.first()
                    {
                        def.impls
                            .push(ImplSynth::DefaultFirstVariant(first.ident_name.clone()));
                    }
                }
                Shape::Untagged(shape)
                    if cx.style.untagged_enum_defaults && !shape.variants.is_empty() =>
                {
                    def.impls.push(ImplSynth::DefaultUntaggedFirstVariant);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Annotate `Option<{Struct}>` (and `Option<Box<{Struct}>>`) fields with
/// `#[patch(name = "Option<InnerPatch>")]`, honoring per-field
/// overrides. Flatten bases and `Vec` fields never qualify.
struct DeepPatchPass;

impl Pass for DeepPatchPass {
    fn name(&self) -> &'static str {
        "deep-patch"
    }

    fn run(&self, ir: &mut Ir, cx: &PassCx<'_>) -> Result<()> {
        // Struct-shaped type names (post alias resolution).
        let struct_names: std::collections::BTreeSet<String> = ir
            .types
            .iter()
            .filter(|def| matches!(def.shape, Shape::Struct(_)))
            .map(|def| def.name.clone())
            .collect();

        // Per-field forced settings; validate selectors.
        let mut forced: BTreeMap<(String, String), bool> = BTreeMap::new();
        for (selector, override_) in &cx.style.fields {
            let Some(enabled) = override_.deep_patch else {
                continue;
            };
            let (type_name, field_name) = parse_field_selector(selector)?;
            if find_field(ir, &type_name, &field_name).is_none() {
                bail!("field override selector {selector:?} matched nothing");
            }
            forced.insert((type_name, field_name), enabled);
        }

        for def in &mut ir.types {
            let type_names: Vec<String> = def
                .schema_key
                .iter()
                .cloned()
                .chain(std::iter::once(def.name.clone()))
                .collect();
            let Shape::Struct(shape) = &mut def.shape else {
                continue;
            };
            for field in &mut shape.fields {
                if field.flatten {
                    continue;
                }
                let inner_struct = match &field.ty {
                    TypeRef::Option(inner) => match inner.as_ref() {
                        TypeRef::Named(name) => Some(name),
                        TypeRef::Boxed(boxed) => match boxed.as_ref() {
                            TypeRef::Named(name) => Some(name),
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                };
                let Some(inner) = inner_struct else { continue };
                if !struct_names.contains(inner) {
                    continue;
                }

                let force = type_names.iter().find_map(|type_name| {
                    forced
                        .get(&(type_name.clone(), field.wire_name.clone()))
                        .or_else(|| forced.get(&(type_name.clone(), field.rust_name.clone())))
                });
                let enabled = match force {
                    Some(&enabled) => enabled,
                    None => cx.style.deep_patch == DeepPatchMode::AllOptionStructs,
                };
                if enabled {
                    field.patch_type = Some(format!("Option<{inner}Patch>"));
                }
            }
        }
        Ok(())
    }
}

/// Assign each emitted type its module from the operation [`Partition`]
/// (schema-name keyed — no Rust-name bridge needed), route shared simple
/// enums to `shared/enums` in split mode (the decision that used to
/// need a post-typify fix-up phase), and apply `[types.*] module`
/// overrides.
struct PartitionPass;

impl Pass for PartitionPass {
    fn name(&self) -> &'static str {
        "partition"
    }

    fn run(&self, ir: &mut Ir, cx: &PassCx<'_>) -> Result<()> {
        let Some(partition) = cx.partition else {
            return Ok(());
        };

        let module_overrides: BTreeMap<&str, &str> = cx
            .style
            .types
            .iter()
            .filter_map(|(selector, override_)| {
                override_
                    .module
                    .as_deref()
                    .map(|module| (selector.as_str(), module))
            })
            .collect();
        for selector in module_overrides.keys() {
            let matched = ir.types.iter().any(|def| {
                def.schema_key.as_deref() == Some(*selector) || def.name == **selector
            });
            if !matched {
                bail!("type override selector {selector:?} matched nothing");
            }
        }

        let default_module = partition.default_module();
        for def in &mut ir.types {
            if !def.emits_item() {
                continue;
            }
            let assigned = def
                .schema_key
                .as_deref()
                .and_then(|key| partition.by_schema.get(key))
                .cloned()
                .unwrap_or_else(|| default_module.to_string());

            // Shared simple enums live together in split mode — the
            // shape is right here in the IR, no reach-back required.
            let assigned = if partition.split_request_response
                && def.is_simple_enum()
                && matches!(
                    assigned.as_str(),
                    crate::partition::SHARED_REQUEST_MODULE
                        | crate::partition::SHARED_RESPONSE_MODULE
                        | crate::partition::SHARED_COMMON_MODULE
                ) {
                crate::partition::SHARED_ENUMS_MODULE.to_string()
            } else {
                assigned
            };

            let assigned = def
                .schema_key
                .as_deref()
                .and_then(|key| module_overrides.get(key))
                .or_else(|| module_overrides.get(def.name.as_str()))
                .map(|module| module.to_string())
                .unwrap_or(assigned);

            def.module = Some(assigned);
        }
        Ok(())
    }
}

/// Build the per-module `use` preambles (trait imports + cross-module
/// globs, including the cross-role bridges) and record which modules
/// must materialize even when empty. Reuses
/// [`Partition::module_imports`] — the import *policy* has one home.
struct ImportsPass;

impl Pass for ImportsPass {
    fn name(&self) -> &'static str {
        "imports"
    }

    fn run(&self, ir: &mut Ir, cx: &PassCx<'_>) -> Result<()> {
        let trait_imports = parse_imports(&cx.style.imports)?;

        match cx.partition {
            Some(partition) => {
                let imports = partition.module_imports(&trait_imports);
                for (module, tokens) in imports {
                    ir.materialized_modules.insert(module.clone());
                    ir.module_imports.insert(module, tokens.to_string());
                }
                ir.materialized_modules
                    .insert(partition.default_module().to_string());
            }
            None => {
                ir.module_imports
                    .insert(String::new(), trait_imports.to_string());
            }
        }
        Ok(())
    }
}

/// Parse configured `use ...;` statements into one token stream.
fn parse_imports(imports: &[String]) -> Result<proc_macro2::TokenStream> {
    let mut tokens = proc_macro2::TokenStream::new();
    for statement in imports {
        let item: syn::ItemUse = syn::parse_str(statement)
            .with_context(|| format!("style import {statement:?} is not a valid use statement"))?;
        tokens.extend(quote::quote! { #item });
    }
    Ok(tokens)
}

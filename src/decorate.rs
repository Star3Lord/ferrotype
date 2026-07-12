//! The decoration pass: everything the old typify fork emitted through
//! its derive/attr/patch knobs, reproduced as an AST transformation over
//! the new fork's output.
//!
//! The current typify fork (see `FORK_FEATURES.md` there) deliberately
//! owns only *wire-shape* decisions — optionality, `allOf` composition,
//! open enums, conversions, docs. Decoration concerns operate on visible
//! syntax and live here instead, applied to the parsed [`syn::File`]
//! before every other post-pass so downstream machinery (overrides,
//! mappings, condense) sees exactly the shape the old fork produced:
//!
//! - the ordered per-kind derive lists (replacing typify's base set),
//!   plus per-type `derives-add` extras;
//! - unconditional / conditional attributes at their configured positions
//!   around the `#[derive(...)]`, and conditional derives;
//! - struct-level `#[serde(rename_all = "...")]` with covered per-field
//!   renames elided;
//! - the `default` + `skip_serializing_if = "Option::is_none"` elision on
//!   `Option<T>` fields (paired with a struct-level
//!   `#[serde_with::skip_serializing_none]` from the attr lists);
//! - deep-patch annotations (`#[patch(name = "Option<InnerPatch>")]`) on
//!   `Option<{generated struct}>` fields, driven by the same predicate
//!   that used to feed the fork's `with_deep_patch_filter`;
//! - the patch-companion naming mirror
//!   (`#[patch(attribute(serde(rename = ...)))]`) on renamed fields of
//!   `Patch`-deriving structs;
//! - `impl Default` for enums selecting their first unit variant
//!   (`enum-default = "first-unit-variant"`);
//! - the string-newtype convenience impls (`AsRef<str>` / `Display` /
//!   `From<&str>`).
//!
//! Item and attribute ordering deliberately mirror the old fork's
//! emission (`#doc #attrs #uncond_pre #cond_pre #cond_derives #derive
//! #serde #uncond_post #cond_post`), pinned by the checked-in goldens.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use quote::{ToTokens, quote};
use syn::punctuated::Punctuated;

use crate::Result;
use crate::config::{AttrPosition, EnumDefaultMode, KindFilter, StyleConfig};
use crate::idents::{rename_all_covers_rename, rust_type_ident};

/// Which generated type kinds an entry applies to, mirroring the old
/// fork's `TypeKind` classification: named-field structs are `Struct`,
/// tuple structs (typify's transparent wrappers) are `Newtype`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Struct,
    Enum,
    Newtype,
}

impl Kind {
    fn matches(self, filter: KindFilter) -> bool {
        match filter {
            KindFilter::All => true,
            KindFilter::Structs => self == Kind::Struct,
            KindFilter::Enums => self == Kind::Enum,
            KindFilter::Newtypes => self == Kind::Newtype,
        }
    }
}

/// Apply the style's decoration to every generated type in `file`.
/// `deep_patch` is the `(owner, field, inner) -> annotate?` predicate
/// (see [`crate::overrides::Overrides::deep_patch_filter_with_rules`]).
pub(crate) fn decorate_file(
    file: &mut syn::File,
    style: &StyleConfig,
    deep_patch: &dyn Fn(&str, &str, &str) -> bool,
) -> Result<()> {
    // Per-type extra derives, keyed by generated Rust name. When the
    // decoration replaces a kind's derive list these must be re-applied
    // (the replacement erases what `with_patch` contributed); when the
    // list is left alone, typify's own `with_patch` handling already
    // placed them.
    let derives_add: BTreeMap<String, &[String]> = style
        .types
        .iter()
        .filter(|(_, override_)| !override_.derives_add.is_empty())
        .map(|(selector, override_)| (rust_type_ident(selector), override_.derives_add.as_slice()))
        .collect();

    // Generated named-field struct names across the whole output — the
    // candidates for `Option<Inner>` deep-patch annotations (matching the
    // old fork's TypeEntryDetails::Struct check).
    let mut struct_names = BTreeSet::new();
    collect_struct_names(&file.items, &mut struct_names);

    let cx = Context_ {
        style,
        deep_patch,
        derives_add,
        struct_names,
    };
    walk_items(&mut file.items, &cx)
}

struct Context_<'a> {
    style: &'a StyleConfig,
    deep_patch: &'a dyn Fn(&str, &str, &str) -> bool,
    derives_add: BTreeMap<String, &'a [String]>,
    struct_names: BTreeSet<String>,
}

/// Modules whose contents are not typify-generated types: the shared
/// helper modules typify emits (`error`, `defaults`, `builder`) and the
/// client emitter's `client` module.
fn is_undecorated_mod(module: &syn::ItemMod) -> bool {
    module.ident == "error"
        || module.ident == "defaults"
        || module.ident == "builder"
        || module.ident == "client"
}

fn collect_struct_names(items: &[syn::Item], out: &mut BTreeSet<String>) {
    for item in items {
        match item {
            syn::Item::Mod(module) => {
                if is_undecorated_mod(module) {
                    continue;
                }
                if let Some((_, child_items)) = &module.content {
                    collect_struct_names(child_items, out);
                }
            }
            syn::Item::Struct(item_struct)
                if matches!(item_struct.fields, syn::Fields::Named(_)) =>
            {
                out.insert(item_struct.ident.to_string());
            }
            _ => {}
        }
    }
}

fn walk_items(items: &mut Vec<syn::Item>, cx: &Context_) -> Result<()> {
    // Recurse into nested partition modules first.
    for item in items.iter_mut() {
        if let syn::Item::Mod(module) = item {
            if is_undecorated_mod(module) {
                continue;
            }
            if let Some((_, child_items)) = &mut module.content {
                walk_items(child_items, cx)?;
            }
        }
    }

    // Decorate the type items of this module in place.
    for item in items.iter_mut() {
        match item {
            syn::Item::Struct(item_struct) => decorate_struct(item_struct, cx)?,
            syn::Item::Enum(item_enum) => decorate_enum(item_enum, cx)?,
            _ => {}
        }
    }

    // With `Default` in the struct or newtype derive list, typify's
    // hand-written `impl Default` blocks for those kinds (schema-level
    // defaults, or all-fields-defaultable synthesis) would conflict
    // with the derive; drop them, exactly as the old fork suppressed
    // them. Schema-provided field defaults are still honored at
    // deserialization time through the per-field `#[serde(default =
    // ...)]` attributes; what changes is that `Foo::default()` returns
    // the all-fields-default value.
    let drop_struct_defaults = cx
        .style
        .derives
        .structs
        .iter()
        .any(|derive| derive == "Default");
    let drop_newtype_defaults = cx
        .style
        .derives
        .newtypes
        .iter()
        .any(|derive| derive == "Default");
    if drop_struct_defaults || drop_newtype_defaults {
        let mut local_structs = BTreeSet::new();
        let mut local_newtypes = BTreeSet::new();
        for item in items.iter() {
            if let syn::Item::Struct(item_struct) = item {
                match item_struct.fields {
                    syn::Fields::Named(_) | syn::Fields::Unit => {
                        local_structs.insert(item_struct.ident.to_string());
                    }
                    syn::Fields::Unnamed(_) => {
                        local_newtypes.insert(item_struct.ident.to_string());
                    }
                }
            }
        }
        items.retain(|item| {
            let Some(name) = impl_self_name(item, "Default") else {
                return true;
            };
            !((drop_struct_defaults && local_structs.contains(&name))
                || (drop_newtype_defaults && local_newtypes.contains(&name)))
        });
    }

    // Synthesized items (enum first-variant Default, string-newtype
    // conveniences) splice into the module's item list; collect the
    // insertions first, then apply back-to-front so indices stay valid.
    let mut insertions: Vec<(usize, Vec<syn::Item>)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match item {
            syn::Item::Enum(item_enum)
                if cx.style.enum_default == EnumDefaultMode::FirstUnitVariant =>
            {
                if let Some(insertion) = enum_default_insertion(items, index, item_enum) {
                    insertions.push(insertion);
                }
            }
            syn::Item::Struct(item_struct) if cx.style.string_newtype_conveniences => {
                if let Some(insertion) = newtype_convenience_insertion(items, index, item_struct) {
                    insertions.push(insertion);
                }
            }
            _ => {}
        }
    }
    for (position, new_items) in insertions.into_iter().rev() {
        items.splice(position..position, new_items);
    }
    Ok(())
}

fn decorate_struct(item_struct: &mut syn::ItemStruct, cx: &Context_) -> Result<()> {
    let kind = match &item_struct.fields {
        syn::Fields::Named(_) | syn::Fields::Unit => Kind::Struct,
        syn::Fields::Unnamed(_) => Kind::Newtype,
    };
    let name = item_struct.ident.to_string();
    let has_patch_derive = decorate_type_attrs(
        &mut item_struct.attrs,
        kind,
        &name,
        // rename_all applies to structs only (matching the old fork's
        // struct-level emission; enums keep their variant renames).
        (kind == Kind::Struct)
            .then_some(cx.style.rename_all.as_deref())
            .flatten(),
        cx,
    )?;

    if let syn::Fields::Named(fields) = &mut item_struct.fields {
        for field in &mut fields.named {
            decorate_field(field, &name, has_patch_derive, cx)?;
        }
    }
    Ok(())
}

fn decorate_enum(item_enum: &mut syn::ItemEnum, cx: &Context_) -> Result<()> {
    let name = item_enum.ident.to_string();
    decorate_type_attrs(&mut item_enum.attrs, Kind::Enum, &name, None, cx)?;
    Ok(())
}

/// Rework a type's attribute stack: rewrite the derive list from the
/// configured per-kind ordering (plus per-type extras), inject the
/// struct-level `rename_all`, and insert the unconditional / conditional
/// attribute and derive lines at their configured positions. Returns
/// whether the final derive list carries `Patch`.
fn decorate_type_attrs(
    attrs: &mut Vec<syn::Attribute>,
    kind: Kind,
    type_name: &str,
    rename_all: Option<&str>,
    cx: &Context_,
) -> Result<bool> {
    let style = cx.style;
    let Some(derive_index) = attrs.iter().position(|attr| attr.path().is_ident("derive")) else {
        // Not a generated type shape (e.g. an item another pass owns).
        return Ok(false);
    };

    // 1. Derive-list rewrite (the old fork's `with_unconditional_derive_for`
    //    "caller-driven path": configured order verbatim, then per-type
    //    extras sorted; base set and kind intrinsics are replaced).
    let configured: &[String] = match kind {
        Kind::Struct => &style.derives.structs,
        Kind::Enum => &style.derives.enums,
        Kind::Newtype => &style.derives.newtypes,
    };
    if !configured.is_empty() {
        // Derives whose upstream emission was replaced by a bespoke impl
        // (e.g. `Deserialize` on constrained newtypes) are absent from
        // typify's list; re-adding them from config would conflict with
        // the manual impl. Deny serde derives the pre-rewrite list lacks.
        let existing = derive_paths(&attrs[derive_index]);
        let existing_short: BTreeSet<String> = existing
            .iter()
            .filter_map(|path| path.segments.last().map(|s| s.ident.to_string()))
            .collect();
        let denied = |derive: &str| {
            let short = derive.rsplit("::").next().unwrap_or(derive);
            matches!(short, "Serialize" | "Deserialize") && !existing_short.contains(short)
        };

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut list: Vec<&str> = Vec::new();
        for derive in configured {
            if !denied(derive) && seen.insert(derive) {
                list.push(derive);
            }
        }
        let extras: BTreeSet<&str> = cx
            .derives_add
            .get(type_name)
            .map(|extra| extra.iter().map(String::as_str).collect())
            .unwrap_or_default();
        for derive in extras {
            if !denied(derive) && seen.insert(derive) {
                list.push(derive);
            }
        }
        let paths: Vec<syn::Path> = list
            .iter()
            .map(|derive| {
                syn::parse_str::<syn::Path>(derive)
                    .with_context(|| format!("invalid derive path {derive:?} in style data"))
            })
            .collect::<Result<_>>()?;
        if let syn::Meta::List(meta) = &mut attrs[derive_index].meta {
            meta.tokens = quote! { #(#paths),* };
        }
    }

    // 2. Struct-level `rename_all`: appended to the type's `#[serde(...)]`
    //    attribute (created when absent), ordered [rename, rename_all,
    //    deny_unknown_fields] like the old fork's serde_options.
    if let Some(case) = rename_all {
        let serde_index = attrs.iter().position(|attr| attr.path().is_ident("serde"));
        match serde_index {
            Some(index) => {
                let mut metas = parse_metas(&attrs[index])?;
                let rename_position = metas
                    .iter()
                    .position(|meta| meta.path().is_ident("rename"))
                    .map(|position| position + 1)
                    .unwrap_or(0);
                metas.insert(rename_position, syn::parse_quote! { rename_all = #case });
                rewrite_metas(&mut attrs[index], &metas);
            }
            None => {
                attrs.insert(
                    derive_index + 1,
                    parse_attr(&format!("serde(rename_all = \"{case}\")"))?,
                );
            }
        }
    }

    let has_patch_derive = derive_paths(&attrs[derive_index]).iter().any(|path| {
        path.segments
            .last()
            .is_some_and(|segment| segment.ident == "Patch")
    });

    // 3. Attribute lines around the derive, in the old fork's order:
    //    ... existing attrs, uncond_pre, cond_pre, cond_derives,
    //    #[derive], #[serde], uncond_post, cond_post.
    let mut pre: Vec<syn::Attribute> = Vec::new();
    let mut post: Vec<syn::Attribute> = Vec::new();
    for entry in &style.attrs {
        if !kind.matches(entry.kinds) {
            continue;
        }
        let attr = parse_attr(&entry.attr)?;
        match entry.position {
            AttrPosition::BeforeDerive => pre.push(attr),
            AttrPosition::AfterDerive => post.push(attr),
        }
    }
    for entry in &style.conditional_attrs {
        if !kind.matches(entry.kinds) {
            continue;
        }
        let attr = parse_attr(&format!(
            "cfg_attr(feature = \"{}\", {})",
            entry.feature, entry.attr
        ))?;
        match entry.position {
            AttrPosition::BeforeDerive => pre.push(attr),
            AttrPosition::AfterDerive => post.push(attr),
        }
    }
    for entry in &style.conditional_derives {
        if !kind.matches(entry.kinds) {
            continue;
        }
        pre.push(parse_attr(&format!(
            "cfg_attr(feature = \"{}\", derive({}))",
            entry.feature, entry.derive
        ))?);
    }

    // The insertion points move as we insert; recompute them.
    let derive_index = attrs
        .iter()
        .position(|attr| attr.path().is_ident("derive"))
        .expect("derive attr still present");
    let post_index = attrs
        .get(derive_index + 1)
        .filter(|attr| attr.path().is_ident("serde"))
        .map(|_| derive_index + 2)
        .unwrap_or(derive_index + 1);
    attrs.splice(post_index..post_index, post);
    attrs.splice(derive_index..derive_index, pre);

    Ok(has_patch_derive)
}

fn decorate_field(
    field: &mut syn::Field,
    owner: &str,
    has_patch_derive: bool,
    cx: &Context_,
) -> Result<()> {
    let style = cx.style;
    let field_ident = field
        .ident
        .as_ref()
        .expect("named field has an ident")
        .to_string();

    // Edit the field's `#[serde(...)]` metas: elide renames the
    // struct-level `rename_all` covers, and drop the `default` +
    // `skip_serializing_if = "Option::is_none"` pair.
    let mut flatten = false;
    let mut surviving_rename: Option<String> = None;
    for attr_index in (0..field.attrs.len()).rev() {
        if !field.attrs[attr_index].path().is_ident("serde") {
            continue;
        }
        let mut metas = parse_metas(&field.attrs[attr_index])?;

        if let Some(case) = &style.rename_all {
            metas.retain(|meta| match rename_value(meta) {
                Some(wire_name) => !rename_all_covers_rename(&field_ident, &wire_name, case),
                None => true,
            });
        }
        if style.elide_option_defaults && has_option_elision_pair(&metas) {
            metas.retain(|meta| !is_bare_default(meta) && !is_option_skip(meta));
        }

        flatten |= metas.iter().any(|meta| meta.path().is_ident("flatten"));
        if surviving_rename.is_none() {
            surviving_rename = metas.iter().find_map(rename_value);
        }

        if metas.is_empty() {
            field.attrs.remove(attr_index);
        } else {
            rewrite_metas(&mut field.attrs[attr_index], &metas);
        }
    }

    // Deep-patch annotation: `Option<{generated struct}>` (through `Box`)
    // fields the predicate accepts, never `#[serde(flatten)]` bases.
    if !flatten
        && let Some(inner) = option_struct_inner(&field.ty, &cx.struct_names)
        && (cx.deep_patch)(owner, &field_ident, &inner)
    {
        let patch_name = format!("Option<{inner}Patch>");
        field
            .attrs
            .push(parse_attr(&format!("patch(name = \"{patch_name}\")"))?);
    }

    // Patch-companion naming mirror: a surviving field rename must repeat
    // on the companion, which does not inherit field serde attrs.
    if has_patch_derive && let Some(wire_name) = surviving_rename {
        field.attrs.push(parse_attr(&format!(
            "patch(attribute(serde(rename = \"{wire_name}\")))"
        ))?);
    }
    Ok(())
}

/// `impl Default` selecting the enum's first unit variant, inserted after
/// the enum's conversion-impl ladder (for all-unit / opened string enums)
/// or directly after the enum item (mixed-variant enums), matching the
/// old fork's item order. `None` when the enum has no unit variant or the
/// module already carries a `Default` impl for it (schema-level default).
fn enum_default_insertion(
    items: &[syn::Item],
    index: usize,
    item_enum: &syn::ItemEnum,
) -> Option<(usize, Vec<syn::Item>)> {
    let first_unit = item_enum
        .variants
        .iter()
        .find(|variant| matches!(variant.fields, syn::Fields::Unit))?;
    let type_name = &item_enum.ident;
    if items
        .iter()
        .any(|item| is_trait_impl_for(item, &type_name.to_string(), "Default"))
    {
        return None;
    }

    // The bespoke ladder exists only for simple (possibly opened) string
    // enums; its impls immediately follow the enum item.
    let is_ladder_enum = {
        let variants: Vec<_> = item_enum.variants.iter().collect();
        match variants.split_last() {
            Some((last, rest)) => {
                rest.iter()
                    .all(|variant| matches!(variant.fields, syn::Fields::Unit))
                    && (matches!(last.fields, syn::Fields::Unit)
                        || matches!(&last.fields, syn::Fields::Unnamed(fields)
                            if fields.unnamed.len() == 1))
            }
            None => false,
        }
    };
    let mut position = index + 1;
    if is_ladder_enum {
        const LADDER: &[&str] = &["Display", "FromStr", "TryFrom"];
        while position < items.len()
            && LADDER.iter().any(|trait_name| {
                is_trait_impl_for(&items[position], &type_name.to_string(), trait_name)
            })
        {
            position += 1;
        }
    }

    let variant_name = &first_unit.ident;
    let default_impl: syn::Item = syn::parse_quote! {
        impl ::std::default::Default for #type_name {
            fn default() -> Self {
                Self::#variant_name
            }
        }
    };
    Some((position, vec![default_impl]))
}

/// How a `#[serde(transparent)]` newtype over `String` came to be, read
/// off the shape of its `FromStr` impl. (typify treats native
/// `::std::string::String` conversion targets as strings too, so those
/// wrappers also carry the infallible `FromStr` and classify as
/// unconstrained.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringNewtypeKind {
    /// Plain string schema: infallible `FromStr`.
    Unconstrained,
    /// Validated string schema: `FromStr` returning `ConversionError`.
    Constrained,
    /// A `FromStr` shape this pass doesn't recognize (a `String`-named
    /// foreign inner, say): left untouched.
    Unrecognized,
}

/// Classify a transparent `String` newtype by its `FromStr` impl in the
/// same module; `None` when the item is not such a newtype (or carries
/// no `FromStr` to classify by).
fn string_newtype_kind(
    items: &[syn::Item],
    item_struct: &syn::ItemStruct,
) -> Option<StringNewtypeKind> {
    let syn::Fields::Unnamed(fields) = &item_struct.fields else {
        return None;
    };
    if fields.unnamed.len() != 1 {
        return None;
    }
    let inner = fields.unnamed.first()?;
    let is_string = matches!(&inner.ty, syn::Type::Path(type_path)
        if type_path.path.segments.last().is_some_and(|segment| segment.ident == "String"));
    let transparent = item_struct.attrs.iter().any(|attr| {
        attr.path().is_ident("serde") && attr.to_token_stream().to_string().contains("transparent")
    });
    if !is_string || !transparent {
        return None;
    }

    let name = item_struct.ident.to_string();
    let from_str = items
        .iter()
        .find(|item| is_trait_impl_for(item, &name, "FromStr"))?;
    let tokens = from_str.to_token_stream().to_string();
    if tokens.contains("Infallible") {
        Some(StringNewtypeKind::Unconstrained)
    } else if tokens.contains("ConversionError") {
        Some(StringNewtypeKind::Constrained)
    } else {
        Some(StringNewtypeKind::Unrecognized)
    }
}

/// The string-newtype convenience impls (`AsRef<str>`, `Display`,
/// `From<&str>` when unconstrained), inserted after the newtype's
/// intrinsic impls (`Deref`, `From<Self> for String`, a schema-default
/// `Default`) and before the constraint impls. Constrained newtypes get
/// only the read-side impls — construction must go through the
/// validating path; impls the module already carries (typify's own
/// `Display` proxy on unconstrained string newtypes) are never
/// duplicated.
fn newtype_convenience_insertion(
    items: &[syn::Item],
    index: usize,
    item_struct: &syn::ItemStruct,
) -> Option<(usize, Vec<syn::Item>)> {
    let kind = string_newtype_kind(items, item_struct)?;
    if kind == StringNewtypeKind::Unrecognized {
        return None;
    }

    let name = item_struct.ident.to_string();
    let type_name = &item_struct.ident;
    let mut new_items: Vec<syn::Item> = Vec::new();
    if !items
        .iter()
        .any(|item| is_trait_impl_for(item, &name, "AsRef"))
    {
        new_items.push(syn::parse_quote! {
            impl ::std::convert::AsRef<str> for #type_name {
                fn as_ref(&self) -> &str {
                    self.0.as_ref()
                }
            }
        });
    }
    if !items
        .iter()
        .any(|item| is_trait_impl_for(item, &name, "Display"))
    {
        new_items.push(syn::parse_quote! {
            impl ::std::fmt::Display for #type_name {
                fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    self.0.fmt(f)
                }
            }
        });
    }
    if kind == StringNewtypeKind::Unconstrained
        && !items.iter().any(|item| {
            is_trait_impl_for(item, &name, "From") && impl_trait_tokens(item).contains("& str")
        })
    {
        new_items.push(syn::parse_quote! {
            impl ::std::convert::From<&str> for #type_name {
                fn from(value: &str) -> Self {
                    Self(value.to_string())
                }
            }
        });
    }
    if new_items.is_empty() {
        return None;
    }

    // Skip past the newtype's intrinsic impls.
    let mut position = index + 1;
    while position < items.len() {
        let item = &items[position];
        let intrinsic = is_trait_impl_for(item, &name, "Deref")
            || is_trait_impl_for(item, &name, "Default")
            || (matches!(item, syn::Item::Impl(item_impl)
                if item_impl.trait_.as_ref().is_some_and(|(_, path, _)| {
                    path.segments.last().is_some_and(|segment| segment.ident == "From")
                }) && item_impl.to_token_stream().to_string().contains(&format!("From < {name} >"))));
        if intrinsic {
            position += 1;
        } else {
            break;
        }
    }
    Some((position, new_items))
}

/// The self-type name of `item` when it is an
/// `impl <...>::{trait_name}<...> for X` block.
fn impl_self_name(item: &syn::Item, trait_name: &str) -> Option<String> {
    let syn::Item::Impl(item_impl) = item else {
        return None;
    };
    let (_, trait_path, _) = item_impl.trait_.as_ref()?;
    if trait_path
        .segments
        .last()
        .is_none_or(|segment| segment.ident != trait_name)
    {
        return None;
    }
    match &*item_impl.self_ty {
        syn::Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

/// Whether `item` is `impl <...>::{trait_name}<...> for {type_name}`.
fn is_trait_impl_for(item: &syn::Item, type_name: &str, trait_name: &str) -> bool {
    let syn::Item::Impl(item_impl) = item else {
        return false;
    };
    let Some((_, trait_path, _)) = &item_impl.trait_ else {
        return false;
    };
    if trait_path
        .segments
        .last()
        .is_none_or(|segment| segment.ident != trait_name)
    {
        return false;
    }
    match &*item_impl.self_ty {
        syn::Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == type_name),
        _ => false,
    }
}

fn impl_trait_tokens(item: &syn::Item) -> String {
    match item {
        syn::Item::Impl(item_impl) => match &item_impl.trait_ {
            Some((_, path, _)) => path.to_token_stream().to_string(),
            None => String::new(),
        },
        _ => String::new(),
    }
}

/// The innermost struct name of an `Option<T>` / `Option<Box<T>>` field
/// type, when `T` names a generated named-field struct.
fn option_struct_inner(ty: &syn::Type, struct_names: &BTreeSet<String>) -> Option<String> {
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
    // A generated struct reference is a bare ident (possibly glob-imported
    // across modules); qualified paths name foreign types.
    if type_path.path.segments.len() != 1 {
        return None;
    }
    let name = type_path.path.segments.last()?.ident.to_string();
    struct_names.contains(&name).then_some(name)
}

/// The derive paths of a `#[derive(...)]` attribute.
fn derive_paths(attr: &syn::Attribute) -> Vec<syn::Path> {
    attr.parse_args_with(Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
        .map(|paths| paths.into_iter().collect())
        .unwrap_or_default()
}

/// Parse an attribute body (without the `#[...]` shell) into an
/// [`syn::Attribute`].
fn parse_attr(body: &str) -> Result<syn::Attribute> {
    let file: syn::File = syn::parse_str(&format!("#[{body}] struct X;"))
        .with_context(|| format!("attribute {body:?} in style data failed to parse"))?;
    let syn::Item::Struct(item) = file.items.into_iter().next().expect("one item") else {
        unreachable!("parsed a struct");
    };
    Ok(item.attrs.into_iter().next().expect("one attribute"))
}

fn parse_metas(attr: &syn::Attribute) -> Result<Vec<syn::Meta>> {
    Ok(attr
        .parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        .with_context(|| {
            format!(
                "failed to parse attribute arguments of `{}`",
                attr.to_token_stream()
            )
        })?
        .into_iter()
        .collect())
}

fn rewrite_metas(attr: &mut syn::Attribute, metas: &[syn::Meta]) {
    if let syn::Meta::List(meta) = &mut attr.meta {
        meta.tokens = quote! { #(#metas),* };
    }
}

/// The string value of a `rename = "..."` meta.
fn rename_value(meta: &syn::Meta) -> Option<String> {
    let syn::Meta::NameValue(name_value) = meta else {
        return None;
    };
    if !name_value.path.is_ident("rename") {
        return None;
    }
    match &name_value.value {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(value),
            ..
        }) => Some(value.value()),
        _ => None,
    }
}

fn is_bare_default(meta: &syn::Meta) -> bool {
    matches!(meta, syn::Meta::Path(path) if path.is_ident("default"))
}

fn is_option_skip(meta: &syn::Meta) -> bool {
    let syn::Meta::NameValue(name_value) = meta else {
        return false;
    };
    name_value.path.is_ident("skip_serializing_if")
        && matches!(&name_value.value, syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(value),
            ..
        }) if value.value() == "::std::option::Option::is_none")
}

fn has_option_elision_pair(metas: &[syn::Meta]) -> bool {
    metas.iter().any(is_bare_default) && metas.iter().any(is_option_skip)
}

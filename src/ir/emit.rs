//! `Ir → syn::File`: the types emitter.
//!
//! Reproduces the typify engine's output shape exactly (the parity
//! contract in docs/MIGRATION.md): per module — import preamble, nested
//! partition modules (name order), an `error` submodule with
//! `ConversionError` on every leaf, then items sorted by type name with
//! each type's impls immediately following it. Attribute layout per
//! item: doc, unconditional before-derive attrs, cfg-gated before-derive
//! attrs, cfg-gated derives, `#[derive(...)]`, type-level `#[serde]`,
//! unconditional after-derive attrs, cfg-gated after-derive attrs.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::Result;

use super::{FieldDef, ImplSynth, Ir, Shape, TypeDef, TypeRef, UntaggedShape};

/// Output sections within one module, in emission order (mirrors the
/// typify engine's `OutputSpaceMod`: Error < Crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Error,
    Crate,
}

/// One module's accumulated items.
#[derive(Default)]
struct ModuleSpace {
    items: BTreeMap<(Section, String), TokenStream>,
    has_types: bool,
}

impl ModuleSpace {
    fn add(&mut self, section: Section, order_hint: &str, tokens: TokenStream) {
        self.items
            .entry((section, order_hint.to_string()))
            .or_default()
            .extend(tokens);
    }

    fn into_stream(self) -> TokenStream {
        let mut error_items = TokenStream::new();
        let mut crate_items = TokenStream::new();
        for ((section, _), tokens) in self.items {
            match section {
                Section::Error => error_items.extend(tokens),
                Section::Crate => crate_items.extend(tokens),
            }
        }
        let error_mod = (!error_items.is_empty()).then(|| {
            quote! {
                /// Error types.
                pub mod error {
                    #error_items
                }
            }
        });
        quote! {
            #error_mod
            #crate_items
        }
    }
}

/// The module tree (slash-separated paths), mirroring the typify
/// engine's `ModuleNode`.
#[derive(Default)]
struct ModuleNode {
    children: BTreeMap<String, ModuleNode>,
    space: Option<ModuleSpace>,
}

impl ModuleNode {
    fn at_path(&mut self, path: &str) -> &mut ModuleNode {
        if path.is_empty() {
            return self;
        }
        let mut node = self;
        for segment in path.split('/') {
            node = node.children.entry(segment.to_string()).or_default();
        }
        node
    }

    fn fill_error_mods(&mut self, error_items: &TokenStream) {
        let is_leaf = self.children.is_empty();
        if let Some(space) = &mut self.space
            && (is_leaf || space.has_types)
        {
            space.add(Section::Error, "", error_items.clone());
        }
        for child in self.children.values_mut() {
            child.fill_error_mods(error_items);
        }
    }

    fn into_stream(
        self,
        path: String,
        imports: &BTreeMap<String, String>,
    ) -> Result<TokenStream> {
        let preamble: TokenStream = match imports.get(&path) {
            Some(text) => text
                .parse()
                .map_err(|error| anyhow::anyhow!("import preamble failed to parse: {error}"))?,
            None => TokenStream::new(),
        };
        let mut children = TokenStream::new();
        for (name, child) in self.children {
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let mod_ident = format_ident!("{}", name);
            let body = child.into_stream(child_path, imports)?;
            children.extend(quote! {
                pub mod #mod_ident {
                    #body
                }
            });
        }
        let body = self
            .space
            .map(ModuleSpace::into_stream)
            .unwrap_or_default();
        Ok(quote! {
            #preamble
            #children
            #body
        })
    }
}

/// Render the IR as the complete output token stream.
pub fn emit_tokens(ir: &Ir) -> Result<TokenStream> {
    let mut root = ModuleNode::default();

    // Materialize referenced-but-possibly-empty modules first.
    for module in &ir.materialized_modules {
        let node = root.at_path(module);
        node.space.get_or_insert_with(ModuleSpace::default);
    }
    // Flat mode: the top level acts as one module.
    if ir.types.iter().all(|def| def.module.is_none()) {
        root.space.get_or_insert_with(ModuleSpace::default);
    }

    for def in &ir.types {
        if !def.emits_item() {
            continue;
        }
        let module = def.module.clone().unwrap_or_default();
        let node = root.at_path(&module);
        let space = node.space.get_or_insert_with(ModuleSpace::default);
        space.has_types = true;
        let tokens = emit_type(ir, def).with_context(|| format!("emitting type {}", def.name))?;
        space.add(Section::Crate, &def.name, tokens);
    }

    root.fill_error_mods(&error_mod_items());
    root.into_stream(String::new(), &ir.module_imports)
}

/// Render the IR as a parsed [`syn::File`] (the artifact the writers
/// consume).
pub fn emit_single_file(ir: &Ir) -> Result<syn::File> {
    let tokens = emit_tokens(ir)?;
    syn::parse2(tokens).context("IR emitter produced tokens that do not parse as a Rust file")
}

/// The `ConversionError` items placed in every leaf `error` module —
/// token-for-token the typify engine's `fill_error_mod`.
fn error_mod_items() -> TokenStream {
    quote! {
        /// Error from a `TryFrom` or `FromStr` implementation.
        pub struct ConversionError(::std::borrow::Cow<'static, str>);

        impl ::std::error::Error for ConversionError {}
        impl ::std::fmt::Display for ConversionError {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>)
                -> Result<(), ::std::fmt::Error>
            {
                ::std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::std::fmt::Debug for ConversionError {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>)
                -> Result<(), ::std::fmt::Error>
            {
                ::std::fmt::Debug::fmt(&self.0, f)
            }
        }
        impl From<&'static str> for ConversionError {
            fn from(value: &'static str) -> Self {
                Self(value.into())
            }
        }
        impl From<String> for ConversionError {
            fn from(value: String) -> Self {
                Self(value.into())
            }
        }
    }
}

/// Render one named type: the item plus its synthesized impls.
fn emit_type(ir: &Ir, def: &TypeDef) -> Result<TokenStream> {
    let doc_text = def
        .description
        .clone()
        .unwrap_or_else(|| format!("`{}`", def.name));
    let doc = quote! { #[doc = #doc_text] };

    let attrs_pre = parse_attr_lines(&def.attrs_pre)?;
    let attrs_post = parse_attr_lines(&def.attrs_post)?;
    let cond_attrs_pre = parse_cond_attr_lines(&def.cond_attrs_pre)?;
    let cond_attrs_post = parse_cond_attr_lines(&def.cond_attrs_post)?;
    let cond_derives = def
        .cond_derives
        .iter()
        .map(|(feature, derive)| {
            let path: syn::Path = syn::parse_str(derive)
                .with_context(|| format!("conditional derive {derive:?} is not a valid path"))?;
            Ok(quote! { #[cfg_attr(feature = #feature, derive(#path))] })
        })
        .collect::<Result<Vec<_>>>()?;
    let derives = def
        .derives
        .iter()
        .map(|derive| {
            let path: syn::Path = syn::parse_str(derive)
                .with_context(|| format!("derive {derive:?} is not a valid path"))?;
            Ok(quote! { #path })
        })
        .collect::<Result<Vec<_>>>()?;
    let serde = serde_attr(&def.serde_options)?;

    let type_ident = format_ident!("{}", def.name);
    let body = match &def.shape {
        Shape::Struct(shape) => {
            let fields = shape
                .fields
                .iter()
                .map(emit_field)
                .collect::<Result<Vec<_>>>()?;
            quote! {
                pub struct #type_ident {
                    #(#fields)*
                }
            }
        }
        Shape::StringEnum(shape) => {
            let variants = shape.variants.iter().map(|variant| {
                let ident = format_ident!("{}", variant.ident_name);
                let doc = variant
                    .description
                    .as_ref()
                    .map(|text| quote! { #[doc = #text] });
                let rename = (variant.raw_name != variant.ident_name).then(|| {
                    let raw = &variant.raw_name;
                    quote! { #[serde(rename = #raw)] }
                });
                quote! {
                    #doc
                    #rename
                    #ident,
                }
            });
            quote! {
                pub enum #type_ident {
                    #(#variants)*
                }
            }
        }
        Shape::Untagged(shape) => {
            let variants = shape
                .variants
                .iter()
                .map(|variant| {
                    let ident = format_ident!("{}", variant.ident_name);
                    let doc = variant
                        .description
                        .as_ref()
                        .map(|text| quote! { #[doc = #text] });
                    let ty = type_tokens(&variant.ty)?;
                    Ok(quote! {
                        #doc
                        #ident(#ty),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            quote! {
                pub enum #type_ident {
                    #(#variants)*
                }
            }
        }
        Shape::Alias(_) => bail!("aliases do not emit items"),
    };

    let impls = def
        .impls
        .iter()
        .map(|synth| emit_impl(ir, def, synth))
        .collect::<Result<Vec<_>>>()?;

    Ok(quote! {
        #doc
        #(#attrs_pre)*
        #(#cond_attrs_pre)*
        #(#cond_derives)*
        #[derive(#(#derives),*)]
        #serde
        #(#attrs_post)*
        #(#cond_attrs_post)*
        #body
        #(#impls)*
    })
}

fn emit_field(field: &FieldDef) -> Result<TokenStream> {
    let doc = field
        .description
        .as_ref()
        .map(|text| quote! { #[doc = #text] });
    let serde = serde_attr(&field.serde_options)?;
    let patch = field
        .patch_type
        .as_ref()
        .map(|patch_type| quote! { #[patch(name = #patch_type)] });
    let name = format_ident!("{}", field.rust_name);
    let ty = type_tokens(&field.ty)?;
    Ok(quote! {
        #doc
        #serde
        #patch
        pub #name: #ty,
    })
}

/// `#[serde(a, b, c)]`, or nothing when there are no options.
fn serde_attr(options: &[String]) -> Result<Option<TokenStream>> {
    if options.is_empty() {
        return Ok(None);
    }
    let parsed = options
        .iter()
        .map(|option| {
            option
                .parse::<TokenStream>()
                .map_err(|error| anyhow::anyhow!("serde option {option:?}: {error}"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(quote! { #[serde( #(#parsed),* )] }))
}

fn parse_attr_lines(attrs: &[String]) -> Result<Vec<TokenStream>> {
    attrs
        .iter()
        .map(|attr| {
            let tokens: TokenStream = attr
                .parse()
                .map_err(|error| anyhow::anyhow!("attribute {attr:?}: {error}"))?;
            Ok(quote! { #[ #tokens ] })
        })
        .collect()
}

fn parse_cond_attr_lines(attrs: &[(String, String)]) -> Result<Vec<TokenStream>> {
    attrs
        .iter()
        .map(|(feature, attr)| {
            let tokens: TokenStream = attr
                .parse()
                .map_err(|error| anyhow::anyhow!("attribute {attr:?}: {error}"))?;
            Ok(quote! { #[cfg_attr(feature = #feature, #tokens)] })
        })
        .collect()
}

/// Render a [`TypeRef`] as a Rust type, with typify's exact path forms.
fn type_tokens(reference: &TypeRef) -> Result<TokenStream> {
    Ok(match reference {
        TypeRef::Named(name) => {
            let ident = format_ident!("{}", name);
            quote! { #ident }
        }
        TypeRef::String => quote! { ::std::string::String },
        TypeRef::Bool => quote! { bool },
        TypeRef::I32 => quote! { i32 },
        TypeRef::I64 => quote! { i64 },
        TypeRef::F64 => quote! { f64 },
        TypeRef::JsonValue => quote! { ::serde_json::Value },
        TypeRef::Unit => quote! { () },
        TypeRef::Custom(path) => {
            let ty: syn::Type = syn::parse_str(path)
                .with_context(|| format!("custom type path {path:?} does not parse"))?;
            quote! { #ty }
        }
        TypeRef::Option(inner) => {
            let inner = type_tokens(inner)?;
            quote! { ::std::option::Option<#inner> }
        }
        TypeRef::Vec(inner) => {
            let inner = type_tokens(inner)?;
            quote! { ::std::vec::Vec<#inner> }
        }
        TypeRef::Map(key, value) => {
            let key = type_tokens(key)?;
            let value = type_tokens(value)?;
            quote! { ::std::collections::HashMap<#key, #value> }
        }
        TypeRef::Boxed(inner) => {
            let inner = type_tokens(inner)?;
            quote! { ::std::boxed::Box<#inner> }
        }
    })
}

/// Render one synthesized impl block.
fn emit_impl(_ir: &Ir, def: &TypeDef, synth: &ImplSynth) -> Result<TokenStream> {
    let type_ident = format_ident!("{}", def.name);
    Ok(match synth {
        ImplSynth::SimpleEnumConversions => {
            let Shape::StringEnum(shape) = &def.shape else {
                bail!("SimpleEnumConversions on a non-enum type {}", def.name);
            };
            let (variant_idents, raw_names): (Vec<_>, Vec<_>) = shape
                .variants
                .iter()
                .map(|variant| {
                    (
                        format_ident!("{}", variant.ident_name),
                        variant.raw_name.clone(),
                    )
                })
                .unzip();
            quote! {
                impl ::std::fmt::Display for #type_ident {
                    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        match *self {
                            #(Self::#variant_idents => f.write_str(#raw_names),)*
                        }
                    }
                }
                impl ::std::str::FromStr for #type_ident {
                    type Err = self::error::ConversionError;

                    fn from_str(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        match value {
                            #(#raw_names => Ok(Self::#variant_idents),)*
                            _ => Err("invalid value".into()),
                        }
                    }
                }
                impl ::std::convert::TryFrom<&str> for #type_ident {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<&::std::string::String> for #type_ident {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<::std::string::String> for #type_ident {
                    type Error = self::error::ConversionError;

                    fn try_from(value: ::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
            }
        }
        ImplSynth::DefaultFirstVariant(variant) => {
            let variant_ident = format_ident!("{}", variant);
            quote! {
                impl ::std::default::Default for #type_ident {
                    fn default() -> Self {
                        Self::#variant_ident
                    }
                }
            }
        }
        ImplSynth::DefaultSchemaVariant(variant) => {
            let variant_ident = format_ident!("{}", variant);
            quote! {
                impl ::std::default::Default for #type_ident {
                    fn default() -> Self {
                        #type_ident::#variant_ident
                    }
                }
            }
        }
        ImplSynth::DefaultUntaggedFirstVariant => {
            let Shape::Untagged(UntaggedShape { variants }) = &def.shape else {
                bail!("DefaultUntaggedFirstVariant on non-untagged type {}", def.name);
            };
            let first = variants
                .first()
                .with_context(|| format!("untagged enum {} has no variants", def.name))?;
            let variant_ident = format_ident!("{}", first.ident_name);
            quote! {
                impl ::std::default::Default for #type_ident {
                    fn default() -> Self {
                        Self::#variant_ident(::std::default::Default::default())
                    }
                }
            }
        }
    })
}

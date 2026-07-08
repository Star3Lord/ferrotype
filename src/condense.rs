//! The `condensed` emit style: a token-verified AST transformation over
//! typify's output.
//!
//! typify's native ("expanded") shape buries types: every string enum
//! drags a ~50-line `Display`/`FromStr`/`TryFrom<&str>`/`TryFrom<&String>`/
//! `TryFrom<String>`(/`Default`) ladder behind it, and every partition
//! module duplicates the `error` module. [`condense_file`] keeps the
//! same trait surface but hoists the boilerplate:
//!
//! - one top-level `support` module per generation unit holds the single
//!   `error` module and a `macro_rules! impl_string_enum` whose expansion
//!   is token-identical to the ladder it replaces;
//! - each string enum's ladder becomes one
//!   `impl_string_enum!(Name { Variant => "wire", … } default = Variant);`
//!   invocation (open enums — the fork's `with_open_string_enums`
//!   catch-all — add an `open = Variant` clause before `default`);
//! - each module that duplicated `error` re-exports `support::error`
//!   instead, so every historical `<module>::error::ConversionError`
//!   path keeps resolving.
//!
//! Safety property: a ladder is only replaced after the macro's expansion
//! for the extracted pairs is verified **token-equal** to the impls being
//! removed (`Self::Variant` vs `TypeName::Variant` in `Default` bodies is
//! the one normalized difference); anything unrecognized is left expanded.
//! The transformation therefore cannot change behavior, only layout.
//!
//! Because prettyplease flows macro tokens into an unreadable wall,
//! rendering gets a token-verified post-pass ([`polish_rendered`]): the
//! macro definition is substituted with a pinned hand-formatted rendering
//! and invocations are reflowed one pair per line, each reflow re-parsed
//! and token-compared before acceptance. Both are no-ops for output
//! without the macro, so the expanded style is byte-untouched.

use anyhow::bail;
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};

use crate::Result;

/// The reserved top-level module name (one per generation unit).
const SUPPORT_MODULE: &str = "support";

/// Apply the condensed layout to a generated file in place.
pub(crate) fn condense_file(file: &mut syn::File) -> Result<()> {
    ensure_no_support_collision(file)?;

    let mut any_enum_macro = false;
    condense_items(&mut file.items, 0, &mut any_enum_macro);

    // Place `pub mod support { ... }`. Flat mode (the root itself held
    // the `error` mod, now a re-export): right after the re-export, so
    // support precedes the types like the error mod did. Partitioned
    // mode: after the last module declaration — the tree writer splits
    // it into `support.rs` like any partition module.
    let insert_at = file
        .items
        .iter()
        .position(is_support_error_reexport)
        .map(|index| index + 1)
        .unwrap_or_else(|| {
            file.items
                .iter()
                .rposition(|item| matches!(item, syn::Item::Mod(_)))
                .map(|index| index + 1)
                .unwrap_or(file.items.len())
        });
    let support: syn::ItemMod = syn::parse2(support_mod(any_enum_macro))
        .expect("support module tokens always parse");
    file.items.insert(insert_at, syn::Item::Mod(support));
    Ok(())
}

/// Is this item the `pub use ...::support::error;` re-export the
/// condensation left where the root `error` mod used to be?
fn is_support_error_reexport(item: &syn::Item) -> bool {
    let syn::Item::Use(item_use) = item else {
        return false;
    };
    item_use.to_token_stream().to_string().contains("support :: error")
}

/// The condensed style claims the `support` module name at the root of
/// the generation unit; a partition module (operation id) or module
/// override landing there would collide. Loud error over silent merge.
fn ensure_no_support_collision(file: &syn::File) -> Result<()> {
    for item in &file.items {
        if let syn::Item::Mod(module) = item
            && module.ident == SUPPORT_MODULE
        {
            bail!(
                "a generated module is named `support`, which the condensed emit \
                 style reserves for its shared helper module; rename the operation \
                 or use a `[types] module` override",
            );
        }
    }
    Ok(())
}

/// Recursively condense one module space (a `Vec<syn::Item>`): replace
/// string-enum ladders with `impl_string_enum!` invocations and the
/// duplicated `error` mod with a `support::error` re-export. `depth` is
/// the module's distance from the generation-unit root (where `support`
/// lives).
fn condense_items(items: &mut Vec<syn::Item>, depth: usize, any_enum_macro: &mut bool) {
    // Children first (their depth anchors differ).
    for item in items.iter_mut() {
        if let syn::Item::Mod(module) = item
            && let Some((_, child_items)) = &mut module.content
        {
            condense_items(child_items, depth + 1, any_enum_macro);
        }
    }

    let uses_enum_macro = condense_ladders(items);
    *any_enum_macro |= uses_enum_macro;

    // Replace this space's `error` mod — only ever the standard one —
    // with the support re-export (plus the macro import when an
    // invocation follows). The `use` lines take the error mod's
    // position, keeping the historical layout: preamble, error, types.
    let error_index = items.iter().position(|item| {
        matches!(item, syn::Item::Mod(module) if module.ident == "error"
            && is_standard_error_mod(module))
    });
    let Some(index) = error_index else {
        return;
    };
    items.remove(index);

    // `use` paths are crate-rooted in edition 2018+; anchor the in-unit
    // path explicitly at every depth.
    let path: TokenStream = if depth == 0 {
        format!("self::{SUPPORT_MODULE}")
    } else {
        format!("{}{SUPPORT_MODULE}", "super::".repeat(depth))
    }
    .parse()
    .expect("support-module path always parses");

    let mut replacement: Vec<syn::Item> = Vec::new();
    if uses_enum_macro {
        replacement.push(syn::parse2(quote! { use #path::impl_string_enum; }).unwrap());
    }
    replacement.push(syn::parse2(quote! { pub use #path::error; }).unwrap());
    items.splice(index..index, replacement);
}

/// Find every string-enum conversion ladder among `items` and replace
/// each with a single macro invocation. Returns whether any invocation
/// was created.
fn condense_ladders(items: &mut Vec<syn::Item>) -> bool {
    let enums: Vec<syn::ItemEnum> = items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Enum(item_enum) => Some(item_enum.clone()),
            _ => None,
        })
        .collect();

    let mut condensed_any = false;
    for item_enum in enums {
        condensed_any |= condense_one_ladder(items, &item_enum);
    }
    condensed_any
}

/// Try to condense the ladder of one enum. The (variant, wire) pairs are
/// extracted from the `Display` impl — the authoritative source — and
/// the replacement only happens when the full expected impl set is
/// present and token-equal to what the macro would expand to.
fn condense_one_ladder(items: &mut Vec<syn::Item>, item_enum: &syn::ItemEnum) -> bool {
    let type_name = item_enum.ident.to_string();
    // A closed string enum is all unit variants; an open one (the
    // fork's `with_open_string_enums`) additionally carries a trailing
    // one-field tuple catch-all. Anything else is not a string enum.
    let Some(open_variant) = string_enum_catch_all(item_enum) else {
        return false;
    };

    // Locate this enum's Display impl and extract the pairs.
    let Some(display_index) = find_trait_impl(items, &type_name, &["fmt", "Display"]) else {
        return false;
    };
    let Some(pairs) = extract_display_pairs(
        impl_at(items, display_index),
        &item_enum.ident,
        open_variant.as_ref(),
    ) else {
        return false;
    };

    // Build the expected ladder from the extracted pairs and require a
    // token-equal match for every rung.
    let type_ident = &item_enum.ident;
    let expected = expected_ladder(type_ident, &pairs, open_variant.as_ref());
    let mut ladder_indices = vec![display_index];
    for expected_impl in expected.iter().skip(1) {
        let found = items.iter().enumerate().position(|(index, item)| {
            !ladder_indices.contains(&index)
                && matches!(item, syn::Item::Impl(_))
                && tokens_equal(item.to_token_stream(), expected_impl.clone())
        });
        let Some(index) = found else {
            return false;
        };
        ladder_indices.push(index);
    }
    if !tokens_equal(
        items[display_index].to_token_stream(),
        expected[0].clone(),
    ) {
        return false;
    }

    // Optional Default rung: accept both `Self::Variant` (the
    // first-unit-variant knob's shape) and `TypeName::Variant` (typify's
    // schema-default shape); the macro expands to the latter —
    // semantically identical, deliberately normalized (this style is off
    // the byte-parity path by definition).
    let mut default_variant: Option<syn::Ident> = None;
    if let Some(default_index) = find_trait_impl(items, &type_name, &["default", "Default"]) {
        let Some(variant) = extract_default_variant(impl_at(items, default_index), type_ident)
        else {
            return false;
        };
        ladder_indices.push(default_index);
        default_variant = Some(variant);
    }

    // Replace: the invocation takes the first rung's position; the rest
    // are removed.
    let variant_idents = pairs.iter().map(|(variant, _)| variant);
    let raws = pairs.iter().map(|(_, raw)| raw);
    let open_clause = open_variant.map(|variant| quote! { open = #variant });
    let default_clause = default_variant.map(|variant| quote! { default = #variant });
    // Trailing comma inside the repetition: the reflowed pretty form
    // (`polish_invocations`) writes one, and its token-fidelity gate
    // compares against these tokens.
    let invocation: syn::Item = syn::parse2(quote! {
        impl_string_enum!(#type_ident { #(#variant_idents => #raws,)* } #open_clause #default_clause);
    })
    .expect("macro invocation tokens always parse");

    ladder_indices.sort_unstable();
    let first = ladder_indices[0];
    for index in ladder_indices.iter().rev() {
        items.remove(*index);
    }
    items.insert(first, invocation);
    true
}

fn impl_at(items: &[syn::Item], index: usize) -> &syn::ItemImpl {
    match &items[index] {
        syn::Item::Impl(item_impl) => item_impl,
        _ => unreachable!("index always points at an impl item"),
    }
}

/// Position of the impl of trait `trait_name` (last path segment) for
/// `type_name` among `items`. `probe` names a method the impl must
/// contain, guarding against inherent impls.
fn find_trait_impl(items: &[syn::Item], type_name: &str, probe: &[&str; 2]) -> Option<usize> {
    items.iter().position(|item| {
        let syn::Item::Impl(item_impl) = item else {
            return false;
        };
        let Some((_, trait_path, _)) = &item_impl.trait_ else {
            return false;
        };
        trait_path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == probe[1])
            && impl_self_ty_name(item_impl).as_deref() == Some(type_name)
            && item_impl.items.iter().any(|impl_item| {
                matches!(impl_item, syn::ImplItem::Fn(f) if f.sig.ident == probe[0])
            })
    })
}

fn impl_self_ty_name(item_impl: &syn::ItemImpl) -> Option<String> {
    match item_impl.self_ty.as_ref() {
        syn::Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

/// `Some(None)` for a closed string enum (all unit variants),
/// `Some(Some(ident))` for an open one — all units plus a trailing
/// one-field tuple catch-all (the fork's `with_open_string_enums`
/// shape) — and `None` for anything that is not a string enum.
fn string_enum_catch_all(item_enum: &syn::ItemEnum) -> Option<Option<syn::Ident>> {
    let variants: Vec<&syn::Variant> = item_enum.variants.iter().collect();
    let (last, units) = variants.split_last()?;
    if units
        .iter()
        .any(|variant| !matches!(variant.fields, syn::Fields::Unit))
    {
        return None;
    }
    match &last.fields {
        syn::Fields::Unit => Some(None),
        syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            Some(Some(last.ident.clone()))
        }
        _ => None,
    }
}

/// Extract `(Variant, "wire")` pairs from a `Display` impl of the shape
/// the ladder uses: `match *self { Self::V => f.write_str("raw"), … }`.
/// For an open enum the final arm is the catch-all
/// (`Self::Other(value) => f.write_str(value.as_str())`); it contributes
/// no pair and must name `open_variant` — the token-equality gate
/// against [`expected_ladder`] then pins its exact shape.
fn extract_display_pairs(
    item_impl: &syn::ItemImpl,
    type_ident: &syn::Ident,
    open_variant: Option<&syn::Ident>,
) -> Option<Vec<(syn::Ident, syn::LitStr)>> {
    let fmt_fn = item_impl.items.iter().find_map(|item| match item {
        syn::ImplItem::Fn(f) if f.sig.ident == "fmt" => Some(f),
        _ => None,
    })?;
    let syn::Stmt::Expr(syn::Expr::Match(match_expr), _) = fmt_fn.block.stmts.first()? else {
        return None;
    };

    let mut pairs = Vec::with_capacity(match_expr.arms.len());
    for (index, arm) in match_expr.arms.iter().enumerate() {
        // Open enums: the catch-all is the final arm, a tuple pattern
        // naming the catch-all variant.
        if let syn::Pat::TupleStruct(tuple) = &arm.pat {
            let is_last = index + 1 == match_expr.arms.len();
            let names_catch_all = open_variant.is_some_and(|open| {
                tuple
                    .path
                    .segments
                    .last()
                    .is_some_and(|segment| &segment.ident == open)
            });
            if is_last && names_catch_all {
                continue;
            }
            return None;
        }
        // Pattern: `Self::Variant` or `TypeName::Variant`.
        let syn::Pat::Path(pat_path) = &arm.pat else {
            return None;
        };
        let segments: Vec<String> = pat_path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        let variant = match segments.as_slice() {
            [qualifier, variant]
                if qualifier == "Self" || type_ident == qualifier.as_str() =>
            {
                format_ident!("{variant}")
            }
            _ => return None,
        };
        // Body: `f.write_str("raw")`.
        let syn::Expr::MethodCall(call) = arm.body.as_ref() else {
            return None;
        };
        if call.method != "write_str" || call.args.len() != 1 {
            return None;
        }
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(raw),
            ..
        }) = call.args.first()?
        else {
            return None;
        };
        pairs.push((variant, raw.clone()));
    }
    Some(pairs)
}

/// Extract the variant of a `Default` impl of the shape
/// `fn default() -> Self { Self::Variant }` (or `TypeName::Variant`).
fn extract_default_variant(
    item_impl: &syn::ItemImpl,
    type_ident: &syn::Ident,
) -> Option<syn::Ident> {
    let default_fn = item_impl.items.iter().find_map(|item| match item {
        syn::ImplItem::Fn(f) if f.sig.ident == "default" => Some(f),
        _ => None,
    })?;
    let syn::Stmt::Expr(syn::Expr::Path(expr_path), None) = default_fn.block.stmts.first()?
    else {
        return None;
    };
    let segments: Vec<String> = expr_path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    match segments.as_slice() {
        [qualifier, variant] if qualifier == "Self" || type_ident == qualifier.as_str() => {
            Some(format_ident!("{variant}"))
        }
        _ => None,
    }
}

/// The five ladder impls the macro expands to for the given pairs, as
/// token streams (Display first — its index anchors the comparison).
/// With `open_variant`, the Display and FromStr rungs take the open
/// shape: `match self` with a catch-all write arm, and an irrefutable
/// FromStr.
fn expected_ladder(
    type_ident: &syn::Ident,
    pairs: &[(syn::Ident, syn::LitStr)],
    open_variant: Option<&syn::Ident>,
) -> Vec<TokenStream> {
    let variants: Vec<&syn::Ident> = pairs.iter().map(|(variant, _)| variant).collect();
    let raws: Vec<&syn::LitStr> = pairs.iter().map(|(_, raw)| raw).collect();
    let display_scrutinee = match open_variant {
        Some(_) => quote! { self },
        None => quote! { *self },
    };
    let display_fallback = open_variant.map(|open| {
        quote! { Self::#open(value) => f.write_str(value.as_str()), }
    });
    let from_str_fallback = match open_variant {
        Some(open) => quote! { _ => Ok(Self::#open(value.to_string())), },
        None => quote! { _ => Err("invalid value".into()), },
    };
    vec![
        quote! {
            impl ::std::fmt::Display for #type_ident {
                fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    match #display_scrutinee {
                        #(Self::#variants => f.write_str(#raws),)*
                        #display_fallback
                    }
                }
            }
        },
        quote! {
            impl ::std::str::FromStr for #type_ident {
                type Err = self::error::ConversionError;

                fn from_str(value: &str) ->
                    ::std::result::Result<Self, self::error::ConversionError>
                {
                    match value {
                        #(#raws => Ok(Self::#variants),)*
                        #from_str_fallback
                    }
                }
            }
        },
        quote! {
            impl ::std::convert::TryFrom<&str> for #type_ident {
                type Error = self::error::ConversionError;

                fn try_from(value: &str) ->
                    ::std::result::Result<Self, self::error::ConversionError>
                {
                    value.parse()
                }
            }
        },
        quote! {
            impl ::std::convert::TryFrom<&::std::string::String> for #type_ident {
                type Error = self::error::ConversionError;

                fn try_from(value: &::std::string::String) ->
                    ::std::result::Result<Self, self::error::ConversionError>
                {
                    value.parse()
                }
            }
        },
        quote! {
            impl ::std::convert::TryFrom<::std::string::String> for #type_ident {
                type Error = self::error::ConversionError;

                fn try_from(value: ::std::string::String) ->
                    ::std::result::Result<Self, self::error::ConversionError>
                {
                    value.parse()
                }
            }
        },
    ]
}

/// The `ConversionError` items placed in every `error` module —
/// token-for-token typify's `fill_error_mod`.
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

/// Is this module the standard generated `error` mod (and safe to
/// replace with a re-export)? Token-compared against the canonical
/// items; anything else — user-injected or drifted — is left alone.
fn is_standard_error_mod(module: &syn::ItemMod) -> bool {
    let Some((_, items)) = &module.content else {
        return false;
    };
    let actual = quote! { #(#items)* };
    tokens_equal(actual, error_mod_items())
}

/// The condensed style's `support` module: the one shared `error`
/// module, plus — when any string enum was condensed — the
/// `impl_string_enum!` macro and its path-import re-export
/// (`pub(crate) use`, the standard `macro_rules!` cross-module pattern;
/// invocations reach it via `use <supers>::support::impl_string_enum;`
/// regardless of module depth or textual order).
fn support_mod(include_enum_macro: bool) -> TokenStream {
    let error_items = error_mod_items();
    let enum_macro = include_enum_macro.then(impl_string_enum_macro);
    quote! {
        /// Shared support items for the generated modules: the
        /// conversion error type and the impl-condensing macros, defined
        /// once instead of per module. Sibling modules re-export
        /// [`error`], so `<module>::error::ConversionError` paths keep
        /// resolving.
        pub mod support {
            /// Error types.
            pub mod error {
                #error_items
            }
            #enum_macro
        }
    }
}

/// The `impl_string_enum!` definition. Each expansion is exactly the
/// impl set the expanded style writes out per string enum — same trait
/// surface, same `self::error::ConversionError` paths (resolving
/// through the invoking module's `error` re-export), same bodies — so
/// the two styles differ only in layout. (`tests/emit_style.rs` pins the
/// expansion token-equal to [`expected_ladder`] and the pretty rendering
/// token-equal to this definition, so none of the three can drift.)
pub(crate) fn impl_string_enum_macro() -> TokenStream {
    quote! {
        /// Implements the wire-format conversions for a string enum:
        /// `Display` and `FromStr` over the `Variant => "wire value"`
        /// pairs (erring with `self::error::ConversionError`), the
        /// `TryFrom<&str>` / `TryFrom<&String>` / `TryFrom<String>`
        /// ladder via `FromStr`, and — when a `default = Variant`
        /// clause is present — `Default`. With an `open = Variant`
        /// clause (an open enum's untagged catch-all), `Display`
        /// writes the carried string and `FromStr` is irrefutable —
        /// unknown input parses into the catch-all.
        macro_rules! impl_string_enum {
            ($Type:ident { $($variant:ident => $raw:literal),* $(,)? } open = $open:ident $(default = $default:ident)?) => {
                impl ::std::fmt::Display for $Type {
                    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        match self {
                            $(Self::$variant => f.write_str($raw),)*
                            Self::$open(value) => f.write_str(value.as_str()),
                        }
                    }
                }
                impl ::std::str::FromStr for $Type {
                    type Err = self::error::ConversionError;

                    fn from_str(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        match value {
                            $($raw => Ok(Self::$variant),)*
                            _ => Ok(Self::$open(value.to_string())),
                        }
                    }
                }
                impl ::std::convert::TryFrom<&str> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<&::std::string::String> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<::std::string::String> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: ::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                $(
                    impl ::std::default::Default for $Type {
                        fn default() -> Self {
                            $Type::$default
                        }
                    }
                )?
            };
            ($Type:ident { $($variant:ident => $raw:literal),* $(,)? } $(default = $default:ident)?) => {
                impl ::std::fmt::Display for $Type {
                    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        match *self {
                            $(Self::$variant => f.write_str($raw),)*
                        }
                    }
                }
                impl ::std::str::FromStr for $Type {
                    type Err = self::error::ConversionError;

                    fn from_str(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        match value {
                            $($raw => Ok(Self::$variant),)*
                            _ => Err("invalid value".into()),
                        }
                    }
                }
                impl ::std::convert::TryFrom<&str> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<&::std::string::String> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<::std::string::String> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: ::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                $(
                    impl ::std::default::Default for $Type {
                        fn default() -> Self {
                            $Type::$default
                        }
                    }
                )?
            };
        }
        pub(crate) use impl_string_enum;
    }
}

/// The hand-formatted rendering of [`impl_string_enum_macro`]'s
/// `macro_rules!` item (at zero indent), substituted into rendered
/// output by [`polish_rendered`]. prettyplease cannot format
/// `macro_rules!` bodies (it flows them as raw tokens), and the macro
/// definition is precisely the place a reader goes to see what the
/// invocations implement — so it must read like the expanded impls it
/// replaces. `tests/emit_style.rs` pins this text token-identical to
/// the emitted macro, so the two cannot drift apart.
pub(crate) const IMPL_STRING_ENUM_MACRO_PRETTY: &str = r#"macro_rules! impl_string_enum {
    ($Type:ident { $($variant:ident => $raw:literal),* $(,)? } open = $open:ident $(default = $default:ident)?) => {
        impl ::std::fmt::Display for $Type {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                match self {
                    $(Self::$variant => f.write_str($raw),)*
                    Self::$open(value) => f.write_str(value.as_str()),
                }
            }
        }
        impl ::std::str::FromStr for $Type {
            type Err = self::error::ConversionError;
            fn from_str(
                value: &str
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                match value {
                    $($raw => Ok(Self::$variant),)*
                    _ => Ok(Self::$open(value.to_string())),
                }
            }
        }
        impl ::std::convert::TryFrom<&str> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: &str
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        impl ::std::convert::TryFrom<&::std::string::String> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: &::std::string::String
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        impl ::std::convert::TryFrom<::std::string::String> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: ::std::string::String
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        $(
            impl ::std::default::Default for $Type {
                fn default() -> Self {
                    $Type::$default
                }
            }
        )?
    };
    ($Type:ident { $($variant:ident => $raw:literal),* $(,)? } $(default = $default:ident)?) => {
        impl ::std::fmt::Display for $Type {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                match *self {
                    $(Self::$variant => f.write_str($raw),)*
                }
            }
        }
        impl ::std::str::FromStr for $Type {
            type Err = self::error::ConversionError;
            fn from_str(
                value: &str
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                match value {
                    $($raw => Ok(Self::$variant),)*
                    _ => Err("invalid value".into()),
                }
            }
        }
        impl ::std::convert::TryFrom<&str> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: &str
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        impl ::std::convert::TryFrom<&::std::string::String> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: &::std::string::String
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        impl ::std::convert::TryFrom<::std::string::String> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: ::std::string::String
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        $(
            impl ::std::default::Default for $Type {
                fn default() -> Self {
                    $Type::$default
                }
            }
        )?
    };
}"#;

/// Post-render fix-up applied to every formatted output document.
/// prettyplease flows macro tokens by line width, which turns both the
/// `macro_rules! impl_string_enum` definition and multi-variant
/// invocations into walls of wrapped tokens — the opposite of what the
/// condensed style is for. This pass re-renders both readably:
///
/// - the definition is replaced with the hand-formatted
///   [`IMPL_STRING_ENUM_MACRO_PRETTY`] (token-identical; pinned by
///   `tests/emit_style.rs`);
/// - every `impl_string_enum!(...)` invocation is reflowed to one
///   `Variant => "wire"` pair per line, and the result is re-parsed and
///   token-compared against the original before being accepted.
///
/// No-op for output without the macro (the expanded style).
pub(crate) fn polish_rendered(source: String) -> String {
    if !source.contains("impl_string_enum") {
        return source;
    }
    polish_invocations(polish_macro_def(source))
}

/// Replace the flowed `macro_rules!` item with the pretty rendering,
/// re-indented to wherever the item sits (depth 1 in single-file
/// output, depth 0 in the tree writer's `support.rs`).
fn polish_macro_def(source: String) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let Some((start, indent)) = lines.iter().enumerate().find_map(|(index, line)| {
        line.trim_start()
            .eq("macro_rules! impl_string_enum {")
            .then(|| (index, &line[..line.len() - line.trim_start().len()]))
    }) else {
        return source;
    };
    let closer = format!("{indent}}}");
    let Some(end) = lines[start + 1..]
        .iter()
        .position(|line| *line == closer)
        .map(|offset| start + 1 + offset)
    else {
        return source;
    };

    let mut polished: Vec<String> = lines[..start].iter().map(|s| s.to_string()).collect();
    polished.extend(
        IMPL_STRING_ENUM_MACRO_PRETTY
            .lines()
            .map(|line| format!("{indent}{line}")),
    );
    polished.extend(lines[end + 1..].iter().map(|s| s.to_string()));
    finish_lines(polished, &source)
}

/// Reflow every `impl_string_enum!(...)` invocation to one variant pair
/// per line. Token-verified: the reflowed text must re-parse to the
/// exact same token stream, otherwise the original rendering is kept.
fn polish_invocations(source: String) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut polished: Vec<String> = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if !line.trim_start().starts_with("impl_string_enum!(") {
            polished.push(line.to_string());
            index += 1;
            continue;
        }
        let indent = &line[..line.len() - line.trim_start().len()];
        // Find the invocation's end: the first line completing a
        // parsable macro item (invocations are statement-terminated
        // with `);`).
        let mut end = index;
        let item = loop {
            if end >= lines.len() {
                break None;
            }
            if lines[end].trim_end().ends_with(");") {
                let candidate = lines[index..=end].join("\n");
                if let Ok(item) = syn::parse_str::<syn::ItemMacro>(&candidate) {
                    break Some(item);
                }
            }
            end += 1;
        };
        let Some(item) = item else {
            polished.push(line.to_string());
            index += 1;
            continue;
        };
        match reflow_invocation(&item, indent) {
            Some(reflowed) => polished.extend(reflowed),
            None => polished.extend(lines[index..=end].iter().map(|s| s.to_string())),
        }
        index = end + 1;
    }
    finish_lines(polished, &source)
}

/// Render one parsed invocation as `Type {` / one pair per line /
/// `} default = Variant);`, verifying token fidelity.
fn reflow_invocation(item: &syn::ItemMacro, indent: &str) -> Option<Vec<String>> {
    use proc_macro2::TokenTree;

    let tokens: Vec<TokenTree> = item.mac.tokens.clone().into_iter().collect();
    // Expected shape: Ident, Brace group, then optionally
    // `default` `=` Ident.
    let (type_ident, rest) = match tokens.split_first()? {
        (TokenTree::Ident(ident), rest) => (ident.clone(), rest),
        _ => return None,
    };
    let (group, rest) = match rest.split_first()? {
        (TokenTree::Group(group), rest)
            if group.delimiter() == proc_macro2::Delimiter::Brace =>
        {
            (group.clone(), rest)
        }
        _ => return None,
    };
    // Trailing clauses, in canonical order: `open = Ident` then
    // `default = Ident`, each at most once.
    let mut open_variant: Option<proc_macro2::Ident> = None;
    let mut default_variant: Option<proc_macro2::Ident> = None;
    let mut rest = rest;
    while !rest.is_empty() {
        match rest {
            [
                TokenTree::Ident(kw),
                TokenTree::Punct(eq),
                TokenTree::Ident(variant),
                tail @ ..,
            ] if eq.as_char() == '=' => {
                if kw == "open" && open_variant.is_none() && default_variant.is_none() {
                    open_variant = Some(variant.clone());
                } else if kw == "default" && default_variant.is_none() {
                    default_variant = Some(variant.clone());
                } else {
                    return None;
                }
                rest = tail;
            }
            _ => return None,
        }
    }

    // Pairs: Ident, '=>' (joint '=' + '>'), Literal, [','].
    let mut pairs: Vec<(proc_macro2::Ident, proc_macro2::Literal)> = Vec::new();
    let mut stream = group.stream().into_iter().peekable();
    while let Some(tree) = stream.next() {
        let TokenTree::Ident(variant) = tree else {
            return None;
        };
        match (stream.next()?, stream.next()?) {
            (TokenTree::Punct(eq), TokenTree::Punct(gt))
                if eq.as_char() == '=' && gt.as_char() == '>' => {}
            _ => return None,
        }
        let TokenTree::Literal(raw) = stream.next()? else {
            return None;
        };
        pairs.push((variant, raw));
        if let Some(TokenTree::Punct(comma)) = stream.peek()
            && comma.as_char() == ','
        {
            stream.next();
        }
    }

    let mut reflowed = Vec::with_capacity(pairs.len() + 2);
    reflowed.push(format!("{indent}impl_string_enum!({type_ident} {{"));
    for (variant, raw) in &pairs {
        reflowed.push(format!("{indent}    {variant} => {raw},"));
    }
    let mut closer = format!("{indent}}}");
    if let Some(variant) = &open_variant {
        closer.push_str(&format!(" open = {variant}"));
    }
    if let Some(variant) = &default_variant {
        closer.push_str(&format!(" default = {variant}"));
    }
    closer.push_str(");");
    reflowed.push(closer);

    // Fidelity gate: the reflowed text must hold the same tokens.
    let reparsed: syn::ItemMacro = syn::parse_str(&reflowed.join("\n")).ok()?;
    tokens_equal(reparsed.mac.tokens.clone(), item.mac.tokens.clone()).then_some(reflowed)
}

/// Structural token equality, ignoring `proc_macro2::Spacing` (which
/// differs between quote-built and text-lexed streams around tokens
/// like `$`) — the correct notion of "same tokens" for macro fidelity.
pub(crate) fn tokens_equal(a: TokenStream, b: TokenStream) -> bool {
    use proc_macro2::TokenTree;
    let (a, b): (Vec<TokenTree>, Vec<TokenTree>) =
        (a.into_iter().collect(), b.into_iter().collect());
    a.len() == b.len()
        && a.into_iter().zip(b).all(|(left, right)| match (left, right) {
            (TokenTree::Group(l), TokenTree::Group(r)) => {
                l.delimiter() == r.delimiter() && tokens_equal(l.stream(), r.stream())
            }
            (TokenTree::Ident(l), TokenTree::Ident(r)) => l == r,
            (TokenTree::Punct(l), TokenTree::Punct(r)) => l.as_char() == r.as_char(),
            (TokenTree::Literal(l), TokenTree::Literal(r)) => l.to_string() == r.to_string(),
            _ => false,
        })
}

/// Join polished lines, preserving the source's trailing newline.
pub(crate) fn finish_lines(lines: Vec<String>, source: &str) -> String {
    let mut result = lines.join("\n");
    if source.ends_with('\n') {
        result.push('\n');
    }
    result
}

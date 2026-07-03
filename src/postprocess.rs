//! AST post-processing of typify's output.
//!
//! The typify fork handles derive/impl conflicts at the source, so the only
//! remaining pass is profile-specific: synthesizing `impl Default` for enums
//! that typify cannot default itself.

use quote::quote;

/// Synthesize a conservative `impl Default` for every enum (in every
/// module) that doesn't already have one.
///
/// typify's `with_enum_first_variant_default` covers enums with at least
/// one unit variant, but untagged `oneOf` enums are all tuple/struct
/// variants and get skipped. When such an enum sits in a required field of
/// a struct that derives `Default`, the derive's `Default` bound fails. We
/// default those enums to their first variant with `Default::default()`
/// payloads — the payload types are generated structs which do implement
/// `Default` under the ApiClient profile.
pub fn synthesize_enum_defaults(file: &mut syn::File) {
    for item in &mut file.items {
        if let syn::Item::Mod(module) = item {
            synthesize_in_module(module);
        }
    }
    synthesize_in_items(&mut file.items);
}

fn synthesize_in_module(module: &mut syn::ItemMod) {
    let Some((_, items)) = &mut module.content else {
        return;
    };
    for item in items.iter_mut() {
        if let syn::Item::Mod(nested) = item {
            synthesize_in_module(nested);
        }
    }
    synthesize_in_items(items);
}

fn synthesize_in_items(items: &mut Vec<syn::Item>) {
    // Pass 1: which types already have a `Default` impl?
    let mut has_default_impl: Vec<String> = Vec::new();
    for item in items.iter() {
        if let syn::Item::Impl(impl_block) = item
            && let Some((_, trait_path, _)) = &impl_block.trait_
            && trait_path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "Default")
            && let Some(target) = impl_target_name(impl_block)
        {
            has_default_impl.push(target);
        }
    }

    // Pass 2: synthesize for enums lacking one.
    let mut synthesized = Vec::new();
    for item in items.iter() {
        if let syn::Item::Enum(item_enum) = item
            && !has_default_impl.contains(&item_enum.ident.to_string())
            && let Some(default_impl) = default_impl_for_enum(item_enum)
        {
            synthesized.push(default_impl);
        }
    }
    items.extend(synthesized);
}

/// Extract the simple ident name (e.g. `"Date"`) of an `impl ... for X`
/// block. Returns `None` if the target type isn't a plain path.
fn impl_target_name(impl_block: &syn::ItemImpl) -> Option<String> {
    let syn::Type::Path(type_path) = impl_block.self_ty.as_ref() else {
        return None;
    };
    type_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

/// Build `impl Default` selecting the enum's first variant, recursively
/// defaulting any payload fields.
fn default_impl_for_enum(item_enum: &syn::ItemEnum) -> Option<syn::Item> {
    let enum_ident = &item_enum.ident;
    let (impl_generics, ty_generics, where_clause) = item_enum.generics.split_for_impl();
    let first = item_enum.variants.first()?;
    let variant_ident = &first.ident;
    let default_value = match &first.fields {
        syn::Fields::Unit => quote!(Self::#variant_ident),
        syn::Fields::Unnamed(fields) => {
            let defaults = fields
                .unnamed
                .iter()
                .map(|_| quote!(::std::default::Default::default()));
            quote!(Self::#variant_ident(#(#defaults),*))
        }
        syn::Fields::Named(fields) => {
            let defaults = fields.named.iter().filter_map(|field| {
                field
                    .ident
                    .as_ref()
                    .map(|ident| quote!(#ident: ::std::default::Default::default()))
            });
            quote!(Self::#variant_ident { #(#defaults),* })
        }
    };

    syn::parse2(quote! {
        impl #impl_generics ::std::default::Default for #enum_ident #ty_generics #where_clause {
            fn default() -> Self {
                #default_value
            }
        }
    })
    .ok()
}

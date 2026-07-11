//! Partitioned emission: assemble typify's types into the nested module
//! tree the partition prescribes.
//!
//! The old typify fork owned this step (`to_stream_partitioned`); the
//! current fork instead exposes the composable pieces —
//! [`typify::TypeSpace::to_stream_for`] generates a self-contained stream
//! for a subset of types (each subset carrying its own `error` module and
//! exactly the `defaults` functions its types need) and `Type::id()` /
//! `Type::name()` identify the members — and consumers assemble the
//! module tree themselves. This module is that assembly, mirroring the
//! old fork's semantics: every named `Struct`/`Enum`/`Newtype` type is
//! bucketed by the Rust-name-keyed partition map (misses land in the
//! default module), slash-separated paths nest, every module referenced
//! by an import preamble is materialized even when empty (leaves keep
//! their `error` module so glob imports and `self::error::…` paths keep
//! resolving), and each module's body renders as preamble → child
//! modules (name order) → own items.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Context;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use typify::{TypeDetails, TypeId, TypeSpace};

use crate::Result;

/// Generate the partitioned module tree: the counterpart of the old
/// fork's `TypeSpace::to_stream_partitioned`, built on `to_stream_for`.
pub(crate) fn partitioned_stream(
    type_space: &TypeSpace,
    partition: &HashMap<String, String>,
    default_module: &str,
    imports_per_module: &HashMap<String, TokenStream>,
) -> Result<TokenStream> {
    // Bucket each emitted type's id into a per-module-path list. Only
    // named `Struct`/`Enum`/`Newtype` entries produce top-level items;
    // everything else is a structural helper (`Option`, `Vec`, …) that
    // `to_stream_for` ignores anyway.
    let mut per_module: BTreeMap<String, Vec<TypeId>> = BTreeMap::new();
    let mut has_types: HashSet<String> = HashSet::new();

    // Materialize the default module and every module a preamble
    // references — another module may glob-import it (e.g.
    // `use super::foo::*;`) even when no generated type landed there.
    per_module.entry(default_module.to_string()).or_default();
    for name in imports_per_module.keys() {
        per_module.entry(name.clone()).or_default();
    }

    for ty in type_space.iter_types() {
        match ty.details() {
            TypeDetails::Struct(_) | TypeDetails::Enum(_) | TypeDetails::Newtype(_) => {}
            _ => continue,
        }
        let module = partition
            .get(&ty.name())
            .map(String::as_str)
            .unwrap_or(default_module)
            .to_string();
        has_types.insert(module.clone());
        per_module.entry(module).or_default().push(ty.id());
    }

    // Arrange the flat path-keyed buckets into a module tree so that
    // slash-separated paths nest and siblings merge under a common
    // parent. Each leaf — and each parent that directly holds types —
    // renders its bucket via `to_stream_for`; pure container parents
    // hold nothing but their children.
    let mut root = ModuleNode::default();
    for (path, ids) in per_module {
        root.insert(&path, ids);
    }
    root.into_stream(type_space, String::new(), &has_types, imports_per_module)
}

/// One node of the module tree: an optional bucket of type ids (present
/// exactly when the node's path was a partition/import key) plus nested
/// children keyed by module name.
#[derive(Default)]
struct ModuleNode {
    ids: Option<Vec<TypeId>>,
    children: BTreeMap<String, ModuleNode>,
}

impl ModuleNode {
    fn insert(&mut self, path: &str, ids: Vec<TypeId>) {
        match path.split_once('/') {
            Some((head, rest)) => {
                self.children
                    .entry(head.to_string())
                    .or_default()
                    .insert(rest, ids);
            }
            None => {
                self.children
                    .entry(path.to_string())
                    .or_default()
                    .ids
                    .get_or_insert_with(Vec::new)
                    .extend(ids);
            }
        }
    }

    /// Render this node's body: the caller-supplied preamble for this
    /// exact path, nested `pub mod` blocks for each child (in name
    /// order), then the node's own generated items. The root node (empty
    /// path) renders as the bare sequence of top-level modules. Leaves
    /// always render their bucket (`to_stream_for` emits the `error`
    /// module even for an empty subset, matching the old fork); parents
    /// render theirs only when types were explicitly partitioned into
    /// them.
    fn into_stream(
        self,
        type_space: &TypeSpace,
        path: String,
        has_types: &HashSet<String>,
        imports_per_module: &HashMap<String, TokenStream>,
    ) -> Result<TokenStream> {
        let preamble = imports_per_module.get(&path).cloned().unwrap_or_default();
        let is_leaf = self.children.is_empty();

        let children = self
            .children
            .into_iter()
            .map(|(name, child)| {
                let child_path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                };
                let mod_ident = format_ident!("{}", name);
                let mod_body =
                    child.into_stream(type_space, child_path, has_types, imports_per_module)?;
                Ok(quote! {
                    pub mod #mod_ident {
                        #mod_body
                    }
                })
            })
            .collect::<Result<Vec<TokenStream>>>()?;

        let body = match &self.ids {
            Some(ids) if is_leaf || has_types.contains(&path) => type_space
                .to_stream_for(ids)
                .context("typify failed to render a partition module")?,
            _ => TokenStream::new(),
        };

        Ok(quote! {
            #preamble
            #(#children)*
            #body
        })
    }
}

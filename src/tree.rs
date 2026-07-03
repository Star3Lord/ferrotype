//! Directory-tree output: write a generated module tree as real files.
//!
//! [`write_file_tree`] takes the same post-processed [`syn::File`] that
//! [`render_file`](crate::render_file) would print as one document and
//! instead splits it into a directory mirroring the module structure —
//! the layout of a hand-maintained types crate:
//!
//! ```text
//! <dir>/
//!   mod.rs                  ← declares every top-level module
//!   cancel_booking/
//!     mod.rs                ← `pub mod request; pub mod response;`
//!     request.rs
//!     response.rs
//!   shared/
//!     mod.rs
//!     common.rs
//!     enums.rs
//!     request.rs
//!     response.rs
//! ```
//!
//! The splitting rule: a `pub mod x { ... }` becomes a directory
//! (`x/mod.rs` plus one entry per nested partition module) when it
//! contains nested partition modules, and a plain file (`x.rs`)
//! otherwise. The small helper modules typify inlines into every
//! partition (`error`, `defaults`, and `builder`) are not partition
//! modules: they stay inline within whichever file their parent module
//! landed in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::Result;
use crate::pipeline::{GENERATED_MARKER, generated_header};

/// Helper submodules that typify duplicates into partition modules;
/// these always stay inline in their parent's file rather than becoming
/// files of their own.
const INLINE_MODS: &[&str] = &["error", "defaults", "builder"];

/// Write `file` as a directory tree rooted at `dir`.
///
/// Every top-level `pub mod` becomes `x/mod.rs` (+ one file per nested
/// partition module, recursively) or `x.rs`; a root `mod.rs` declares
/// the top-level modules and carries any non-module items. Every written
/// file is prettyplease-formatted, prefixed with the `// @generated`
/// header naming `spec_path`, and written idempotently (identical
/// content leaves bytes and mtime untouched).
///
/// Stale-file cleanup: after writing, any `.rs` file under `dir` that
/// was not part of this run's output **and** whose first line starts
/// with the `// @generated` marker is deleted (an operation removed from
/// the spec shouldn't leave its module behind), and directories left
/// empty are removed. Files without the marker — user-owned code — are
/// never touched.
pub fn write_file_tree(
    file: &syn::File,
    spec_path: impl AsRef<Path>,
    dir: impl AsRef<Path>,
) -> Result<()> {
    let dir = dir.as_ref();
    let header = generated_header(spec_path);

    // Plan the file set: relative path → items. The root mod.rs keeps
    // every item in its original position, with split-off modules
    // replaced by `pub mod <name>;` declarations.
    let mut files: BTreeMap<PathBuf, Vec<syn::Item>> = BTreeMap::new();
    let mut root_items: Vec<syn::Item> = Vec::new();
    for item in &file.items {
        match item {
            syn::Item::Mod(module) if module.content.is_some() => {
                root_items.push(plan_module(module, Path::new(""), &mut files));
            }
            other => root_items.push(other.clone()),
        }
    }
    files.insert(PathBuf::from("mod.rs"), root_items);

    let mut written: BTreeSet<PathBuf> = BTreeSet::new();
    for (rel_path, items) in files {
        let file = syn::File {
            shebang: None,
            attrs: Vec::new(),
            items,
        };
        let contents = format!("{header}{}", prettyplease::unparse(&file));
        write_if_changed(&dir.join(&rel_path), &contents)?;
        written.insert(rel_path);
    }

    remove_stale_generated(dir, Path::new(""), &written)
}

/// Plan the file(s) for `module` under `parent` (a `dir`-relative
/// directory), and return the `pub mod <name>;` declaration that takes
/// its place in the parent file.
fn plan_module(
    module: &syn::ItemMod,
    parent: &Path,
    files: &mut BTreeMap<PathBuf, Vec<syn::Item>>,
) -> syn::Item {
    let name = module.ident.to_string();
    let (_, items) = module
        .content
        .as_ref()
        .expect("plan_module callers check for inline content");

    if items.iter().any(is_partition_mod) {
        // Directory: mod.rs holds the non-split items (import preamble,
        // inline helper mods, …) plus declarations of the split-off
        // children, each in the position the inline module occupied.
        let mod_dir = parent.join(&name);
        let mut mod_items = Vec::new();
        for item in items {
            match item {
                syn::Item::Mod(child) if is_partition_mod_item(child) => {
                    mod_items.push(plan_module(child, &mod_dir, files));
                }
                other => mod_items.push(other.clone()),
            }
        }
        files.insert(mod_dir.join("mod.rs"), mod_items);
    } else {
        // Leaf: one file holding the module's entire body.
        files.insert(parent.join(format!("{name}.rs")), items.clone());
    }

    syn::Item::Mod(syn::ItemMod {
        attrs: module.attrs.clone(),
        vis: module.vis.clone(),
        unsafety: module.unsafety,
        mod_token: module.mod_token,
        ident: module.ident.clone(),
        content: None,
        semi: Some(Default::default()),
    })
}

/// Whether `item` is a nested partition module — an inline module that
/// should be split into its own file (as opposed to the `error` /
/// `defaults` / `builder` helpers, which stay inline).
fn is_partition_mod(item: &syn::Item) -> bool {
    matches!(item, syn::Item::Mod(module) if is_partition_mod_item(module))
}

fn is_partition_mod_item(module: &syn::ItemMod) -> bool {
    module.content.is_some() && !INLINE_MODS.contains(&module.ident.to_string().as_str())
}

/// Write `contents` to `path`, creating parent directories as needed.
/// Idempotent: when the file already holds identical content its bytes
/// and mtime are left untouched, so downstream builds don't churn.
pub(crate) fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == contents
    {
        return Ok(());
    }
    std::fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

/// Delete previously generated files that this run no longer produces:
/// every `.rs` file under `root` that is not in `written` and whose
/// first line starts with [`GENERATED_MARKER`]. Directories left empty
/// are removed. Files without the marker are never deleted.
fn remove_stale_generated(root: &Path, rel: &Path, written: &BTreeSet<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(root.join(rel)) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        let child_rel = rel.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", entry.path().display()))?;
        if file_type.is_dir() {
            remove_stale_generated(root, &child_rel, written)?;
            // Only succeeds if the directory is now empty; anything else
            // (user files remain) is left alone.
            let _ = std::fs::remove_dir(root.join(&child_rel));
        } else if file_type.is_file()
            && child_rel.extension().is_some_and(|ext| ext == "rs")
            && !written.contains(&child_rel)
            && is_generated_file(&entry.path())
        {
            std::fs::remove_file(entry.path())
                .with_context(|| format!("failed to remove stale {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Whether the file's first line starts with the `// @generated` marker.
fn is_generated_file(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    contents
        .lines()
        .next()
        .is_some_and(|line| line.starts_with(GENERATED_MARKER))
}

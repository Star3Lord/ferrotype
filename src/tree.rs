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
/// Ejection: an existing file at a generated path whose first line
/// lacks the `// @generated` marker is user-owned (ejected by hand or
/// via the `eject` subcommand) — it is **skipped on write** with a
/// note on stderr, never overwritten. Delete the file and regenerate
/// to restore the generated version.
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
    let files = plan_file_tree(file, spec_path);

    let mut written: BTreeSet<PathBuf> = BTreeSet::new();
    for (rel_path, contents) in files {
        let path = dir.join(&rel_path);
        if path.exists() && !is_generated_file(&path) {
            eprintln!(
                "openapi-codegen: skipping {} — existing file has no `{GENERATED_MARKER}` \
                 marker (ejected/user-owned); delete it and regenerate to restore",
                path.display(),
            );
        } else {
            write_if_changed(&path, &contents)?;
        }
        written.insert(rel_path);
    }

    remove_stale_generated(dir, Path::new(""), &written)
}

/// The `ext/` files the verify gate mounts beside the generated tree,
/// as `dir`-relative path → contents pairs for its scratch crate.
///
/// Generated code may reference helpers in the user-owned `ext/`
/// (config field-attrs and type mappings naming `...::ext::<name>::...`
/// paths), so the gate compiles against what the user actually wrote —
/// but only the modules it references: `ext/<name>.rs` and
/// `ext/<name>/**` for each `name` in `referenced`, under a synthesized
/// `ext/mod.rs` (the pristine `scaffold` plus one `pub mod <name>;`
/// per module). Everything else under `ext/` is user code that may
/// lean on the enclosing crate — its dependencies, items at its root —
/// and is that crate's build to check, not the gate's; a mounted
/// module, by contrast, must stand on the generated tree and the
/// declared `[verify] dependencies` alone.
///
/// Nothing referenced yields the scaffold alone. A referenced module
/// with no file or directory of its own (declared inline in the user's
/// `ext/mod.rs`, or a typo in the config) falls back to mounting the
/// user's whole `ext/` so the compiler can report on it; a tree without
/// `ext/mod.rs` yet falls back to the scaffold.
pub(crate) fn plan_ext_dir(
    dir: &Path,
    referenced: &BTreeSet<String>,
    scaffold: &str,
) -> BTreeMap<PathBuf, String> {
    let ext_dir = dir.join("ext");
    let scaffold_only = || BTreeMap::from([(PathBuf::from("ext/mod.rs"), scaffold.to_string())]);
    if referenced.is_empty() {
        return scaffold_only();
    }

    let mut files = BTreeMap::new();
    for name in referenced {
        let before = files.len();
        collect_ext_file(&ext_dir, &ext_dir.join(format!("{name}.rs")), &mut files);
        collect_ext_tree(&ext_dir, &ext_dir.join(name), &mut files);
        if files.len() == before {
            let mut whole = BTreeMap::new();
            collect_ext_tree(&ext_dir, &ext_dir, &mut whole);
            // A directory without a mod.rs cannot satisfy `pub mod ext;`.
            return if whole.contains_key(Path::new("ext/mod.rs")) {
                whole
            } else {
                scaffold_only()
            };
        }
    }

    let declarations: String = referenced
        .iter()
        .map(|name| format!("pub mod {name};\n"))
        .collect();
    files.insert(
        PathBuf::from("ext/mod.rs"),
        format!("{scaffold}\n{declarations}"),
    );
    files
}

/// Record `path` (an `.rs` file under the user's `ext/` at `root`)
/// into `files` under its `ext/`-prefixed relative path.
fn collect_ext_file(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, String>) {
    if path.extension().is_some_and(|ext| ext == "rs")
        && let Ok(contents) = std::fs::read_to_string(path)
        && let Ok(rel) = path.strip_prefix(root)
    {
        files.insert(PathBuf::from("ext").join(rel), contents);
    }
}

/// Record every `.rs` file under `dir` (recursively) into `files`; a
/// missing or unreadable directory records nothing.
fn collect_ext_tree(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_ext_tree(root, &path, files);
        } else {
            collect_ext_file(root, &path, files);
        }
    }
}

/// Write the user-owned `ext/mod.rs` scaffold, once: if the file
/// already exists — scaffolded earlier, possibly edited since — it is
/// left byte-untouched. The scaffold carries no `// @generated` marker,
/// so [`write_file_tree`]'s overwrite protection and stale cleanup
/// never touch it either.
pub(crate) fn write_ext_scaffold(dir: &Path, contents: &str) -> Result<()> {
    let path = dir.join("ext").join("mod.rs");
    if path.exists() {
        return Ok(());
    }
    write_if_changed(&path, contents)
}

/// Plan the directory-tree output of [`write_file_tree`] without touching
/// the filesystem: the map of `dir`-relative paths to the exact file
/// contents (header + formatted source) that would be written.
pub fn plan_file_tree(
    file: &syn::File,
    spec_path: impl AsRef<Path>,
) -> BTreeMap<PathBuf, String> {
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

    files
        .into_iter()
        .map(|(rel_path, items)| {
            let file = syn::File {
                shebang: None,
                attrs: Vec::new(),
                items,
            };
            let body = crate::render::render_body(&file);
            let contents = format!("{header}{body}");
            (rel_path, contents)
        })
        .collect()
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

/// The marker opening an ejected file's first line.
pub(crate) const EJECTED_MARKER: &str = "// @ejected";

/// Eject a generated file: verify the `// @generated` marker and
/// rewrite the header so the file becomes user-owned.
///
/// The two-line generated header (`// @generated by openapi-codegen
/// from <spec>` + `// Do not edit by hand.`) is replaced with
/// `// @ejected — was generated from <spec>; delete this file and
/// regenerate to restore.` From then on [`write_file_tree`] skips the
/// file on write and stale cleanup never deletes it; deleting it and
/// regenerating restores the generated version (un-eject).
///
/// Ejecting a file that is missing, already ejected, or was never
/// generated (no marker) is an error.
pub fn eject_file(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {} — does it exist?", path.display()))?;

    let (first_line, rest) = contents.split_once('\n').unwrap_or((contents.as_str(), ""));
    if first_line.starts_with(EJECTED_MARKER) {
        anyhow::bail!("{} is already ejected", path.display());
    }
    if !first_line.starts_with(GENERATED_MARKER) {
        anyhow::bail!(
            "{} has no `{GENERATED_MARKER}` marker on its first line; only \
             generated files can be ejected",
            path.display(),
        );
    }

    // `// @generated by openapi-codegen from <spec>` → the spec path.
    let spec = first_line
        .split_once(" from ")
        .map(|(_, spec)| spec.trim())
        .unwrap_or("<unknown spec>");
    let header = format!(
        "{EJECTED_MARKER} — was generated from {spec}; delete this file and \
         regenerate to restore.\n",
    );

    // Drop the second header line too, when it is the standard one.
    let body = match rest.split_once('\n') {
        Some(("// Do not edit by hand.", tail)) => tail,
        _ => rest,
    };

    std::fs::write(path, format!("{header}{body}"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    eprintln!(
        "openapi-codegen: ejected {} — now user-owned; regeneration will skip it",
        path.display(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCAFFOLD: &str = "//! User-owned extensions.\n";

    /// A fresh tree root holding the given `ext/`-relative files.
    fn tree_with_ext(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "openapi-codegen-tree-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, contents) in files {
            write_if_changed(&dir.join("ext").join(rel), contents).unwrap();
        }
        dir
    }

    fn referenced(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// Only the referenced modules are mounted — a file module with its
    /// submodule directory, a directory-only module — under a
    /// synthesized mod.rs; the user's mod.rs and unreferenced siblings
    /// stay out.
    #[test]
    fn mounts_referenced_ext_modules_only() {
        let dir = tree_with_ext(
            "referenced",
            &[
                ("mod.rs", "pub mod datetime;\npub mod stamp;\npub mod origins;\n"),
                ("datetime.rs", "pub mod option {}\n#[cfg(test)]\nmod tests;\n"),
                ("datetime/tests/mod.rs", "#[test]\nfn t() {}\n"),
                ("stamp/mod.rs", "pub type Stamp = String;\n"),
                ("origins.rs", "use lib_errors::sites;\n"),
            ],
        );

        let planned = plan_ext_dir(&dir, &referenced(&["datetime", "stamp"]), SCAFFOLD);

        let paths: Vec<_> = planned.keys().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(
            paths,
            [
                "ext/datetime/tests/mod.rs",
                "ext/datetime.rs",
                "ext/mod.rs",
                "ext/stamp/mod.rs",
            ],
        );
        assert_eq!(
            planned[Path::new("ext/mod.rs")],
            format!("{SCAFFOLD}\npub mod datetime;\npub mod stamp;\n"),
        );
    }

    /// Nothing referenced: the pristine scaffold alone, whatever the
    /// user's ext/ holds.
    #[test]
    fn nothing_referenced_mounts_scaffold_only() {
        let dir = tree_with_ext(
            "unreferenced",
            &[("mod.rs", "pub mod origins;\n"), ("origins.rs", "broken")],
        );

        let planned = plan_ext_dir(&dir, &BTreeSet::new(), SCAFFOLD);

        assert_eq!(
            planned,
            BTreeMap::from([(PathBuf::from("ext/mod.rs"), SCAFFOLD.to_string())]),
        );
    }

    /// A referenced module without a file of its own falls back to the
    /// user's whole ext/ (it may be declared inline in mod.rs); without
    /// any mod.rs there is nothing to mount but the scaffold.
    #[test]
    fn missing_referenced_module_falls_back() {
        let dir = tree_with_ext(
            "inline",
            &[
                ("mod.rs", "pub mod inline { pub type T = u8; }\npub mod other;\n"),
                ("other.rs", "pub fn f() {}\n"),
            ],
        );
        let planned = plan_ext_dir(&dir, &referenced(&["inline"]), SCAFFOLD);
        let paths: Vec<_> = planned.keys().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(paths, ["ext/mod.rs", "ext/other.rs"]);
        assert!(planned[Path::new("ext/mod.rs")].contains("pub mod inline"));

        let fresh = tree_with_ext("fresh", &[]);
        let planned = plan_ext_dir(&fresh, &referenced(&["datetime"]), SCAFFOLD);
        assert_eq!(
            planned,
            BTreeMap::from([(PathBuf::from("ext/mod.rs"), SCAFFOLD.to_string())]),
        );
    }
}

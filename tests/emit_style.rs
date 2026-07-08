//! The condensed emit style (docs/MIGRATION.md D14):
//! `[style] emit-style = "condensed"` trades the per-enum conversion
//! ladder and per-module `error` mods for one `support` module and one
//! `impl_string_enum!` invocation per enum.
//!
//! Three families of assertions:
//!
//! - **emission shape** — the invocation and the single support module
//!   are present, the raw impl blocks and duplicated error mods are
//!   not, and this holds across output modes (flat, partitioned, split,
//!   tree);
//! - **capability equivalence** — the type items themselves (structs
//!   and enums, attributes included) are token-identical between the
//!   two styles, so only boilerplate moved (behavioral equivalence of
//!   the macro expansion is pinned by `examples/petstore_tree_condensed`
//!   and the consumer workspace's round-trip suites, plus the
//!   macro-text pin inside `src/ir/emit.rs`);
//! - **golden fence** — the checked-in
//!   `examples/generated_tree/petstore_condensed/` tree is exactly what
//!   the generator produces today.
//!
//! The parity gate (`tests/parity.rs`) is deliberately untouched:
//! `emit-style` defaults to `expanded` in every preset, so the
//! IR-vs-fork byte-identity oracle keeps its meaning and condensed is a
//! consumer opt-in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use openapi_codegen::config::EmitStyle;
use openapi_codegen::{Generator, StyleConfig, StyleProfile, plan_file_tree};

const PETSTORE_SPEC: &str = "specs/petstore.yaml";

fn petstore(condensed: bool) -> Generator {
    let generator = Generator::new(PETSTORE_SPEC)
        .profile(StyleProfile::ApiClient)
        ;
    if condensed {
        generator.style(|style| style.emit_style = EmitStyle::Condensed)
    } else {
        generator
    }
}

// ─── Emission shape ─────────────────────────────────────────────────────────

#[test]
fn condensed_split_replaces_ladders_and_error_mods() {
    let out = petstore(true)
        .split_request_response(true)
        .generate_to_string()
        .unwrap();

    // One support module, one macro definition, one error mod (inside
    // support) — and one invocation for the one string enum.
    assert_eq!(out.matches("pub mod support").count(), 1, "{out}");
    assert_eq!(out.matches("macro_rules! impl_string_enum").count(), 1, "{out}");
    assert_eq!(out.matches("pub mod error").count(), 1, "{out}");
    assert!(out.contains("impl_string_enum!(PetStatus {"), "{out}");
    assert!(out.contains("} default = Available);"), "{out}");

    // The raw ladder is gone from the module body...
    for gone in [
        "impl ::std::fmt::Display for PetStatus",
        "impl ::std::str::FromStr for PetStatus",
        "impl ::std::convert::TryFrom<&str> for PetStatus",
        "impl ::std::default::Default for PetStatus",
    ] {
        assert!(!out.contains(gone), "{gone} should be condensed:\n{out}");
    }

    // ...while every module that used to carry an error mod re-exports
    // the shared one, and the enum module imports the macro.
    assert_eq!(out.matches("pub use super::super::support::error;").count(), 8, "{out}");
    assert_eq!(
        out.matches("use super::super::support::impl_string_enum;").count(),
        1,
        "{out}",
    );
}

#[test]
fn condensed_flat_and_partitioned_modes_anchor_support_paths() {
    let flat = petstore(true).generate_to_string().unwrap();
    assert!(flat.contains("use self::support::impl_string_enum;"), "{flat}");
    assert!(flat.contains("pub use self::support::error;"), "{flat}");
    assert!(flat.contains("impl_string_enum!(PetStatus {"), "{flat}");

    let partitioned = petstore(true)
        .partition_by_operation(true)
        .generate_to_string()
        .unwrap();
    assert!(
        partitioned.contains("use super::support::impl_string_enum;"),
        "{partitioned}",
    );
    assert!(partitioned.contains("pub use super::support::error;"), "{partitioned}");
}

#[test]
fn condensed_tree_gets_a_support_file() {
    let file = petstore(true)
        .split_request_response(true)
        .generate_to_syn_file()
        .unwrap();
    let tree = plan_file_tree(&file, PETSTORE_SPEC);

    let support = &tree[Path::new("support.rs")];
    assert!(support.contains("macro_rules! impl_string_enum"), "{support}");
    assert!(support.contains("pub mod error"), "{support}");
    assert!(support.contains("pub(crate) use impl_string_enum;"), "{support}");

    let root = &tree[Path::new("mod.rs")];
    assert!(root.contains("pub mod support;"), "{root}");

    let enums = &tree[Path::new("shared/enums.rs")];
    assert!(enums.contains("impl_string_enum!(PetStatus {"), "{enums}");
    assert!(!enums.contains("macro_rules!"), "{enums}");

    // A module without enums keeps only the error re-export.
    let response = &tree[Path::new("create_pet/response.rs")];
    assert!(response.contains("pub use super::super::support::error;"), "{response}");
    assert!(!response.contains("impl_string_enum"), "{response}");
}

#[test]
fn schema_default_and_no_default_render_the_right_clause() {
    let spec = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "WithDefault": { "type": "string", "enum": ["a", "b"], "default": "b" },
            "Holder": {
                "type": "object",
                "properties": {
                    "with": { "$ref": "#/components/schemas/WithDefault" },
                    "plain": { "type": "string", "enum": ["x", "y"] }
                }
            }
        } }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("emit_style");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("defaults.json");
    std::fs::write(&path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    // Schema-level default → its variant; no schema default under
    // `enum-default = "schema-only"` → no default clause at all.
    let out = Generator::new(&path)
        .profile(StyleProfile::ApiClient)
        
        .style(|style| {
            style.emit_style = EmitStyle::Condensed;
            style.enum_default = openapi_codegen::config::EnumDefaultMode::SchemaOnly;
        })
        .generate_to_string()
        .unwrap();
    assert!(out.contains("} default = B);"), "{out}");
    assert!(
        out.contains("impl_string_enum!(HolderPlain {"),
        "inline enum condensed too:\n{out}",
    );
    let invocation_start = out.find("impl_string_enum!(HolderPlain {").unwrap();
    let invocation_end = invocation_start + out[invocation_start..].find(");").unwrap();
    let invocation = &out[invocation_start..invocation_end];
    assert!(
        !invocation.contains("default"),
        "no default clause without a schema default:\n{invocation}",
    );
}

#[test]
fn open_enums_key_produces_catch_all_and_condenses() {
    let spec = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "Kind": { "type": "string", "enum": ["yes", "no"], "default": "yes" }
        } }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("emit_style");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("open_enums.json");
    std::fs::write(&path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    // Expanded: the catch-all variant and the open ladder shapes.
    let expanded = Generator::new(&path)
        .profile(StyleProfile::ApiClient)
        .style(|style| style.open_enums = Some("Other".to_string()))
        .generate_to_string()
        .unwrap();
    assert!(expanded.contains("Other(::std::string::String)"), "{expanded}");
    assert!(expanded.contains("#[serde(untagged)]"), "{expanded}");
    assert!(
        expanded.contains("Ok(Self::Other(value.to_string()))"),
        "irrefutable FromStr: {expanded}",
    );

    // Condensed: the same enum collapses to one invocation carrying the
    // `open` clause (and the schema default).
    let condensed = Generator::new(&path)
        .profile(StyleProfile::ApiClient)
        .style(|style| {
            style.open_enums = Some("Other".to_string());
            style.emit_style = EmitStyle::Condensed;
        })
        .generate_to_string()
        .unwrap();
    assert!(
        condensed.contains("} open = Other default = Yes);"),
        "{condensed}",
    );
    assert!(
        !condensed.contains("impl ::std::fmt::Display for Kind"),
        "open ladder must condense: {condensed}",
    );

    // Config plumbing: the kebab-case key round-trips.
    let config = StyleConfig::from_toml_str(
        "profile = \"api-client\"\n[style]\nopen-enums = \"Other\"\n",
        StyleConfig::api_client(),
    )
    .unwrap();
    assert_eq!(config.open_enums.as_deref(), Some("Other"));
}

#[test]
fn condensed_reserves_the_support_module_name() {
    let spec = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": { "/s": { "get": {
            "operationId": "support",
            "responses": { "200": { "content": { "application/json": {
                "schema": { "$ref": "#/components/schemas/Thing" }
            } } } }
        } } },
        "components": { "schemas": {
            "Thing": { "type": "object", "properties": { "x": { "type": "string" } } }
        } }
    });
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("emit_style");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("support_op.json");
    std::fs::write(&path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    let error = format!(
        "{:#}",
        Generator::new(&path)
            .profile(StyleProfile::ApiClient)
            
            .style(|style| style.emit_style = EmitStyle::Condensed)
            .partition_by_operation(true)
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(error.contains("support"), "{error}");
}

// ─── Capability equivalence: the types themselves are untouched ─────────────

/// Collect every schema-derived struct/enum item with its module path,
/// rendered as a normalized token string. The boilerplate homes —
/// `error` mods (expanded style) and the `support` mod (condensed
/// style) — are excluded: relocating their contents is exactly the
/// styles' delta.
fn type_items(source: &str) -> BTreeMap<(String, String), String> {
    use quote::ToTokens as _;

    fn walk(
        items: &[syn::Item],
        path: String,
        acc: &mut BTreeMap<(String, String), String>,
    ) {
        for item in items {
            match item {
                syn::Item::Mod(module)
                    if module.ident != "support" && module.ident != "error" =>
                {
                    if let Some((_, nested)) = &module.content {
                        let nested_path = if path.is_empty() {
                            module.ident.to_string()
                        } else {
                            format!("{path}::{}", module.ident)
                        };
                        walk(nested, nested_path, acc);
                    }
                }
                syn::Item::Struct(def) => {
                    acc.insert(
                        (path.clone(), def.ident.to_string()),
                        item.to_token_stream().to_string(),
                    );
                }
                syn::Item::Enum(def) => {
                    acc.insert(
                        (path.clone(), def.ident.to_string()),
                        item.to_token_stream().to_string(),
                    );
                }
                _ => {}
            }
        }
    }

    let file = syn::parse_file(source).expect("generated output parses");
    let mut acc = BTreeMap::new();
    walk(&file.items, String::new(), &mut acc);
    acc
}

#[test]
fn condensed_leaves_every_type_item_token_identical() {
    let expanded = petstore(false)
        .split_request_response(true)
        .generate_to_string()
        .unwrap();
    let condensed = petstore(true)
        .split_request_response(true)
        .generate_to_string()
        .unwrap();

    let expanded_types = type_items(&expanded);
    let condensed_types = type_items(&condensed);
    assert_eq!(
        expanded_types.keys().collect::<Vec<_>>(),
        condensed_types.keys().collect::<Vec<_>>(),
        "the two styles must declare the same types in the same modules",
    );
    for (key, expanded_tokens) in &expanded_types {
        assert_eq!(
            expanded_tokens, &condensed_types[key],
            "type {key:?} changed between emit styles",
        );
    }
}

// ─── Config plumbing ─────────────────────────────────────────────────────────

#[test]
fn emit_style_key_round_trips_through_codegen_toml() {
    let config = StyleConfig::from_toml_str(
        "profile = \"api-client\"\n[style]\nemit-style = \"condensed\"\n",
        StyleConfig::api_client(),
    )
    .unwrap();
    assert_eq!(config.emit_style, EmitStyle::Condensed);

    // Presets default to expanded — the parity strategy (D14).
    assert_eq!(StyleConfig::api_client().emit_style, EmitStyle::Expanded);
    assert_eq!(StyleConfig::default().emit_style, EmitStyle::Expanded);

    let error = StyleConfig::from_toml_str(
        "[style]\nemit-style = \"pretty\"\n",
        StyleConfig::api_client(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("emit-style"), "{error:#}");
}

// ─── Golden fence for the checked-in condensed tree ─────────────────────────

/// Every checked-in file under `dir`, as relative path → contents.
fn read_golden_tree(dir: &str) -> BTreeMap<PathBuf, String> {
    fn walk(root: &Path, dir: &Path, acc: &mut BTreeMap<PathBuf, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, acc);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                acc.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read_to_string(&path).unwrap(),
                );
            }
        }
    }
    let mut acc = BTreeMap::new();
    walk(Path::new(dir), Path::new(dir), &mut acc);
    acc
}

#[test]
fn golden_petstore_condensed_tree() {
    let file = petstore(true)
        .split_request_response(true)
        .generate_to_syn_file()
        .unwrap();
    let planned = plan_file_tree(&file, PETSTORE_SPEC);
    let golden = read_golden_tree("examples/generated_tree/petstore_condensed");

    assert_eq!(
        golden.keys().collect::<Vec<_>>(),
        planned.keys().collect::<Vec<_>>(),
        "file sets differ",
    );
    for (path, golden_contents) in &golden {
        assert_eq!(
            golden_contents, &planned[path],
            "condensed golden diverged: {}",
            path.display(),
        );
    }
}

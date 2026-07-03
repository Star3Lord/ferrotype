//! Differential parity harness (migration step 0+).
//!
//! The checked-in outputs under `examples/generated*/` are **golden**: the
//! `golden_*` tests pin the current (typify-fork) engine to them
//! byte-for-byte, fencing the migration. Engine-vs-engine tests compare the
//! IR engine against the same goldens under the contract documented in
//! `docs/MIGRATION.md` ("Parity contract"): equality of normalized token
//! streams (formatting-insensitive, everything else — names, field order,
//! attributes, impls, module structure — significant), with byte equality
//! reported too since the IR emitter targets it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use openapi_codegen::{Engine, Generator, StyleProfile, plan_file_tree};

const PETSTORE_SPEC: &str = "specs/petstore.yaml";
const SABRE_SPEC: &str = "specs/sabre-booking/spec.openapi.yaml";
const SABRE_PATCHES: &str = "specs/sabre-booking/patches";

/// Output-shape modes of the pipeline, one axis of the parity matrix.
/// `Flat` joins the matrix with the engine-vs-engine tests in step 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum Mode {
    /// No partitioning: one flat stream of items.
    Flat,
    /// One `pub mod <op>` per operation plus `shared`.
    Partitioned,
    /// Request/response split, rendered into one file.
    Split,
}

fn generator(spec: &str, patches: Option<&str>, mode: Mode) -> Generator {
    let mut generator = Generator::new(spec).profile(StyleProfile::ApiClient);
    if let Some(dir) = patches {
        generator = generator.patches_dir(dir);
    }
    match mode {
        Mode::Flat => generator,
        Mode::Partitioned => generator.partition_by_operation(true),
        Mode::Split => generator.split_request_response(true),
    }
}

/// Single-file output of the typify-fork engine.
fn typify_single(spec: &str, patches: Option<&str>, mode: Mode) -> String {
    generator(spec, patches, mode)
        .generate_to_string()
        .unwrap()
}

/// Folder-tree output of the typify-fork engine, planned in memory:
/// relative path → exact file contents.
fn typify_tree(spec: &str, patches: Option<&str>) -> BTreeMap<PathBuf, String> {
    let file = generator(spec, patches, Mode::Split)
        .load()
        .unwrap()
        .lower()
        .unwrap()
        .build_types()
        .unwrap()
        .into_file()
        .unwrap();
    plan_file_tree(&file, spec)
}

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

/// First point of divergence between two strings, rendered with context —
/// assertion messages for multi-thousand-line outputs must stay readable.
fn first_divergence(label: &str, expected: &str, actual: &str) -> String {
    if expected == actual {
        return String::new();
    }
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let first_diff = expected_lines
        .iter()
        .zip(actual_lines.iter())
        .position(|(e, a)| e != a)
        .unwrap_or_else(|| expected_lines.len().min(actual_lines.len()));

    let context = 3usize;
    let start = first_diff.saturating_sub(context);
    let mut message = format!(
        "{label}: outputs diverge at line {} (expected {} lines, actual {})\n",
        first_diff + 1,
        expected_lines.len(),
        actual_lines.len(),
    );
    for index in start..(first_diff + context + 1) {
        let expected_line = expected_lines.get(index).copied();
        let actual_line = actual_lines.get(index).copied();
        if expected_line.is_none() && actual_line.is_none() {
            break;
        }
        let marker = if expected_line != actual_line { ">" } else { " " };
        message.push_str(&format!(
            "{marker} {:>5} expected | {}\n{marker} {:>5} actual   | {}\n",
            index + 1,
            expected_line.unwrap_or("<eof>"),
            index + 1,
            actual_line.unwrap_or("<eof>"),
        ));
    }
    message
}

/// Normalize Rust source to a token-stream string: formatting-insensitive,
/// everything else significant. Panics (with `label`) on unparsable input —
/// generated output failing to parse is itself a failure.
fn normalized_tokens(label: &str, source: &str) -> String {
    // The `// @generated` header is comment trivia and does not survive
    // tokenization anyway; parse the whole thing.
    let file = syn::parse_file(source)
        .unwrap_or_else(|error| panic!("{label}: output does not parse: {error}"));
    use quote::ToTokens as _;
    file.into_token_stream().to_string()
}

/// Assert the parity contract between two single-file outputs: normalized
/// token equality (the contract), reporting byte equality alongside.
fn assert_parity(label: &str, expected: &str, actual: &str) {
    let expected_tokens = normalized_tokens(&format!("{label} (expected)"), expected);
    let actual_tokens = normalized_tokens(&format!("{label} (actual)"), actual);
    if expected_tokens != actual_tokens {
        panic!(
            "{label}: token-stream divergence.\n{}",
            first_divergence(label, expected, actual),
        );
    }
    // Token parity holds; report byte drift loudly but don't fail the
    // contract on it — see docs/MIGRATION.md.
    if expected != actual {
        eprintln!(
            "parity[{label}]: token-identical but not byte-identical\n{}",
            first_divergence(label, expected, actual),
        );
    }
}

/// Assert byte equality (the step-0 golden fence and the step-1 guard).
fn assert_bytes(label: &str, expected: &str, actual: &str) {
    assert!(
        expected == actual,
        "{label}: byte divergence.\n{}",
        first_divergence(label, expected, actual),
    );
}

fn assert_tree_parity(
    label: &str,
    expected: &BTreeMap<PathBuf, String>,
    actual: &BTreeMap<PathBuf, String>,
    contract: fn(&str, &str, &str),
) {
    let expected_paths: Vec<_> = expected.keys().collect();
    let actual_paths: Vec<_> = actual.keys().collect();
    assert!(
        expected_paths == actual_paths,
        "{label}: file sets differ.\n  expected: {expected_paths:?}\n  actual:   {actual_paths:?}",
    );
    for (path, expected_contents) in expected {
        contract(
            &format!("{label} :: {}", path.display()),
            expected_contents,
            &actual[path],
        );
    }
}

// ─── Step 0: the golden fence — current engine ↔ checked-in outputs ────────

#[test]
fn golden_petstore_partitioned() {
    let generated = typify_single(PETSTORE_SPEC, None, Mode::Partitioned);
    let golden = std::fs::read_to_string("examples/generated/petstore.rs").unwrap();
    assert_bytes("petstore partitioned vs golden", &golden, &generated);
}

#[test]
fn golden_sabre_partitioned() {
    let generated = typify_single(SABRE_SPEC, Some(SABRE_PATCHES), Mode::Partitioned);
    let golden = std::fs::read_to_string("examples/generated/sabre_booking.rs").unwrap();
    assert_bytes("sabre partitioned vs golden", &golden, &generated);
}

#[test]
fn golden_petstore_tree() {
    let planned = typify_tree(PETSTORE_SPEC, None);
    let golden = read_golden_tree("examples/generated_tree/petstore");
    assert_tree_parity("petstore tree vs golden", &golden, &planned, assert_bytes);
}

#[test]
fn golden_sabre_tree() {
    let planned = typify_tree(SABRE_SPEC, Some(SABRE_PATCHES));
    let golden = read_golden_tree("examples/generated_tree/sabre_booking");
    assert_tree_parity("sabre tree vs golden", &golden, &planned, assert_bytes);
}

// ─── Step 2: the IR engine vs the frozen fork ───────────────────────────────

/// Single-file output of the IR engine.
fn ir_single(spec: &str, patches: Option<&str>, mode: Mode) -> String {
    generator(spec, patches, mode)
        .engine(Engine::Ir)
        .generate_to_string()
        .unwrap()
}

/// Folder-tree output of the IR engine, planned in memory.
fn ir_tree(spec: &str, patches: Option<&str>) -> BTreeMap<PathBuf, String> {
    let file = generator(spec, patches, Mode::Split)
        .engine(Engine::Ir)
        .generate_to_syn_file()
        .unwrap();
    plan_file_tree(&file, spec)
}

fn engine_parity_single(name: &str, spec: &str, patches: Option<&str>, mode: Mode) {
    let expected = typify_single(spec, patches, mode);
    let actual = ir_single(spec, patches, mode);
    assert_parity(&format!("{name} ({mode:?})"), &expected, &actual);
}

#[test]
fn ir_petstore_flat() {
    engine_parity_single("petstore", PETSTORE_SPEC, None, Mode::Flat);
}

#[test]
fn ir_petstore_partitioned() {
    engine_parity_single("petstore", PETSTORE_SPEC, None, Mode::Partitioned);
}

#[test]
fn ir_petstore_split() {
    engine_parity_single("petstore", PETSTORE_SPEC, None, Mode::Split);
}

#[test]
fn ir_petstore_tree() {
    let expected = typify_tree(PETSTORE_SPEC, None);
    let actual = ir_tree(PETSTORE_SPEC, None);
    assert_tree_parity("petstore tree", &expected, &actual, assert_parity);
}

#[test]
fn ir_sabre_flat() {
    engine_parity_single("sabre", SABRE_SPEC, Some(SABRE_PATCHES), Mode::Flat);
}

#[test]
fn ir_sabre_partitioned() {
    engine_parity_single("sabre", SABRE_SPEC, Some(SABRE_PATCHES), Mode::Partitioned);
}

#[test]
fn ir_sabre_split() {
    engine_parity_single("sabre", SABRE_SPEC, Some(SABRE_PATCHES), Mode::Split);
}

#[test]
fn ir_sabre_tree() {
    let expected = typify_tree(SABRE_SPEC, Some(SABRE_PATCHES));
    let actual = ir_tree(SABRE_SPEC, Some(SABRE_PATCHES));
    assert_tree_parity("sabre tree", &expected, &actual, assert_parity);
}
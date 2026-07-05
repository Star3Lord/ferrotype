//! The golden fence: the checked-in outputs under `examples/generated*/`
//! are pinned byte-for-byte against the pipeline.
//!
//! Any intentional output change must regenerate the goldens explicitly
//! (`cargo run --example …` / the checked-in regen scripts) and re-run
//! the consuming examples — a diff here is either a regression or an
//! undocumented behavior change. This fence started life as the
//! migration parity harness (docs/MIGRATION.md); the engine-vs-engine
//! half retired with the IR engine (decision D15), the fence remains.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use openapi_codegen::{Generator, StyleProfile, plan_file_tree};

const PETSTORE_SPEC: &str = "specs/petstore.yaml";
const SABRE_SPEC: &str = "specs/sabre-booking/spec.openapi.yaml";
const SABRE_PATCHES: &str = "specs/sabre-booking/patches";

/// Output-shape modes of the pipeline.
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

/// Single-file output of the pipeline.
fn generate_single(spec: &str, patches: Option<&str>, mode: Mode) -> String {
    generator(spec, patches, mode)
        .generate_to_string()
        .unwrap()
}

/// Folder-tree output, planned in memory: relative path → exact file
/// contents.
fn generate_tree(spec: &str, patches: Option<&str>) -> BTreeMap<PathBuf, String> {
    let file = generator(spec, patches, Mode::Split)
        .generate_to_syn_file()
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

/// Assert byte equality against a golden.
fn assert_bytes(label: &str, expected: &str, actual: &str) {
    assert!(
        expected == actual,
        "{label}: byte divergence.\n{}",
        first_divergence(label, expected, actual),
    );
}

fn assert_tree_bytes(
    label: &str,
    expected: &BTreeMap<PathBuf, String>,
    actual: &BTreeMap<PathBuf, String>,
) {
    let expected_paths: Vec<_> = expected.keys().collect();
    let actual_paths: Vec<_> = actual.keys().collect();
    assert!(
        expected_paths == actual_paths,
        "{label}: file sets differ.\n  expected: {expected_paths:?}\n  actual:   {actual_paths:?}",
    );
    for (path, expected_contents) in expected {
        assert_bytes(
            &format!("{label} :: {}", path.display()),
            expected_contents,
            &actual[path],
        );
    }
}

#[test]
fn golden_petstore_partitioned() {
    let generated = generate_single(PETSTORE_SPEC, None, Mode::Partitioned);
    let golden = std::fs::read_to_string("examples/generated/petstore.rs").unwrap();
    assert_bytes("petstore partitioned vs golden", &golden, &generated);
}

#[test]
fn golden_sabre_partitioned() {
    let generated = generate_single(SABRE_SPEC, Some(SABRE_PATCHES), Mode::Partitioned);
    let golden = std::fs::read_to_string("examples/generated/sabre_booking.rs").unwrap();
    assert_bytes("sabre partitioned vs golden", &golden, &generated);
}

#[test]
fn golden_petstore_tree() {
    let planned = generate_tree(PETSTORE_SPEC, None);
    let golden = read_golden_tree("examples/generated_tree/petstore");
    assert_tree_bytes("petstore tree vs golden", &golden, &planned);
}

#[test]
fn golden_sabre_tree() {
    let planned = generate_tree(SABRE_SPEC, Some(SABRE_PATCHES));
    let golden = read_golden_tree("examples/generated_tree/sabre_booking");
    assert_tree_bytes("sabre tree vs golden", &golden, &planned);
}

// ─── Client goldens ──────────────────────────────────────────────────────────
//
// The `--client` outputs are pinned as text only: openapi-codegen
// deliberately takes no reqwest/tokio dependencies, so compilation of
// client output is proven by the verify gate and by the
// `via-cli-client` crate in the examples workspace (which also runs
// wiremock round-trips against it).

/// Single-file petstore output with the client enabled (NoAuth: the
/// spec has no securitySchemes). Regenerate with:
///
/// ```text
/// cargo run -- generate --spec specs/petstore.yaml --profile api-client \
///     --partition-by-operation --client --output examples/generated/petstore_client.rs
/// ```
#[test]
fn golden_petstore_client_partitioned() {
    let generated = generator(PETSTORE_SPEC, None, Mode::Partitioned)
        .client(true)
        .generate_to_string()
        .unwrap();
    let golden = std::fs::read_to_string("examples/generated/petstore_client.rs").unwrap();
    assert_bytes("petstore client partitioned vs golden", &golden, &generated);
}

/// Folder-tree sabre output with the client enabled: the split
/// request/response layout plus `client/{mod,auth,support}.rs` (OAuth2
/// client-credentials from the patched securitySchemes) and the
/// user-owned `ext/mod.rs`. Regenerate with:
///
/// ```text
/// cargo run -- generate --spec specs/sabre-booking/spec.openapi.yaml \
///     --patches-dir specs/sabre-booking/patches --profile api-client \
///     --split-request-response --client \
///     --output-dir examples/generated_tree/sabre_booking_client
/// ```
///
/// Generation runs into a fresh temp directory (never the checked-in
/// golden), which also pins the write-once `ext/mod.rs` story: a fresh
/// run scaffolds ext/mod.rs with exactly the checked-in bytes, while
/// regenerating over the golden directory leaves the existing
/// (marker-less, user-owned) file untouched — both paths land on the
/// same content, so the byte comparison holds either way.
#[test]
fn golden_sabre_client_tree() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sabre_client_golden");
    let _ = std::fs::remove_dir_all(&dir);
    generator(SABRE_SPEC, Some(SABRE_PATCHES), Mode::Split)
        .client(true)
        .generate_to_dir(&dir)
        .unwrap();

    let generated = read_golden_tree(dir.to_str().unwrap());
    let golden = read_golden_tree("examples/generated_tree/sabre_booking_client");
    assert_tree_bytes("sabre client tree vs golden", &golden, &generated);
}

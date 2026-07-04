//! The real-world spec audit (docs/SPEC_COVERAGE.md): generation,
//! compile, and wire round-trip matrix over public OpenAPI documents.
//!
//! Heavy and network-adjacent, so every test is `#[ignore]`d. Re-run:
//!
//! ```text
//! ./specs/external/fetch.sh
//! cargo test --release --test real_world -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Matrix tests print `MATRIX <spec> <mode> ...` outcome lines; wire
//! tests print a `WIRE <spec> ...` summary plus one line per failing
//! example pair. Individual example failures do **not** fail the test
//! (real specs carry garbage examples — the audit document classifies
//! them); harness-level failures (spec missing, generation or compile
//! gate breaking where the matrix says it must succeed) do.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use openapi_codegen::{Generator, StyleProfile, load_spec, plan_file_tree};
use serde_json::Value;

fn corpus(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("specs/external")
        .join(name);
    assert!(
        path.exists(),
        "missing corpus spec {name}; run ./specs/external/fetch.sh first",
    );
    path
}

fn generator(path: &Path, profile: StyleProfile) -> Generator {
    Generator::new(path).profile(profile)
}

/// Top-level type names declared more than once in flat output — the
/// inline-synthetic vs named-schema collision family (typify names an
/// inline property type `{Parent}{Prop}`, which can equal a real
/// schema's name).
fn duplicate_type_names(source: &str) -> std::collections::BTreeSet<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        for prefix in ["pub struct ", "pub enum ", "pub type "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    *counts.entry(name).or_default() += 1;
                }
            }
        }
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect()
}

/// The documented RFC 6902 escape, mechanized: rename every named
/// schema whose Rust name is duplicated in the output (suffix `-x2`)
/// and rewrite its `$ref`s, iterating to a fixpoint. Returns the
/// generated source, the rename map (schema key → renamed key), and
/// whether `[style] patch = false` was needed (the known deep-patch
/// recursion limitation, both engines, docs/SPEC_COVERAGE.md).
fn generate_with_workarounds(
    path: &Path,
    ladder: Ladder,
) -> (String, BTreeMap<String, String>, usize) {
    let mut renames: BTreeMap<String, String> = BTreeMap::new();
    let mut title_duplicates: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut last_source = String::new();
    for round in 0..5 {
        let mut generator = generator(path, StyleProfile::ApiClient);
        generator = generator.style(move |style| {
            if ladder.patch_off {
                style.patch = false;
            }
            if ladder.no_default_derive {
                // The documented escape for specs whose types cannot
                // satisfy `Default` (e.g. required never-typed fields):
                // the derive lists are style data, and the untagged
                // Default synthesis exists solely to support them.
                style.derives.structs.retain(|derive| derive != "Default");
                style.derives.newtypes.retain(|derive| derive != "Default");
                style.untagged_enum_defaults = false;
            }
            if ladder.wire_faithful_names {
                // The api-client preset imposes camelCase wire names —
                // the house style. Wire-fidelity tests must keep the
                // spec's own property names (GitHub, Plaid, and
                // DigitalOcean are snake_case APIs).
                style.rename_all = None;
            }
        });
        let applied_renames = renames.clone();
        let applied_titles = title_duplicates.clone();
        generator = generator.patch_spec_with(move |spec| {
            apply_renames(spec, &applied_renames);
            dedupe_titles(spec, &applied_titles, &mut BTreeMap::new());
        });
        let source = generator.generate_to_string().expect("generation succeeds");
        let duplicates = duplicate_type_names(&source);
        if duplicates.is_empty() {
            return (source, renames, round);
        }
        last_source = source;

        // Two mechanisms, both applied every round: rename *named*
        // schemas whose Rust names are duplicated, and de-duplicate
        // inline `title`s producing the same idents.
        let document = load_spec(path).unwrap();
        let schemas = document
            .pointer("/components/schemas")
            .and_then(Value::as_object)
            .unwrap();
        // Two schema keys can share one Rust ident (`pay_frequency`
        // vs `PayFrequency`): each colliding key gets a distinct
        // ordinal suffix.
        let mut ordinal: BTreeMap<String, usize> = BTreeMap::new();
        for key in schemas.keys() {
            if renames.contains_key(key) {
                continue;
            }
            let ident = typify::rust_type_ident(key);
            if duplicates.contains(&ident) {
                let n = ordinal.entry(ident).or_insert(1);
                *n += 1;
                renames.insert(key.clone(), format!("{key}-x{n}"));
            }
        }
        title_duplicates.extend(duplicates);
    }
    let remaining = duplicate_type_names(&last_source);
    println!(
        "WORKAROUND: {} duplicate names remain after 5 rounds: {:?}",
        remaining.len(),
        remaining.iter().take(8).collect::<Vec<_>>(),
    );
    (last_source, renames, 5)
}

/// Suffix every duplicated inline `title` occurrence past the first
/// with a distinct marker, so typify's title-derived names de-collide.
fn dedupe_titles(node: &mut Value, duplicates: &std::collections::BTreeSet<String>, seen: &mut BTreeMap<String, usize>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(title)) = map.get("title") {
                let ident = typify::rust_type_ident(title);
                if duplicates.contains(&ident) {
                    let count = seen.entry(ident).or_default();
                    *count += 1;
                    if *count > 1 {
                        let suffixed = format!("{title} V{count}");
                        map.insert("title".to_string(), Value::String(suffixed));
                    }
                }
            }
            map.values_mut()
                .for_each(|value| dedupe_titles(value, duplicates, seen));
        }
        Value::Array(entries) => entries
            .iter_mut()
            .for_each(|value| dedupe_titles(value, duplicates, seen)),
        _ => {}
    }
}

/// Move renamed schema keys and rewrite every `$ref` to them.
fn apply_renames(spec: &mut Value, renames: &BTreeMap<String, String>) {
    if renames.is_empty() {
        return;
    }
    if let Some(schemas) = spec
        .pointer_mut("/components/schemas")
        .and_then(Value::as_object_mut)
    {
        for (old, new) in renames {
            if let Some(schema) = schemas.remove(old) {
                schemas.insert(new.clone(), schema);
            }
        }
    }
    fn rewrite(node: &mut Value, renames: &BTreeMap<String, String>) {
        match node {
            Value::String(text) => {
                if let Some(key) = text.strip_prefix("#/components/schemas/")
                    && let Some(new) = renames.get(key)
                {
                    *text = format!("#/components/schemas/{new}");
                }
            }
            Value::Object(map) => map.values_mut().for_each(|value| rewrite(value, renames)),
            Value::Array(entries) => entries.iter_mut().for_each(|value| rewrite(value, renames)),
            _ => {}
        }
    }
    rewrite(spec, renames);
}

/// One generation mode of the matrix; returns a printable outcome.
fn run_mode(label: &str, run: impl FnOnce() -> Result<String, anyhow::Error>) -> bool {
    let start = Instant::now();
    match run() {
        Ok(note) => {
            println!("MATRIX {label}: ok in {:.1?} ({note})", start.elapsed());
            true
        }
        Err(error) => {
            let message = format!("{error:#}");
            let first = message
                .lines()
                .find(|line| line.contains("error"))
                .or_else(|| message.lines().next())
                .unwrap_or_default();
            println!("MATRIX {label}: ERROR in {:.1?}: {first}", start.elapsed());
            false
        }
    }
}

/// The generation + compile matrix for one spec that is expected to
/// generate: ApiClient flat, ApiClient split tree, Typify flat, the
/// verify gate over both profiles' flat output as-is, and — when the
/// plain gate fails — the documented workaround ladder (schema renames
/// for the inline-name collision family; `patch = false` for the
/// deep-patch recursion limitation), which must compile.
fn matrix(spec: &str, path: &Path) {
    let flat_ok = run_mode(&format!("{spec} api-client flat"), || {
        let out = generator(path, StyleProfile::ApiClient).generate_to_string()?;
        let duplicates = duplicate_type_names(&out);
        Ok(format!(
            "{} lines, {} duplicated type names",
            out.lines().count(),
            duplicates.len(),
        ))
    });
    run_mode(&format!("{spec} api-client split-tree"), || {
        let file = generator(path, StyleProfile::ApiClient)
            .split_request_response(true)
            .generate_to_syn_file()?;
        let tree = plan_file_tree(&file, path);
        Ok(format!("{} files", tree.len()))
    });
    run_mode(&format!("{spec} typify flat"), || {
        let out = generator(path, StyleProfile::Typify).generate_to_string()?;
        Ok(format!("{} lines", out.lines().count()))
    });
    let compile_ok = run_mode(&format!("{spec} api-client verify-gate"), || {
        generator(path, StyleProfile::ApiClient)
            .verify_compile(true)
            .generate_to_string()?;
        Ok("compiles as-is".to_string())
    });
    run_mode(&format!("{spec} typify verify-gate"), || {
        generator(path, StyleProfile::Typify)
            .verify_compile(true)
            .generate_to_string()?;
        Ok("compiles as-is".to_string())
    });
    assert!(flat_ok, "{spec}: api-client flat generation must succeed");

    if !compile_ok {
        let workaround_ok = run_mode(&format!("{spec} api-client workarounds"), || {
            let (ladder, label) = escalate(spec, path)?;
            let (_, renames, rounds) = generate_with_workarounds(path, ladder);
            Ok(format!(
                "compiles with {} schema renames ({rounds} rounds){label}",
                renames.len(),
            ))
        });
        assert!(
            workaround_ok,
            "{spec}: output must compile at least under the documented workarounds",
        );
    }
}

/// Walk the config ladder until the output compiles; returns the rung
/// and its description.
fn escalate(spec: &str, path: &Path) -> Result<(Ladder, &'static str), anyhow::Error> {
    let rungs: [(Ladder, &'static str); 3] = [
        (Ladder::default(), ""),
        (
            Ladder {
                patch_off: true,
                ..Ladder::default()
            },
            " + patch = false",
        ),
        (
            Ladder {
                patch_off: true,
                no_default_derive: true,
                ..Ladder::default()
            },
            " + patch = false + no Default derive",
        ),
    ];
    let mut last = String::new();
    for (ladder, label) in rungs {
        let (source, _, _) = generate_with_workarounds(path, ladder);
        match scratch_check(spec, &source) {
            Ok(()) => return Ok((ladder, label)),
            Err(error) => last = error,
        }
    }
    Err(anyhow::Error::msg(last))
}

/// `cargo check` a generated source in a scratch crate (the wire
/// harness's manifest).
fn scratch_check(spec: &str, source: &str) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("openapi-codegen-audit-check-{spec}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), WIRE_MANIFEST).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        format!("#![allow(unused)]\n{source}"),
    )
    .unwrap();
    let output = std::process::Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(&dir)
        .output()
        .expect("cargo check");
    if output.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    } else {
        // Keep the crate for inspection, like the verify gate does.
        Err(format!(
            "(scratch kept at {}) {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr),
        ))
    }
}

const WIRE_MANIFEST: &str = r#"[package]
name = "wire-audit"
version = "0.0.0"
edition = "2024"

[features]
schemars = []

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_with = "3"
struct-patch = { version = "0.10", default-features = false, features = ["status", "op", "nesting", "none_as_default"] }
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["serde"] }
regress = "0.10"

[profile.dev]
debug = false
"#;

// ─── Wire round-trips ────────────────────────────────────────────────────────

/// Style fallbacks applied on top of the api-client preset, mirroring
/// real config decisions a consumer of the given spec would make.
#[derive(Clone, Copy, Default)]
struct Ladder {
    /// `[style] patch = false` — the deep-patch-recursion limitation.
    patch_off: bool,
    /// Drop `Default` from the derive lists (required never-typed
    /// fields can't satisfy it).
    no_default_derive: bool,
    /// `rename-all` unset: keep the spec's own wire names.
    wire_faithful_names: bool,
}

/// One (type, payload) pair to round-trip.
struct WirePair {
    label: String,
    rust_type: String,
    payload: Value,
}

/// The schema key of a `#/components/schemas/<key>` reference.
fn schema_ref_key(schema: &Value) -> Option<&str> {
    schema
        .get("$ref")?
        .as_str()?
        .strip_prefix("#/components/schemas/")
}

/// Harvest whole-payload examples paired with **named** schema types:
/// schema-level `example`s, plus request/response `content` examples
/// whose schema is a `$ref` (inline `example` and `examples.*.value`,
/// following `$ref`s into `components.examples`).
fn harvest_examples(document: &Value) -> Vec<WirePair> {
    let mut pairs = Vec::new();
    let mut push = |key: &str, label: String, payload: &Value| {
        pairs.push(WirePair {
            label,
            rust_type: typify::rust_type_ident(key),
            payload: payload.clone(),
        });
    };

    if let Some(schemas) = document.pointer("/components/schemas").and_then(Value::as_object) {
        for (key, schema) in schemas {
            if let Some(example) = schema.get("example") {
                push(key, format!("schemas/{key}/example"), example);
            }
            if let Some(examples) = schema.get("examples").and_then(Value::as_array) {
                for (index, example) in examples.iter().enumerate() {
                    push(key, format!("schemas/{key}/examples[{index}]"), example);
                }
            }
        }
    }

    // Operation-level content examples paired through their `$ref`
    // schema. Media-type objects live at `content.application/json`.
    fn walk_content(document: &Value, node: &Value, context: String, out: &mut Vec<(String, String, Value)>) {
        let Some(map) = node.as_object() else { return };
        if let Some(content) = map.get("content").and_then(Value::as_object) {
            for (media, media_object) in content {
                if !media.starts_with("application/json") {
                    continue;
                }
                let Some(key) = media_object.get("schema").and_then(schema_ref_key) else {
                    continue;
                };
                if let Some(example) = media_object.get("example") {
                    out.push((key.to_string(), format!("{context}/example"), example.clone()));
                }
                if let Some(examples) = media_object.get("examples").and_then(Value::as_object) {
                    for (name, example_object) in examples {
                        // Either an inline `value` or a `$ref` into
                        // `components.examples`.
                        let resolved = example_object.get("value").cloned().or_else(|| {
                            let pointer = example_object
                                .get("$ref")?
                                .as_str()?
                                .strip_prefix("#")?
                                .replace("~1", "/")
                                .replace("~0", "~");
                            document.pointer(&pointer)?.get("value").cloned()
                        });
                        if let Some(value) = resolved {
                            out.push((key.to_string(), format!("{context}/examples/{name}"), value));
                        }
                    }
                }
            }
        }
        for (child_key, child) in map {
            if matches!(child_key.as_str(), "schema" | "example" | "examples" | "content") {
                continue;
            }
            if child.is_object() {
                walk_content(document, child, format!("{context}/{child_key}"), out);
            }
        }
    }
    let mut op_pairs = Vec::new();
    if let Some(paths) = document.get("paths") {
        walk_content(document, paths, "paths".to_string(), &mut op_pairs);
    }
    for (key, label, payload) in op_pairs {
        push(&key, label, &payload);
    }
    pairs
}

/// Checked-in real payload fixtures named `<spec>__<schema-key>.json`.
fn fixture_pairs(spec: &str) -> Vec<WirePair> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_world");
    let mut pairs = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return pairs;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&format!("{spec}__")) else {
            continue;
        };
        let Some(key) = rest.strip_suffix(".json") else {
            continue;
        };
        let payload: Value =
            serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
        pairs.push(WirePair {
            label: format!("fixture/{key}"),
            rust_type: typify::rust_type_ident(key),
            payload,
        });
    }
    pairs
}

/// Does the generated source define a named type `name`?
fn defines_type(source: &str, name: &str) -> bool {
    [
        format!("pub struct {name} "),
        format!("pub struct {name}("),
        format!("pub struct {name} {{"),
        format!("pub enum {name} "),
        format!("pub type {name} "),
    ]
    .iter()
    .any(|needle| source.contains(needle.as_str()))
}

/// Build and run the wire scratch crate: the ApiClient flat output as a
/// module plus one generic round-trip check per harvested pair.
fn wire_roundtrips(spec: &str, path: &Path) {
    // The workaround ladder mirrors the matrix, plus wire-faithful
    // naming: the api-client preset's `rename-all = "camelCase"` is
    // the sabre house style; testing against snake_case APIs (GitHub,
    // Plaid, DigitalOcean) requires keeping the spec's own wire names
    // — exactly the config call a real consumer of those specs makes.
    let start = Instant::now();
    let (source, renames) = {
        let mut chosen = None;
        for (patch_off, no_default) in [(false, false), (true, false), (true, true)] {
            let ladder = Ladder {
                patch_off,
                no_default_derive: no_default,
                wire_faithful_names: true,
            };
            let (source, renames, _) = generate_with_workarounds(path, ladder);
            if scratch_check(spec, &source).is_ok() {
                if patch_off || no_default {
                    println!(
                        "WIRE {spec}: ladder rung patch_off={patch_off} no_default={no_default}",
                    );
                }
                chosen = Some((source, renames));
                break;
            }
        }
        chosen.unwrap_or_else(|| panic!("{spec}: no ladder rung compiles"))
    };
    let generation_time = start.elapsed();

    let document = load_spec(path).unwrap();
    let mut pairs = harvest_examples(&document);
    pairs.extend(fixture_pairs(spec));
    for pair in &mut pairs {
        // Re-key types whose schemas the rename workaround moved.
        for (old, new) in &renames {
            if pair.rust_type == typify::rust_type_ident(old) {
                pair.rust_type = typify::rust_type_ident(new);
            }
        }
    }
    // Only pairs whose type actually generated (inline/alias-elided
    // schemas have no named type to test), and only object/array
    // payloads (scalar examples exercise nothing interesting).
    pairs.retain(|pair| {
        (pair.payload.is_object() || pair.payload.is_array())
            && defines_type(&source, &pair.rust_type)
    });
    assert!(
        !pairs.is_empty(),
        "{spec}: no example pairs harvested — harness rot or spec without examples",
    );

    // Deduplicate identical (type, payload) pairs.
    let mut seen = std::collections::BTreeSet::new();
    pairs.retain(|pair| seen.insert((pair.rust_type.clone(), pair.payload.to_string())));

    let dir = std::env::temp_dir().join(format!("openapi-codegen-wire-{spec}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), WIRE_MANIFEST).unwrap();
    std::fs::write(dir.join("src/types.rs"), &source).unwrap();
    let payloads: Vec<&Value> = pairs.iter().map(|pair| &pair.payload).collect();
    std::fs::write(
        dir.join("payloads.json"),
        serde_json::to_string(&payloads).unwrap(),
    )
    .unwrap();

    let mut main = String::from(HARNESS_PRELUDE);
    for (index, pair) in pairs.iter().enumerate() {
        main.push_str(&format!(
            "        check::<types::{}>({index}, &payloads[{index}]);\n",
            pair.rust_type,
        ));
    }
    // Deep expandable unions (Stripe) overflow the default 8 MB stack
    // during untagged deserialization; a fat-stack worker keeps the
    // harness measuring wire behavior rather than stack limits.
    main.push_str("    }).unwrap().join().unwrap();\n}\n");
    std::fs::write(dir.join("src/main.rs"), main).unwrap();

    let build_start = Instant::now();
    let output = std::process::Command::new("cargo")
        .args(["run", "--quiet"])
        .current_dir(&dir)
        .output()
        .expect("cargo run for wire scratch crate");
    let build_run_time = build_start.elapsed();
    assert!(
        output.status.success(),
        "{spec}: wire scratch crate failed to build/run (kept at {}):\n{}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr),
    );

    // Aggregate `STATUS\tindex\tdetail` lines.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut by_status: BTreeMap<&str, usize> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(status), Some(index)) = (parts.next(), parts.next()) else {
            continue;
        };
        let detail = parts.next().unwrap_or_default();
        *by_status.entry(status).or_default() += 1;
        if status != "OK" {
            let index: usize = index.parse().unwrap();
            failures.push(format!(
                "  {status} {} [{}] {}",
                pairs[index].rust_type,
                pairs[index].label,
                detail.chars().take(180).collect::<String>(),
            ));
        }
    }
    println!(
        "WIRE {spec}: {} pairs — {:?} (gen {:.1?}, build+run {:.1?})",
        pairs.len(),
        by_status,
        generation_time,
        build_run_time,
    );
    for failure in &failures {
        println!("{failure}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The scratch `main.rs` prelude: the generic round-trip check.
/// Statuses: OK; DESER (example does not deserialize); SER; REPARSE
/// (serialize→deserialize fails); UNSTABLE (v != reparse(serialize(v)));
/// LOSSY (re-serialization loses payload data beyond the documented
/// None-elision, compared null-stripped both sides).
const HARNESS_PRELUDE: &str = r#"#[allow(unused_imports, dead_code, clippy::all)]
mod types;

use serde_json::Value;

fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, entry| !entry.is_null());
            map.values_mut().for_each(strip_nulls);
        }
        Value::Array(entries) => entries.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// `40` and `40.0` are the same JSON number; a `number`-typed field
/// legitimately re-serializes an integer example as a float lexeme.
fn scalar_equal(a: &Value, b: &Value) -> bool {
    if let (Value::Number(na), Value::Number(nb)) = (a, b)
        && let (Some(fa), Some(fb)) = (na.as_f64(), nb.as_f64())
    {
        return fa == fb;
    }
    a == b
}

fn first_diff(path: String, a: &Value, b: &Value, out: &mut Option<String>) {
    if out.is_some() {
        return;
    }
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            for (key, va) in ma {
                match mb.get(key) {
                    Some(vb) => first_diff(format!("{path}/{key}"), va, vb, out),
                    None => *out = Some(format!("{path}/{key} missing in output")),
                }
                if out.is_some() {
                    return;
                }
            }
            for key in mb.keys() {
                if !ma.contains_key(key) {
                    *out = Some(format!("{path}/{key} added in output"));
                    return;
                }
            }
        }
        (Value::Array(aa), Value::Array(ab)) => {
            if aa.len() != ab.len() {
                *out = Some(format!("{path} array length {} vs {}", aa.len(), ab.len()));
                return;
            }
            for (index, (va, vb)) in aa.iter().zip(ab).enumerate() {
                first_diff(format!("{path}/{index}"), va, vb, out);
                if out.is_some() {
                    return;
                }
            }
        }
        _ if !scalar_equal(a, b) => *out = Some(format!("{path}: {a} vs {b}")),
        _ => {}
    }
}

fn check<T>(index: usize, example: &Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize + PartialEq,
{
    let parsed: T = match serde_json::from_value(example.clone()) {
        Ok(parsed) => parsed,
        Err(error) => {
            println!("DESER\t{index}\t{error}");
            return;
        }
    };
    let back = match serde_json::to_value(&parsed) {
        Ok(back) => back,
        Err(error) => {
            println!("SER\t{index}\t{error}");
            return;
        }
    };
    match serde_json::from_value::<T>(back.clone()) {
        Ok(reparsed) if reparsed == parsed => {}
        Ok(_) => {
            println!("UNSTABLE\t{index}\t");
            return;
        }
        Err(error) => {
            println!("REPARSE\t{index}\t{error}");
            return;
        }
    }
    let mut original = example.clone();
    strip_nulls(&mut original);
    let mut output = back;
    strip_nulls(&mut output);
    let mut diff = None;
    first_diff(String::new(), &original, &output, &mut diff);
    match diff {
        None => println!("OK\t{index}\t"),
        Some(diff) => println!("LOSSY\t{index}\t{diff}"),
    }
}

fn main() {
    let payloads: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string("payloads.json").unwrap()).unwrap();
    std::thread::Builder::new().stack_size(512 * 1024 * 1024).spawn(move || {
"#;

// ─── Per-spec matrix tests ───────────────────────────────────────────────────

#[test]
#[ignore = "network corpus + heavy; see file docs"]
fn matrix_github() {
    matrix("github", &corpus("github.json"));
}

#[test]
#[ignore = "network corpus + heavy; see file docs"]
fn matrix_stripe() {
    matrix("stripe", &corpus("stripe.json"));
}

#[test]
#[ignore = "network corpus + heavy; see file docs"]
fn matrix_plaid() {
    matrix("plaid", &corpus("plaid.yml"));
}

#[test]
#[ignore = "network corpus + heavy; see file docs"]
fn matrix_digitalocean() {
    matrix("digitalocean", &corpus("digitalocean.yaml"));
}

/// Swagger 2.0 must fail loudly with an actionable version-gate
/// message, in every mode.
#[test]
#[ignore = "network corpus"]
fn matrix_docker_swagger_2_0_is_loud() {
    let error = format!(
        "{:#}",
        generator(&corpus("docker-v2.0.yaml"), StyleProfile::ApiClient)
            .generate_to_string()
            .unwrap_err(),
    );
    println!("MATRIX docker-2.0: loud error: {error}");
    assert!(
        error.contains("2.0") && error.to_lowercase().contains("convert"),
        "version gate must be loud and actionable: {error}",
    );
}

/// OpenAPI 3.1 is a documented seam: accepted, with 3.1-only keywords
/// (`const`, `prefixItems`) passing through unmodeled. Characterize —
/// don't assert an error that doesn't exist.
#[test]
#[ignore = "network corpus"]
fn matrix_museum_3_1() {
    matrix("museum-3.1", &corpus("museum-3.1.yaml"));
}

// ─── Per-spec wire tests ─────────────────────────────────────────────────────

#[test]
#[ignore = "network corpus + heavy; see file docs"]
fn wire_github() {
    wire_roundtrips("github", &corpus("github.json"));
}

#[test]
#[ignore = "network corpus + heavy; see file docs"]
fn wire_stripe() {
    wire_roundtrips("stripe", &corpus("stripe.json"));
}

#[test]
#[ignore = "network corpus + heavy; see file docs"]
fn wire_plaid() {
    wire_roundtrips("plaid", &corpus("plaid.yml"));
}

#[test]
#[ignore = "network corpus + heavy; see file docs"]
fn wire_digitalocean() {
    wire_roundtrips("digitalocean", &corpus("digitalocean.yaml"));
}

#[test]
#[ignore = "network corpus"]
fn wire_museum_3_1() {
    wire_roundtrips("museum-3.1", &corpus("museum-3.1.yaml"));
}

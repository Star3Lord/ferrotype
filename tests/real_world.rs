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
            let first = message.lines().next().unwrap_or_default();
            println!("MATRIX {label}: ERROR in {:.1?}: {first}", start.elapsed());
            false
        }
    }
}

/// The generation + compile matrix for one spec that is expected to
/// generate: ApiClient flat, ApiClient split tree, Typify flat, then
/// the verify gate over both profiles' flat output.
fn matrix(spec: &str, path: &Path) {
    let flat_ok = run_mode(&format!("{spec} api-client flat"), || {
        let out = generator(path, StyleProfile::ApiClient).generate_to_string()?;
        Ok(format!("{} lines", out.lines().count()))
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
        Ok("compiles".to_string())
    });
    run_mode(&format!("{spec} typify verify-gate"), || {
        generator(path, StyleProfile::Typify)
            .verify_compile(true)
            .generate_to_string()?;
        Ok("compiles".to_string())
    });
    assert!(flat_ok, "{spec}: api-client flat generation must succeed");
    assert!(compile_ok, "{spec}: api-client output must pass the compile gate");
}

// ─── Wire round-trips ────────────────────────────────────────────────────────

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
    let start = Instant::now();
    let source = generator(path, StyleProfile::ApiClient)
        .generate_to_string()
        .expect("wire tests need successful generation");
    let generation_time = start.elapsed();

    let document = load_spec(path).unwrap();
    let mut pairs = harvest_examples(&document);
    pairs.extend(fixture_pairs(spec));
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
    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
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
"#,
    )
    .unwrap();
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
            "    check::<types::{}>({index}, &payloads[{index}]);\n",
            pair.rust_type,
        ));
    }
    main.push_str("}\n");
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

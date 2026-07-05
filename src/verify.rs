//! The opt-in compile gate: `cargo check` the generated output in a
//! scratch crate before declaring generation successful.
//!
//! Enabled by [`Generator::verify_compile`](crate::Generator::verify_compile),
//! the CLI's `--verify`, or a `[verify] enabled = true` table in
//! codegen.toml. The gate assembles a throwaway crate in a temp
//! directory — edition 2024, the generated source mounted as the lib —
//! with the runtime dependencies generated code needs by default
//! (serde, serde_with, struct-patch) plus any `[verify] dependencies`
//! lines from the config (raw Cargo dependency lines; a user line for a
//! default crate replaces the default). `cargo check` runs with the
//! user's toolchain; on failure, generation fails with the captured
//! rustc output and the scratch crate is left in place for inspection.
//!
//! The gate runs *before* any output file is written, so a failing run
//! leaves previously generated files untouched. The declared
//! dependencies must be resolvable in the environment running the
//! generator (network or a warm cargo cache).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, bail};

use crate::Result;
use crate::config::VerifyConfig;

/// Dependencies every scratch crate starts with — the runtime surface
/// of generated code. A `[verify] dependencies` line naming the same
/// crate replaces the default.
const DEFAULT_DEPENDENCIES: &[&str] = &[
    r#"serde = { version = "1", features = ["derive"] }"#,
    r#"serde_with = "3""#,
    r#"struct-patch = { version = "0.10", default-features = false, features = ["status", "op", "nesting", "none_as_default"] }"#,
];

/// Well-known crates generated code may reference depending on the
/// spec and style — typify's chrono/uuid format defaults,
/// `serde_json::Value` for free-form schemas, `regress` for validating
/// string newtypes, and the client emitter's HTTP surface (reqwest /
/// reqwest-middleware / async-trait, plus base64 when an OAuth2
/// provider is emitted). Each is added to the scratch crate only when
/// the rendered output actually mentions its marker; user `[verify]
/// dependencies` lines still win name collisions. (The `::reqwest::`
/// marker keeps its trailing `::` so it cannot fire on
/// `::reqwest_middleware` alone, mirroring `::uuid::`.)
const CONDITIONAL_DEPENDENCIES: &[(&str, &str)] = &[
    ("::chrono", r#"chrono = { version = "0.4", features = ["serde"] }"#),
    ("::uuid::", r#"uuid = { version = "1", features = ["serde"] }"#),
    ("::serde_json", r#"serde_json = "1""#),
    ("::regress", r#"regress = "0.10""#),
    (
        "::reqwest::",
        r#"reqwest = { version = "0.13", default-features = false, features = ["json", "form", "query"] }"#,
    ),
    (
        "::reqwest_middleware",
        r#"reqwest-middleware = { version = "0.5", features = ["json", "query"] }"#,
    ),
    ("::async_trait", r#"async-trait = "0.1""#),
    ("::base64", r#"base64 = "0.22""#),
];

/// The conditional dependency lines whose markers appear in any of the
/// rendered `sources`.
fn detect_conditional_dependencies<'a>(
    sources: impl Iterator<Item = &'a str>,
) -> Vec<&'static str> {
    let mut found = Vec::new();
    for source in sources {
        for (marker, line) in CONDITIONAL_DEPENDENCIES {
            if !found.contains(line) && source.contains(marker) {
                found.push(*line);
            }
        }
    }
    found
}

/// Gate a single-file output: the generated source becomes the scratch
/// crate's `src/lib.rs` body.
pub(crate) fn verify_single_file(source: &str, config: &VerifyConfig) -> Result<()> {
    let auto = detect_conditional_dependencies(std::iter::once(source));
    let lib = format!("#![allow(unused)]\n{source}");
    run_gate(config, &auto, |src| {
        std::fs::write(src.join("lib.rs"), &lib).context("failed to write scratch lib.rs")
    })
}

/// Gate a folder-tree output: the planned files mount under a `types`
/// module of the scratch crate.
pub(crate) fn verify_tree(
    files: &BTreeMap<PathBuf, String>,
    config: &VerifyConfig,
) -> Result<()> {
    let auto = detect_conditional_dependencies(files.values().map(String::as_str));
    run_gate(config, &auto, |src| {
        std::fs::write(src.join("lib.rs"), "#![allow(unused)]\npub mod types;\n")
            .context("failed to write scratch lib.rs")?;
        for (rel_path, contents) in files {
            let path = src.join("types").join(rel_path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            std::fs::write(&path, contents)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        Ok(())
    })
}

/// Assemble the scratch crate (`write_sources` populates `src/`), run
/// `cargo check`, and fail with the compiler output on error.
fn run_gate(
    config: &VerifyConfig,
    auto_dependencies: &[&str],
    write_sources: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "openapi-codegen-verify-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to clear {}", dir.display()))?;
    }
    let src = dir.join("src");
    std::fs::create_dir_all(&src)
        .with_context(|| format!("failed to create {}", src.display()))?;
    std::fs::write(dir.join("Cargo.toml"), manifest(config, auto_dependencies))
        .context("failed to write scratch Cargo.toml")?;
    write_sources(&src)?;

    let output = std::process::Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(&dir)
        .output()
        .context("failed to run `cargo check` (is cargo on PATH?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "generated code failed to compile (scratch crate kept at {}):\n{stderr}{}",
            dir.display(),
            missing_crate_hint(&stderr),
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// A targeted hint appended to the compiler output when the failure
/// includes unresolved-crate errors: generated code can reference
/// crates the gate doesn't know about, and the fix — a `[verify]
/// dependencies` line — is not obvious from the raw rustc output.
fn missing_crate_hint(stderr: &str) -> String {
    let mut missing: Vec<&str> = Vec::new();
    for line in stderr.lines() {
        if !(line.contains("E0433")
            || line.contains("use of undeclared crate")
            || line.contains("can't find crate"))
        {
            continue;
        }
        if let Some(name) = line.split('`').nth(1)
            && !name.is_empty()
            && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            && !missing.contains(&name)
        {
            missing.push(name);
        }
    }
    if missing.is_empty() {
        return String::new();
    }
    format!(
        "\nhint: the scratch crate could not resolve: {}. The generated code references \
         crates the verify gate does not declare by default; add them to the config:\n\
         \n  [verify]\n  dependencies = ['{} = \"1\"']\n",
        missing.join(", "),
        missing[0],
    )
}

/// The scratch crate manifest: defaults, the auto-detected conditional
/// dependencies, and the config's raw dependency lines — user lines
/// winning on crate-name collisions. A `schemars = []` feature is
/// declared (never enabled) so the generated `#[cfg_attr(feature =
/// "schemars", ...)]` gates don't trip `unexpected_cfgs`.
fn manifest(config: &VerifyConfig, auto_dependencies: &[&str]) -> String {
    let dep_name = |line: &str| {
        line.split('=')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let user_names: Vec<String> = config.dependencies.iter().map(|d| dep_name(d)).collect();

    let mut lines = vec![
        "[package]".to_string(),
        "name = \"openapi-codegen-verify\"".to_string(),
        "version = \"0.0.0\"".to_string(),
        "edition = \"2024\"".to_string(),
        String::new(),
        "[lib]".to_string(),
        "path = \"src/lib.rs\"".to_string(),
        String::new(),
        "[features]".to_string(),
        "schemars = []".to_string(),
        String::new(),
        "[dependencies]".to_string(),
    ];
    for default in DEFAULT_DEPENDENCIES
        .iter()
        .chain(auto_dependencies.iter())
    {
        if !user_names.contains(&dep_name(default)) {
            lines.push(default.to_string());
        }
    }
    for dependency in &config.dependencies {
        lines.push(dependency.clone());
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch manifest declares the `schemars` feature (so the
    /// cfg-gated derives stop tripping `unexpected_cfgs`) and includes
    /// conditional dependencies only when the rendered output
    /// references them — with user lines winning name collisions.
    #[test]
    fn manifest_features_and_conditional_dependencies() {
        let bare = manifest(&VerifyConfig::default(), &[]);
        assert!(bare.contains("[features]\nschemars = []"), "{bare}");
        assert!(!bare.contains("serde_json"), "{bare}");
        assert!(bare.contains("serde = {"), "{bare}");

        let auto =
            detect_conditional_dependencies(std::iter::once("pub v: ::serde_json::Value,"));
        assert_eq!(auto, vec![r#"serde_json = "1""#]);
        let with_auto = manifest(&VerifyConfig::default(), &auto);
        assert!(with_auto.contains("serde_json = \"1\""), "{with_auto}");
        assert!(!with_auto.contains("chrono"), "{with_auto}");

        // A user line for the same crate replaces the auto default.
        let config = VerifyConfig {
            enabled: true,
            dependencies: vec![r#"serde_json = { version = "1.0.100" }"#.to_string()],
        };
        let user_wins = manifest(&config, &auto);
        assert_eq!(user_wins.matches("serde_json").count(), 1, "{user_wins}");
        assert!(user_wins.contains("1.0.100"), "{user_wins}");
    }

    /// All four well-known markers are detected, each at most once.
    #[test]
    fn conditional_dependency_detection() {
        let auto = detect_conditional_dependencies(
            [
                "x: ::chrono::DateTime<::chrono::offset::Utc>,",
                "y: ::uuid::Uuid, z: ::serde_json::Value,",
                "let _ = ::regress::Regex::new(p);",
            ]
            .into_iter(),
        );
        assert_eq!(auto.len(), 4, "{auto:?}");
    }

    /// Unresolved-crate failures produce the `[verify] dependencies`
    /// hint naming every missing crate.
    #[test]
    fn missing_crate_hint_names_crates() {
        let stderr = "error[E0433]: cannot find `serde_json` in the crate root\n\
                      error[E0433]: failed to resolve: use of undeclared crate or module `time`\n\
                      error: aborting due to 2 previous errors\n";
        let hint = missing_crate_hint(stderr);
        assert!(hint.contains("serde_json, time"), "{hint}");
        assert!(hint.contains("[verify]"), "{hint}");
        assert!(hint.contains("dependencies = ["), "{hint}");

        assert_eq!(missing_crate_hint("error[E0308]: mismatched types"), "");
    }
}

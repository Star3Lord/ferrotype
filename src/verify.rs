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

/// Gate a single-file output: the generated source becomes the scratch
/// crate's `src/lib.rs` body.
pub(crate) fn verify_single_file(source: &str, config: &VerifyConfig) -> Result<()> {
    let lib = format!("#![allow(unused)]\n{source}");
    run_gate(config, |src| {
        std::fs::write(src.join("lib.rs"), &lib).context("failed to write scratch lib.rs")
    })
}

/// Gate a folder-tree output: the planned files mount under a `types`
/// module of the scratch crate.
pub(crate) fn verify_tree(
    files: &BTreeMap<PathBuf, String>,
    config: &VerifyConfig,
) -> Result<()> {
    run_gate(config, |src| {
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
    std::fs::write(dir.join("Cargo.toml"), manifest(config))
        .context("failed to write scratch Cargo.toml")?;
    write_sources(&src)?;

    let output = std::process::Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(&dir)
        .output()
        .context("failed to run `cargo check` (is cargo on PATH?)")?;
    if !output.status.success() {
        bail!(
            "generated code failed to compile (scratch crate kept at {}):\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The scratch crate manifest: defaults plus the config's raw
/// dependency lines, user lines winning on crate-name collisions.
fn manifest(config: &VerifyConfig) -> String {
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
        "[dependencies]".to_string(),
    ];
    for default in DEFAULT_DEPENDENCIES {
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

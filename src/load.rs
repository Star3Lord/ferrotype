//! Spec loading and RFC 6902 patching.

use std::path::Path;

use anyhow::{Context, bail};
use serde_json::Value;

use crate::Result;

/// Load an OpenAPI document from `path`. `.yaml` / `.yml` extensions are
/// parsed as YAML; everything else as JSON.
pub fn load_spec(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read spec {}", path.display()))?;
    let spec = match path.extension().and_then(|e| e.to_str()) {
        Some("yaml" | "yml") => serde_yaml::from_str(&raw)
            .with_context(|| format!("spec {} did not parse as YAML", path.display()))?,
        _ => serde_json::from_str(&raw)
            .with_context(|| format!("spec {} did not parse as JSON", path.display()))?,
    };
    Ok(spec)
}

/// Deserialized form of a single file under a patches directory.
///
/// `description` is required and must be non-empty — every patch must record
/// what real-world behaviour it captures and why the published spec
/// disagrees.
///
/// `ops` is a verbatim RFC 6902 patch array. Any operation supported by
/// [`json_patch::Patch`] is allowed, including `op: test`, which lets a
/// patch assert a precondition on the spec before touching it — the build
/// fails loudly if a future spec revision silently invalidates the patch.
#[derive(Debug, serde::Deserialize)]
struct PatchFile {
    description: String,
    ops: json_patch::Patch,
}

/// Apply every `.yaml` / `.yml` / `.json` patch file under `dir` to `spec`
/// in lexicographic filename order.
///
/// Patches run between "spec parsed" and any internal lowering, so JSON
/// Pointer paths target the vanilla OpenAPI structure
/// (`/components/schemas/X`, `/paths/~1foo/get/...`) — not the lowered
/// JSON-Schema shape typify consumes downstream.
pub fn apply_patches_dir(spec: &mut Value, dir: &Path) -> Result<()> {
    if !dir.exists() {
        bail!("patches directory {} does not exist", dir.display());
    }

    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read patches directory {}", dir.display()))?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let ext = path.extension().and_then(|e| e.to_str())?;
            (path.is_file() && matches!(ext, "yaml" | "yml" | "json")).then_some(path)
        })
        .collect();
    files.sort();

    for file in &files {
        let raw = std::fs::read_to_string(file)
            .with_context(|| format!("failed to read patch {}", file.display()))?;
        let parsed: PatchFile = match file.extension().and_then(|e| e.to_str()) {
            Some("json") => serde_json::from_str(&raw)
                .with_context(|| format!("patch {} did not parse as JSON", file.display()))?,
            _ => serde_yaml::from_str(&raw)
                .with_context(|| format!("patch {} did not parse as YAML", file.display()))?,
        };
        if parsed.description.trim().is_empty() {
            bail!(
                "patch {} has an empty `description`; every patch must record \
                 what real-world behaviour it captures",
                file.display(),
            );
        }
        json_patch::patch(spec, &parsed.ops)
            .with_context(|| format!("patch {} failed to apply", file.display()))?;

        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let summary = parsed.description.lines().next().unwrap_or("").trim();
        eprintln!(
            "openapi-codegen: applied patch {name}: {n} ops ({summary})",
            n = parsed.ops.0.len(),
        );
    }

    Ok(())
}

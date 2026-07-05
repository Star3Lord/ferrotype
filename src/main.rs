//! CLI for [`openapi_codegen`].
//!
//! ```text
//! openapi-codegen generate --spec specs/petstore.yaml --profile api-client \
//!     --partition-by-operation --output generated/petstore.rs
//! openapi-codegen lower --spec specs/petstore.yaml --output petstore.schema.json
//! ```

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use openapi_codegen::{Generator, StyleProfile};

#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate Rust types from an OpenAPI spec.
    Generate(GenerateArgs),
    /// Lower an OpenAPI spec to a plain JSON Schema document
    /// (`{"definitions": {...}}`) consumable by `typify::import_types!`
    /// or `cargo typify`.
    Lower(LowerArgs),
    /// Take ownership of a generated file: verify its `// @generated`
    /// marker and rewrite the header to `// @ejected`. Regeneration
    /// then skips the file (never overwrites or deletes it); delete it
    /// and regenerate to restore the generated version.
    Eject(EjectArgs),
}

#[derive(clap::Args)]
struct GenerateArgs {
    /// The OpenAPI document (YAML or JSON).
    #[arg(long, value_name = "PATH")]
    spec: PathBuf,

    /// Where to write the generated Rust as a single file. `-` writes to
    /// stdout.
    #[arg(
        long,
        short,
        value_name = "PATH",
        conflicts_with = "output_dir",
        required_unless_present = "output_dir"
    )]
    output: Option<PathBuf>,

    /// Where to write the generated Rust as a directory tree: one file
    /// per partition module plus a root `mod.rs`. Stale `// @generated`
    /// files from previous runs are cleaned up.
    #[arg(long, value_name = "DIR")]
    output_dir: Option<PathBuf>,

    /// Style profile controlling the shape of the generated types.
    #[arg(long, value_enum, default_value_t = StyleProfile::Typify)]
    profile: StyleProfile,

    /// A codegen.toml overriding the profile preset: any `[style]` key
    /// plus `[types]` / `[fields]` per-type and per-field overrides.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Group types into one module per OpenAPI operation (plus `shared`).
    #[arg(long, default_value_t = false)]
    partition_by_operation: bool,

    /// Partition each operation's types into `request` / `response`
    /// submodules, with cross-operation types classified into
    /// `shared::{request, response, enums, common}`. Implies
    /// `--partition-by-operation`.
    #[arg(long, default_value_t = false)]
    split_request_response: bool,

    /// Directory of RFC 6902 patch files applied to the spec before
    /// processing. Each file: `{ description: <non-empty>, ops: [...] }`.
    #[arg(long, value_name = "DIR")]
    patches_dir: Option<PathBuf>,

    /// Compile-gate the output: `cargo check` it in a scratch crate and
    /// fail (writing nothing) on compiler errors. Scratch dependencies
    /// beyond serde/serde_with/struct-patch come from the config's
    /// `[verify] dependencies` list.
    #[arg(long, default_value_t = false)]
    verify: bool,

    /// Generate an API client alongside the types: a `client` module
    /// (reqwest-middleware client, auth from securitySchemes, body
    /// hooks) and — with --output-dir — a user-owned `ext` module.
    /// The config's `[client]` table holds the finer knobs.
    #[arg(long, default_value_t = false)]
    client: bool,
}

#[derive(clap::Args)]
struct EjectArgs {
    /// The generated file to take ownership of.
    #[arg(value_name = "FILE")]
    file: PathBuf,
}

#[derive(clap::Args)]
struct LowerArgs {
    /// The OpenAPI document (YAML or JSON).
    #[arg(long, value_name = "PATH")]
    spec: PathBuf,

    /// Where to write the lowered JSON Schema. `-` writes to stdout.
    #[arg(long, short, value_name = "PATH")]
    output: PathBuf,

    /// Directory of RFC 6902 patch files applied before lowering.
    #[arg(long, value_name = "DIR")]
    patches_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Generate(args) => generate(args),
        Command::Lower(args) => lower(args),
        Command::Eject(args) => openapi_codegen::eject_file(&args.file),
    }
}

fn generate(args: GenerateArgs) -> anyhow::Result<()> {
    let mut generator = Generator::new(&args.spec)
        .profile(args.profile)
        .partition_by_operation(args.partition_by_operation)
        .split_request_response(args.split_request_response);
    if let Some(dir) = &args.patches_dir {
        generator = generator.patches_dir(dir);
    }
    if let Some(config) = &args.config {
        generator = generator.config_file(config);
    }
    if args.verify {
        generator = generator.verify_compile(true);
    }
    if args.client {
        generator = generator.client(true);
    }

    if let Some(dir) = &args.output_dir {
        generator.generate_to_dir(dir)?;
        eprintln!(
            "openapi-codegen: wrote {}{} (profile: {:?})",
            dir.display(),
            std::path::MAIN_SEPARATOR,
            args.profile,
        );
        return Ok(());
    }

    let output = args
        .output
        .as_ref()
        .expect("clap enforces --output unless --output-dir is given");
    if output.as_os_str() == "-" {
        print!("{}", generator.generate_to_string()?);
    } else {
        generator.generate_to_file(output)?;
        eprintln!(
            "openapi-codegen: wrote {} (profile: {:?})",
            output.display(),
            args.profile,
        );
    }
    Ok(())
}

fn lower(args: LowerArgs) -> anyhow::Result<()> {
    let mut spec = openapi_codegen::load_spec(&args.spec)?;
    if let Some(dir) = &args.patches_dir {
        openapi_codegen::apply_patches_dir(&mut spec, dir)?;
    }
    openapi_codegen::lower_to_json_schema(&mut spec);

    let schemas = spec
        .pointer("/components/schemas")
        .cloned()
        .context("OpenAPI spec is missing /components/schemas")?;
    let document = serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": schemas,
    });
    let rendered = serde_json::to_string_pretty(&document)?;

    if args.output.as_os_str() == "-" {
        println!("{rendered}");
    } else {
        std::fs::write(&args.output, rendered)
            .with_context(|| format!("failed to write {}", args.output.display()))?;
        eprintln!("openapi-codegen: wrote {}", args.output.display());
    }
    Ok(())
}

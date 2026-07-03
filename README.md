# openapi-codegen

Generate ergonomic Rust types from OpenAPI specs. Point it at an OpenAPI
3.x document and it produces API-client-style types — bare `Option<T>`
fields, `camelCase` serde renames, `#[serde(flatten)]` inheritance,
`struct_patch` deep patches — partitioned into one module per operation.

Two interchangeable engines run the back half of the pipeline (see
[docs/MIGRATION.md](docs/MIGRATION.md)):

- **`typify` (default)** — the local
  [typify fork](../typify/FORK_FEATURES.md), styled through its
  `TypeSpaceSettings` knobs. Frozen at tag `fork-freeze-20260703`.
- **`ir`** (`--engine ir` / `Generator::engine(Engine::Ir)`) — the owned
  `Spec → IR → passes → emitter` pipeline, styled through declarative
  [`StyleConfig`] data (a `codegen.toml` and/or built-in profile presets).
  Byte-identical to the typify engine on the checked-in fixtures across
  every output mode (`tests/parity.rs` is the gate), with per-type and
  per-field overrides the knob surface never had. Supports the
  `api-client` profile; the `typify` profile *means* the typify engine.

## Pipeline

```text
load (YAML/JSON)
  → patch (RFC 6902 files + Rust hooks)
  → partition (operation reachability → per-op modules + shared)
  → Spec (typed normalization; keeps discriminator/examples)
      ├─ engine typify: Spec → draft-07 → typify fork (knob profiles)
      │                  → post-process (Default impls for untagged oneOf)
      └─ engine ir:     Spec → IR → ordered passes (style as data)
                         → emitter
  → format (prettyplease) → write (idempotent)
```

- **`src/load.rs`** — spec parsing and patch application. Patch files are
  `{ description: <non-empty>, ops: [<RFC 6902 op>...] }`; `op: test`
  entries let a patch assert preconditions so future spec revisions fail
  loudly instead of silently drifting.
- **`src/partition.rs`** — walks every operation's request/response `$ref`
  closure; schemas reachable from exactly one operation land in
  `pub mod <snake_operation_id>`, everything else in `pub mod shared`.
  The opt-in split mode walks each role's closure separately and
  produces nested `<op>/{request,response}` +
  `shared/{request,response,enums,common}` module paths (see
  [below](#requestresponse-splitting-and-folder-output)).
- **`src/lower.rs`** — rewrites `#/components/schemas/` refs, converts
  `nullable: true` to `anyOf [.., null]`, infers missing `type` from
  `format`, normalizes exclusive bounds, and strips OpenAPI-only metadata.
- **`src/profile.rs`** — named presets of typify-fork settings.
- **`src/postprocess.rs`** — synthesizes `impl Default` for enums typify
  can't default (untagged `oneOf` with no unit variant).
- **`src/tree.rs`** — the folder-tree writer: splits the generated module
  tree into one file per partition module.

## Style profiles

| Profile | Output |
|---|---|
| `typify` | Upstream typify output, unchanged. |
| `api-client` | The ergonomic client shape (see below). |

`api-client` generates structs like:

```rust
#[serde_with::skip_serializing_none]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Patch)]
#[serde(rename_all = "camelCase")]
#[patch(attribute(serde_with::skip_serializing_none))]
#[patch(attribute(derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)))]
#[patch(attribute(serde(default, rename_all = "camelCase")))]
#[cfg_attr(feature = "schemars", patch(attribute(derive(schemars::JsonSchema))))]
pub struct CancelBookingRequest {
    pub confirmation_id: String,
    pub booking_source: Option<BookingSourceEnum>,
    pub cancel_all: Option<bool>,
    pub flights: Option<Vec<FlightReference>>,
    #[patch(name = "Option<NotificationPatch>")]
    pub notification: Option<Notification>,
    // ...
}
```

Consumers of the generated code need `serde`, `serde_with`, and
`struct-patch` as dependencies, plus an optional `schemars` Cargo feature
for the cfg-gated `JsonSchema` derives.

## Request/response splitting and folder output

Two opt-in refinements of the per-operation partition, modeled on a
hand-maintained types crate (one folder per operation, request and
response trees kept apart, shared enums in one place):

**`--split-request-response`** (library:
`Generator::split_request_response(true)`, implies
`--partition-by-operation`) classifies every operation's `$ref` entry
points by role — `requestBody` and parameter schemas are *request*,
`responses` are *response* — and BFS-walks each role's closure
separately. Each schema's `(operation, role)` usages across the whole
spec decide its module:

| Usage | Module |
|---|---|
| exactly one `(op, role)` | `<op>::request` / `<op>::response` |
| several ops, request-only | `shared::request` |
| several ops, response-only | `shared::response` |
| shared + simple enum (all unit variants) | `shared::enums` |
| both roles / orphans / inline-schema types | `shared::common` |

`shared::enums` wins over the role rows: atomic enums are safe to share
across request and response shapes, so they live together regardless of
role (the `shared/enums.rs` convention). `shared::common` is the
documented catch-all — the hand-written layout has no equivalent bucket
(it duplicates dual-role types per role), so this crate keeps one copy
in a common pool instead.

One deliberate boundary rule: a role's walk does not traverse *into*
entry points of the opposite role. APIs routinely echo the request
inside the response (Sabre's `CancelBookingResponse.request` is the
`CancelBookingRequest` it answers); traversing that edge would drag the
whole request tree into `shared::common`. The reference still compiles
— the referencing module gets a targeted glob import of the referenced
root's module, exactly how the hand-written crate imports
`cancel_booking::request::CancelBookingRequest` from the response file.

**`--output-dir <DIR>`** (library: `Generator::generate_to_dir`, staged
pipeline: `GeneratedTypes::render_to_dir`; conflicts with `--output`)
writes the module tree as real files instead of one document:

```text
<DIR>/
  mod.rs                  ← pub mod cancel_booking; … pub mod shared;
  cancel_booking/
    mod.rs                ← pub mod request; pub mod response;
    request.rs
    response.rs
  …one folder per operation…
  shared/
    mod.rs
    common.rs
    enums.rs
    request.rs
    response.rs
```

Each partition module becomes `x/mod.rs` (when it contains nested
partition modules) or `x.rs` (when it's a leaf); the small helper
modules typify duplicates into every partition (`error`, `defaults`,
`builder`) stay inline in their parent's file. Every file carries the
`// @generated` header, is prettyplease-formatted, and is written
idempotently. Files from previous runs that are no longer produced are
deleted **only if** their first line starts with `// @generated` —
user-owned files in the same tree are never touched. The two flags
compose freely: `--split-request-response` with `--output` nests the
modules in one file, and `--output-dir` without splitting writes one
file per flat module.

```bash
cargo run -- generate \
    --spec specs/sabre-booking/spec.openapi.yaml \
    --patches-dir specs/sabre-booking/patches \
    --profile api-client \
    --split-request-response \
    --output-dir examples/generated_tree/sabre_booking
```

Mount the result with a `#[path]` attribute (see
`examples/sabre_booking_tree.rs`) or copy it into a crate's `src/`.

## CLI

```bash
# Full pipeline: spec → Rust (one file)
cargo run -- generate \
    --spec specs/sabre-booking/spec.openapi.yaml \
    --patches-dir specs/sabre-booking/patches \
    --profile api-client \
    --partition-by-operation \
    --output examples/generated/sabre_booking.rs

# Same, as a folder tree with request/response splitting
cargo run -- generate \
    --spec specs/sabre-booking/spec.openapi.yaml \
    --patches-dir specs/sabre-booking/patches \
    --profile api-client \
    --split-request-response \
    --output-dir examples/generated_tree/sabre_booking

# Lower only: spec → JSON Schema, for typify::import_types! / cargo-typify
cargo run -- lower --spec specs/petstore.yaml --output petstore.schema.json
```

Install it for use from other crates' scripts with
`cargo install --path .`.

## Library (e.g. from another crate's `build.rs`)

```rust
use openapi_codegen::{Generator, StyleProfile};

Generator::new("specs/booking/spec.openapi.yaml")
    .patches_dir("specs/booking/patches")
    .profile(StyleProfile::ApiClient)
    .partition_by_operation(true)
    // Granular control: every knob of the typify fork is reachable here.
    .customize(|settings| {
        settings.with_derive("Eq".to_string());
    })
    // Rust escape hatch for spec edits RFC 6902 can't express cleanly.
    .patch_spec_with(|spec| {
        // e.g. rename a schema and rewrite refs
        let _ = spec;
    })
    .generate_to_file("src/generated/booking.rs")
    .unwrap();
```

Writes are idempotent — unchanged output leaves the file's mtime alone, so
`build.rs` consumers don't recompile downstream crates needlessly.

## Programmatic pipeline (step-by-step control)

The builder hooks cover spec and settings edits; for everything in
between, `Generator::load()` runs the same pipeline one checkpoint at a
time. Each stage hands back the intermediate artifact — parsed spec,
operation `Partition`, `TypeSpaceSettings`, `TypeSpace`, `syn::File` —
for inspection or mutation before the next stage consumes it:

```rust
use openapi_codegen::{Generator, StyleProfile, render_file};

let mut stage = Generator::new("specs/petstore.yaml")
    .profile(StyleProfile::ApiClient)
    .partition_by_operation(true)
    .load()?;                                    // spec parsed + patched
stage.spec_mut()["components"]["schemas"]["Dog"]["description"] =
    "A very good dog.".into();                   // arbitrary spec edits

let mut stage = stage.lower()?;                  // partitioned + lowered
stage.partition_mut().unwrap().by_schema         // move a type between
    .insert("Dog".into(), "create_pet".into());  // modules
stage.settings_mut().with_schema_in_docs(true);  // any typify knob

let stage = stage.build_types()?;                // typify has run
let names = stage.type_space().definition_rust_names();

let mut file = stage.into_file()?;               // post-processed syn AST
file.items.push(syn::parse_quote! { pub const GENERATED: bool = true; });

let source = render_file(&file, "specs/petstore.yaml");
```

`GeneratedTypes::tokens()` is the raw-`TokenStream` escape hatch below
`into_file()` (no post-processing), and `render()` collapses the last two
steps. With no between-stage edits the staged path and
`generate_to_string()` produce byte-identical output — the one-shot
method is implemented as exactly this sequence. See
`examples/custom_pipeline.rs` for a runnable end-to-end walkthrough.

## Macro

`typify::import_types!` consumes JSON Schema, not OpenAPI; the `lower`
subcommand bridges the gap. The fork's macro knobs cover the wire-shape
settings:

```rust
typify::import_types!(
    schema = "petstore.schema.json",
    unconstrained_string = true,
    array_optionality = OptionalIfNotRequired,
    allof_strategy = Compose,
    conditional_derives = [ { feature = "schemars", body = schemars::JsonSchema } ],
);
```

## Examples

`petstore` and `sabre_booking` compile checked-in generated output and
assert wire behavior (serialization shape, patch merging, defaults);
`petstore_tree` and `sabre_booking_tree` do the same against the
checked-in folder-tree output under `examples/generated_tree/`;
`custom_pipeline` drives the staged API and asserts on the rendered
source:

```bash
cargo run --example petstore           # small spec exercising every knob
cargo run --example sabre_booking      # 257-schema real-world spec
cargo run --example petstore_tree      # folder tree + request/response split
cargo run --example sabre_booking_tree # …at real-world scale
cargo run --example custom_pipeline    # step-by-step pipeline customization
```

Regeneration commands are in each example's header comment. The Sabre
Booking spec (`specs/sabre-booking/`) is the real-world fixture: 9
operations, 257 schemas, Swagger-conversion `allOf` patterns, plus an RFC
6902 patch documenting a spec/reality discrepancy.

## Style as data (IR engine)

On the IR engine the style profile is data — `StyleConfig::api_client()`
is the exact declarative form of the `api-client` knob recipe — and a
`codegen.toml` can override any of it, plus target individual types and
fields:

```toml
profile = "api-client"

[style]
rename-all = "camelCase"
deep-patch = "all-option-structs"

[types."Agency"]
derives-add = ["Eq"]
module = "shared/common"

[fields."CancelBookingRequest.notification"]
deep-patch = true

[fields."Pet.id"]
type = "::my_crate::PetId"
```

```bash
cargo run -- generate --spec spec.yaml --profile api-client --engine ir \
    --config codegen.toml --split-request-response --output-dir src/generated
```

Unmatched `[types]`/`[fields]` selectors are hard errors. From the
library, `Generator::style(|s| ...)` is the code-level hook and
`Generator::ir_pass(...)` appends custom IR passes after the built-in
pipeline. Every config key is consumed by exactly one named pass
(`docs/MIGRATION.md` maps them).

## Relationship to the typify fork

On the default engine, everything style-related lives in the fork behind
`TypeSpaceSettings` knobs; this crate sequences the pipeline and picks
knob values. See
[`../typify/FORK_FEATURES.md`](../typify/FORK_FEATURES.md) for the full
feature-by-feature mapping to settings, macro keys, and CLI flags. The
fork is frozen (tag `fork-freeze-20260703`); the IR engine exists to
retire it, and `tests/parity.rs` holds the two engines identical on the
fixtures until the default flips.

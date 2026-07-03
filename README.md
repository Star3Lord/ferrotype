# openapi-codegen

Generate ergonomic Rust types from OpenAPI specs. Point it at an OpenAPI
3.x document and it produces API-client-style types — bare `Option<T>`
fields, `camelCase` serde renames, `#[serde(flatten)]` inheritance,
`struct_patch` deep patches — partitioned into one module per operation.

The schema-to-Rust core is the local
[typify fork](../typify/FORK_FEATURES.md) (battle-tested upstream
semantics plus opt-in ergonomic knobs, kept rebase-clean against
upstream `main`). Everything typify doesn't own lives here, as data and
verified AST passes: declarative style ([`StyleConfig`], a
`codegen.toml`, built-in profile presets), per-type and per-field
overrides (patch opt-out, deep-patch control, type replacement, module
placement), the condensed emit layout, operation partitioning, and the
folder-tree writer. (An experimental in-house IR engine was built,
audited, and retired in favor of this split — see
[docs/MIGRATION.md](docs/MIGRATION.md), decision D15.)

## Pipeline

```text
load (YAML/JSON)
  → patch (RFC 6902 files + Rust hooks)
  → partition (operation reachability → per-op modules + shared)
  → Spec (typed normalization; keeps discriminator/examples)
  → draft-07 render → typify fork (StyleConfig → TypeSpaceSettings)
  → AST post-passes (per-type/per-field overrides, patch stripping,
     Default synthesis for untagged oneOf, condensed emit style)
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
- **`src/spec/`** — the typed `Spec` model: dialect-tolerant
  normalization (3.0.x + Swagger-2.0-converted), preserving
  `discriminator`/`examples`/operations for future consumers; renders the
  draft-07 document typify consumes.
- **`src/config.rs`** — style as data: [`StyleConfig`], the presets, the
  `codegen.toml` loader, and the mapping onto the fork's
  `TypeSpaceSettings` knobs.
- **`src/overrides.rs`** — per-type / per-field override resolution: the
  deep-patch predicate handed to the fork, patch-machinery stripping,
  field type replacement, and hard-error selector validation.
- **`src/condense.rs`** — the condensed emit style, as a token-verified
  AST transformation (see [below](#readable-output--emit-style)).
- **`src/render.rs`** — the shared rendering passes both output modes
  finish with: doc-comment normalization (stacked `/// ` lines, split,
  spaced, soft-wrapped; schema-in-docs blocks exempt), the condensed
  style's macro polish, and the item-spacing pass (a blank line between
  adjacent items, token-verified).
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
for the cfg-gated `JsonSchema` derives. The `struct_patch` machinery
itself is optional, globally or per type — see
[Style as data](#style-as-data); with patch fully off the generated code
has no `struct-patch` dependency at all.

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

## Style as data

The style profile is data — `StyleConfig::api_client()` is the exact
declarative form of the `api-client` knob recipe, applied onto the
fork's `TypeSpaceSettings` — and a `codegen.toml` can override any of
it, plus target individual types and fields:

```toml
profile = "api-client"

[style]
rename-all = "camelCase"
deep-patch = "all-option-structs"
# struct_patch support is optional: `false` strips the `Patch` derive,
# every `#[patch(...)]` attribute, and all deep-patch annotations
# (api-client default: true).
patch = true

# Map schema `type`+`format` pairs to arbitrary Rust types (the fork's
# `with_format_type`): instance types string/integer/number, any format.
# An entry wins over typify's built-in format handling and over the
# `date` / `date-time` / `uuid` sugar keys for the same format. The
# mapped path is emitted verbatim and must implement
# Serialize/Deserialize for the wire shape.
[style.formats]
"string/date-time" = "::time::OffsetDateTime"
"string/decimal" = "::rust_decimal::Decimal"
"integer/int64" = "::my_crate::BigInt"

[types."Agency"]
derives-add = ["Eq"]
module = "shared/common"

# Map a named schema to an existing Rust type instead of generating a
# struct (upstream typify's `with_replacement`): nothing is emitted for
# the schema and every reference names the path — which, again, must
# implement Serialize/Deserialize. `replace-impls` optionally declares
# traits the type provides ("display", "from-str",
# "from-string-irrefutable", "default"). Cannot be combined with
# `patch`/`derives-add`/`module` (nothing is generated to patch,
# derive on, or place).
[types."Money"]
replace = "::my_crate::Money"
replace-impls = ["display", "from-str"]

# Per-type override of the [style] patch baseline: this type loses its
# `Patch` derive and `NotificationPatch` companion, and every
# `#[patch(name = "Option<NotificationPatch>")]` annotation on fields
# of *other* types referencing it is pruned too. (`patch = true`
# re-enables one type when the global baseline is `false`.)
[types."Notification"]
patch = false

[fields."CancelBookingRequest.notification"]
deep-patch = true

[fields."Pet.id"]
type = "::my_crate::PetId"
```

```bash
cargo run -- generate --spec spec.yaml --profile api-client \
    --config codegen.toml --split-request-response --output-dir src/generated
```

How the granular keys land (the fork's knob surface is global-per-kind
by design, so this crate owns the per-type/per-field decisions): the
`deep-patch` keys become the predicate handed to the fork's
`with_deep_patch_filter`, deciding every `#[patch(name = ...)]`
annotation at the source; `derives-add` rides the fork's per-type
`with_patch` mechanism; `patch = false` types get their `Patch` derive
and `patch(...)` attributes stripped in a post-generation AST pass; and
`type` replacements rewrite the field's AST (its deep-patch annotation
is withheld, since a replaced type has no known Patch companion).

Unmatched `[types]`/`[fields]` selectors are hard errors, as are
contradictions the generated code could not satisfy (`patch = false` on
a non-struct, or a forced `deep-patch = true` whose owner or target
type is not patchable). When no struct keeps patch support, the
`use ::struct_patch::Patch;` preamble import is dropped too, so fully
patch-free output compiles without the `struct-patch` dependency.
A `replace` entry generates no AST item, so its validation is that the
replacement path actually appears in the output — a replace on a schema
nothing references is an error like any other unmatched selector — and
fields holding a replaced type never receive deep-patch annotations
(the replacement has no generated `Patch` companion).

## Readable output / emit style

Regardless of emit style, every rendered file separates adjacent items
with a single blank line — prettyplease alone packs one item flush
against the next type's doc comment. Runs of one-line declarations
(the `use` preamble, a root `mod.rs`'s `pub mod x;` block) stay tight.
The pass is token-verified whitespace-only: the spaced output must
re-parse to the identical token stream or rendering falls back to the
packed form (`src/render.rs`, decision D16 in
[docs/MIGRATION.md](docs/MIGRATION.md)).

Doc comments are normalized the same way for both styles: the raw
`#[doc]` strings typify carries (cramped `///text`, `/** ... */`
blocks for multi-line descriptions, unwrapped spec-length lines)
render as stacked `/// ` lines — one leading doc space with each
line's own indentation preserved, multi-line descriptions split
line-per-line, and long lines soft-wrapped at word boundaries to 92
content characters without re-flowing the spec's own line structure.
The spec's newlines stay *visible* line breaks in rustdoc and IDE
hover: CommonMark collapses a bare newline to a space, so every
original line followed by another gets a trailing-backslash hard
break, while the soft wrap's own line breaks carry no marker and keep
flowing. Doc blocks containing fenced code (the `with_schema_in_docs`
`<details>` sections) pass through untouched (decision D17).

Beyond spacing: by default every string enum is followed by ~50 lines
of mechanical impls (`Display`, `FromStr`, the three `TryFrom` forms,
`Default`), and every partition module carries its own copy of the
`error` module — typify's native shape, which the goldens pin. That
burying of types under boilerplate is what `emit-style` addresses:

```toml
[style]
emit-style = "condensed"   # default: "expanded"
```

Under `condensed`:

- **One `support` module per generation unit** (a `support.rs` file at
  the tree root in `--output-dir` mode) holds the single `error`
  module — one `ConversionError` type instead of an identical copy per
  module — and the `impl_string_enum!` macro, whose definition is
  emitted readably formatted and documents exactly the impls it
  expands to.
- **One invocation per enum** replaces the impl ladder. The variant →
  wire-string mapping is right there, and `default = Variant` shows the
  `Default` selection:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FareRuleRestrictionEnum {
    #[serde(rename = "CHANGEABLE")]
    Changeable,
    #[serde(rename = "REFUNDABLE")]
    Refundable,
    #[serde(rename = "CHANGEABLE_AND_REFUNDABLE")]
    ChangeableAndRefundable,
}

impl_string_enum!(FareRuleRestrictionEnum {
    Changeable => "CHANGEABLE",
    Refundable => "REFUNDABLE",
    ChangeableAndRefundable => "CHANGEABLE_AND_REFUNDABLE",
} default = Changeable);
```

- **Every module that used to duplicate `pub mod error { ... }`**
  re-exports the shared one (`pub use super::…::support::error;`), so
  `<module>::error::ConversionError` paths in consumer code keep
  resolving.

Capabilities are identical, by construction: the condensation is a
token-verified AST transformation over typify's output — a ladder is
only replaced after the macro's expansion for the extracted pairs is
verified token-equal to the impls being removed, and anything
unrecognized is left expanded. The capability-equivalence tests and the
checked-in `examples/generated_tree/petstore_condensed/` golden pin
this, and the type items themselves are token-identical between styles.
On the Sabre tree the condensed style is ~36% fewer lines overall and
~67% fewer in `shared/enums.rs`.

`emit-style` defaults to `expanded` in every preset so the goldens keep
their meaning; flipping to condensed is a consumer choice in
`codegen.toml` (see decision D14 in
[docs/MIGRATION.md](docs/MIGRATION.md)).

From the library, `Generator::style(|s| ...)` is the code-level hook
(isomorphic to the file: `style.patch`, `TypeOverride::patch`, and
friends are plain fields), and `Generator::customize(|settings| ...)`
reaches the raw fork knobs underneath the data layer.

## Relationship to the typify fork

The schema-to-Rust semantics live in the fork behind opt-in
`TypeSpaceSettings` knobs; this crate sequences the pipeline, maps
[`StyleConfig`] data onto those knobs, and owns every decision typify
structurally can't host (per-type/per-field overrides, condensed
emission, partitioning, trees). See
[`../typify/FORK_FEATURES.md`](../typify/FORK_FEATURES.md) for the full
feature-by-feature mapping to settings, macro keys, and CLI flags. The
fork's defaults match upstream byte-for-byte — its upstream test goldens
are unchanged — so rebasing it onto upstream `main` stays cheap; every
deviation is an explicit knob this crate turns.

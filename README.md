# Ferrotype

Generate ergonomic Rust types from OpenAPI specs. Point it at an OpenAPI
3.x document and it produces API-client-style types — bare `Option<T>`
fields, `camelCase` serde renames, `#[serde(flatten)]` inheritance,
`struct_patch` deep patches — partitioned into one module per operation.

The schema-to-Rust core is the
[typify fork](https://github.com/Star3Lord/typify) (battle-tested
upstream semantics plus a small set of opt-in wire-shape mechanisms —
subset conversions, explicit optionality, `allOf` composition, open
enums, subset emission — kept rebase-clean against upstream `main`; see
its `FORK_FEATURES.md`). Everything typify doesn't own lives here, as
data and verified AST passes: declarative style ([`StyleConfig`], a
`codegen.toml`, built-in profile presets), the decoration pass (derive
lists, attribute stacks, `rename_all`, patch machinery, enum defaults,
newtype conveniences), per-type and per-field overrides (patch opt-out,
deep-patch control, type replacement, module placement), the condensed
emit layout, operation partitioning, and the folder-tree writer. (An
experimental in-house IR engine was built, audited, and retired in
favor of this split — see [docs/MIGRATION.md](docs/MIGRATION.md),
decision D15; the fork's knob surface was later condensed into the six
wire-shape mechanisms and this crate's decoration pass — decision D24.)

## Pipeline

```text
load (YAML/JSON)
  → patch (RFC 6902 files + Rust hooks)
  → partition (operation reachability → per-op modules + shared)
  → Spec (typed normalization; keeps discriminator/examples)
  → draft-07 render → typify fork (StyleConfig → TypeSpaceSettings)
  → AST post-passes (decoration: derives/attrs/rename_all/patch
     machinery/enum defaults; per-type/per-field overrides, patch
     stripping, Default synthesis for untagged oneOf, condensed emit
     style)
  → format (prettyplease) → write (idempotent)
```

- `src/load.rs` — spec parsing and patch application. Patch files are
`{ description: <non-empty>, ops: [<RFC 6902 op>...] }`; `op: test`
entries let a patch assert preconditions so future spec revisions fail
loudly instead of silently drifting.
- `src/partition.rs` — walks every operation's request/response `$ref`
closure; schemas reachable from exactly one operation land in
`pub mod <snake_operation_id>`, everything else in `pub mod shared`.
The opt-in split mode walks each role's closure separately and
produces nested `<op>/{request,response}` +
`shared/{request,response,enums,common}` module paths (see
[below](#requestresponse-splitting-and-folder-output)).
- `src/spec/` — the typed `Spec` model: dialect-tolerant
normalization (3.0.x + Swagger-2.0-converted), preserving
`discriminator`/`examples`/operations for future consumers; renders the
draft-07 document typify consumes (hoisting self-colliding nullable
inners into `{name}Inner` definitions on the way).
- `src/config.rs` — style as data: [`StyleConfig`], the presets, the
`codegen.toml` loader, and the mapping onto the fork's
`TypeSpaceSettings` mechanisms (subset conversions, optionality
policy, `allOf` strategy, open enums, docs).
- `src/decorate.rs` — the decoration pass, first of the AST passes:
derive lists, attribute stacks, `rename_all` + covered-rename elision,
option serde-noise elision, deep-patch annotations and patch-companion
naming mirrors, enum first-unit-variant `Default` impls, and
string-newtype conveniences — everything the fork's retired style
knobs used to emit.
- `src/modules.rs` — partitioned emission: assembles the nested module
tree from `TypeSpace::to_stream_for` subsets (each with its own
`error` module and the `defaults` fns its types need).
- `src/idents.rs` — the Rust identifier forms of schema names
(`rust_type_ident` / `rust_field_ident`, this crate's port of typify's
sanitization) for config-selector resolution.
- `src/overrides.rs` — per-type / per-field override resolution: the
deep-patch predicate the decoration pass consults, patch-machinery
stripping, field type replacement, and hard-error selector validation.
- `src/condense.rs` — the condensed emit style, as a token-verified
AST transformation (see [below](#readable-output--emit-style)).
- `src/render.rs` — the shared rendering passes both output modes
finish with: doc-comment normalization (stacked `///`  lines, split,
spaced, soft-wrapped; schema-in-docs blocks exempt), the condensed
style's macro polish, and the item-spacing pass (a blank line between
adjacent items, token-verified).
- `src/postprocess.rs` — synthesizes `impl Default` for enums typify
can't default (untagged `oneOf` with no unit variant).
- `src/tree.rs` — the folder-tree writer: splits the generated module
tree into one file per partition module.



## Style profiles


| Profile      | Output                                  |
| ------------ | --------------------------------------- |
| `typify`     | Upstream typify output, unchanged.      |
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

`--split-request-response` (library:
`Generator::split_request_response(true)`, implies
`--partition-by-operation`) classifies every operation's `$ref` entry
points by role — `requestBody` and parameter schemas are *request*,
`responses` are *response* — and BFS-walks each role's closure
separately. Each schema's `(operation, role)` usages across the whole
spec decide its module:


| Usage                                      | Module                             |
| ------------------------------------------ | ---------------------------------- |
| exactly one `(op, role)`                   | `<op>::request` / `<op>::response` |
| several ops, request-only                  | `shared::request`                  |
| several ops, response-only                 | `shared::response`                 |
| shared + simple enum (all unit variants)   | `shared::enums`                    |
| both roles / orphans / inline-schema types | `shared::common`                   |


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

`--output-dir <DIR>` (library: `Generator::generate_to_dir`, staged
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

## Generated API client

Opt-in: `--client` on the CLI, `Generator::client(true)` in the
library, or a `[client]` table in codegen.toml. Off by default — with
it off, every output is byte-identical to before the feature existed.
With it on, the output grows a `client` module next to the types (in
tree output: `client/{mod,auth,support}.rs`, plus a user-owned `ext/`):

```rust
use bindings::client::{Client, auth::OAuth2ClientCredentials};

let client = Client::builder()          // base URL defaults to servers[0]
    .auth(OAuth2ClientCredentials::new(client_id, client_secret))
    .body_hook(bindings::ext::pcc::fill_target_pcc("G7HE"))
    .build();

let response = client.cancel_booking(&request).await?;
```

**The client is concrete** — no trait layer, no generated mocks. It
holds the base URL, a `reqwest_middleware::ClientWithMiddleware` (pass
a middleware stack for retries/tracing, or a plain `reqwest::Client`,
accepted via `From`), an `Arc<dyn AuthProvider>`, and the registered
body hooks. One `pub async fn <operation_id>` per operation: path
parameters are percent-encoded, `Option` query parameters are skipped
when `None`, scalar parameter types follow the resolved style's format
mappings (the api-client preset's uuid/date-time → `String` become
`&str` parameters; the plain profile keeps `&uuid::Uuid`).

**Request path.** The body serializes to `serde_json::Value`, every
`ClientBuilder::body_hook(|op_info, &mut value| …)` runs in
registration order, then the value is sent. Hooks are the typed seam
for cross-cutting request edits — a fill-if-unset tenant/PCC field is a
three-line closure in `ext/` instead of a trait implemented over every
request type.

**Response path.** Non-2xx → `Error::Status { op, status, body }` with
the raw body. 2xx bodies are read as **text first** and decoded via
`serde_json::from_str`, so a decode failure is
`Error::Decode { op, source, body }` carrying serde's line/column
diagnostics *and* the raw payload — not reqwest's opaque "error
decoding response body". Errors are hand-rolled `Display` +
`std::error::Error` impls; no thiserror in generated code.

**Auth from** `securitySchemes`**.** The `client::auth` module holds
`trait AuthProvider` (`async fn authorize(request, &OperationInfo)`,
`#[async_trait]`, dyn-usable) plus providers derived from the spec:
`NoAuth` (the builder default) and `StaticBearer` always; `BasicAuth`
and `ApiKey` (header/query) when declared; and for oauth2
`clientCredentials` schemes, `OAuth2ClientCredentials` — a
client-credentials token fetch with a TTL cache (`std::sync::Mutex` +
`Instant`, no tokio dependency; the guard is never held across an
await), the spec's `tokenUrl` baked in as an overridable default, and
`x-base64-encode-client-credentials: true` honored by base64-encoding
id and secret individually before the standard basic-auth encoding.
Spec'd auth header parameters (an explicit `Authorization` header
parameter, or a header named by an `apiKey` scheme) are folded out of
method signatures — the provider owns those headers
(`suppress-auth-headers = false` keeps them).

Three escalation levels to change auth behavior, no codegen fork
needed: configure the generated provider (token URL, encoding, HTTP
client) → pass your own `impl AuthProvider` from `ext/` to
`ClientBuilder::auth` → eject `client/auth.rs` and own it.

**Config keys** (`[client]` table; kebab-case, unknown keys are hard
errors):


| Key                     | Default | Meaning                                                |
| ----------------------- | ------- | ------------------------------------------------------ |
| `enabled`               | `false` | generate the `client` module                           |
| `suppress-auth-headers` | `true`  | fold spec'd auth header params out of signatures       |
| `ext-module`            | `true`  | scaffold + declare the user-owned `ext/` (tree output) |


**The** `ext/` **module** (directory-tree output only) is the user-owned
home for code that belongs next to the generated output: impls on
generated types, helper types, hook functions. `ext/mod.rs` is
scaffolded once *without* the `// @generated` marker — born ejected —
and `pub mod ext;` is always declared from the generated root. Nothing
under `ext/` is ever overwritten or deleted; grow it into
`ext/pcc.rs`, `ext/hooks.rs`, … freely.

**Ejection** generalizes that ownership story to any generated file:

```bash
openapi-codegen eject src/generated/sabre_booking/client/auth.rs
```

verifies the `// @generated` marker and rewrites the header to
`// @ejected — was generated from <spec>; delete this file and regenerate to restore.` Regeneration *skips* files without the marker
(with a stderr note) and never deletes them; un-eject by deleting the
file and regenerating. Single-file output supports the client too
(`pub mod client { … }` is appended), but ejection and `ext/` need the
directory tree — there is no file to own inside one document.

**v1 boundaries** (loud errors carrying the schema's origin, per
project policy — refuse rather than guess; the patch mechanism is the
escape hatch): JSON bodies only; request/response schemas must be
`$ref`s to named schemas (inline schemas error with a "patch it into
components.schemas" hint); one success schema per operation; scalar
inline parameters only (no `$ref` parameters); http bearer/basic,
apiKey header/query, and oauth2 clientCredentials schemes (pass a
custom provider for anything else).

Generated-code dependencies stay minimal — reqwest,
reqwest-middleware, serde, serde_json, async-trait, and base64 only
when an OAuth2 provider is emitted:

```toml
async-trait = "0.1"
base64 = "0.22"                # only with an OAuth2 provider
reqwest = { version = "0.13", features = ["json", "form", "query"] }
reqwest-middleware = { version = "0.5", features = ["json", "query"] }
```

The `--verify` gate auto-declares each of these in its scratch crate
when the rendered output references it. The examples workspace's
`via-cli-client` crate is the living end-to-end story: checked-in
`--client` output, an `ext/` PCC hook, and wiremock round-trips pinning
the OAuth2 token cache (one fetch for two calls), the hook landing on
the wire, and both error surfaces.

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

# Same, plus the generated API client (client/ + ext/ in the tree)
cargo run -- generate \
    --spec specs/sabre-booking/spec.openapi.yaml \
    --patches-dir specs/sabre-booking/patches \
    --profile api-client \
    --split-request-response \
    --client \
    --output-dir examples/generated_tree/sabre_booking_client

# Lower only: spec → JSON Schema, for typify::import_types! / cargo-typify
cargo run -- lower --spec specs/petstore.yaml --output petstore.schema.json

# Take ownership of one generated file (regeneration then skips it)
cargo run -- eject examples/generated_tree/sabre_booking_client/client/auth.rs
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
let names: Vec<(String, String)> = stage
    .type_space()
    .iter_definitions()
    .map(|(key, ty)| (key.to_string(), ty.name()))
    .collect();

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
subcommand bridges the gap. (The fork itself also detects OpenAPI
documents directly, by their top-level `openapi` member.) The fork's
macro knobs cover the wire-shape settings:

```rust
typify::import_types!(
    schema = "petstore.schema.json",
    optional_properties = Explicit,
    all_of_strategy = Compose,
    open_enum_variant = "Other",
    schema_in_docs = false,
    convert = {
        { type = "string", format = "date-time" } = ::std::string::String,
    },
);
```

Everything decoration-flavored (derives, attrs, `rename_all`, patch
machinery) is this crate's post-processing and has no macro
counterpart.



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
# Open string enums (the fork's `with_open_enum_variant`): every plain
# string enum gains a trailing `#[serde(untagged)] Other(String)`
# catch-all, so wire values outside the documented set round-trip
# losslessly instead of failing to deserialize. The value names the
# catch-all variant; enums already declaring it stay closed; opened
# enums drop `Copy`. Use when the spec's enums lag the live wire.
open-enums = "Other"

# Map schema `type`+`format` pairs to arbitrary Rust types (a subset
# conversion on the fork's `with_conversion`): instance types
# string/integer/number, any format.
# An entry wins over typify's built-in format handling and over the
# `date` / `date-time` / `uuid` sugar keys for the same format. The
# mapped path is emitted verbatim and must implement
# Serialize/Deserialize for the wire shape.
[style.formats]
"string/decimal" = "::rust_decimal::Decimal"
"integer/int64" = "::my_crate::BigInt"

# The table form additionally attaches per-field attributes (applied at
# the AST level to every field of the mapped type — `field-attrs` on
# required fields, `optional-field-attrs` on Option-wrapped ones; the
# two never substitute for each other, since a `serde(with = ...)`
# module for T cannot handle Option<T>; Vec<T> fields are out of
# scope) and declares the type's capabilities. `impls` drives
# capability-aware derives: serde plus Debug/Clone are always assumed,
# everything else defaults to NOT provided, and codegen prunes a
# `Default`/`PartialEq`/`Eq`/`Hash`/`Ord` derive from any generated
# struct a mapped type cannot satisfy — transitively (a struct whose
# required field lost Default loses it too), with a stderr warning per
# removal. Option/Vec-wrapped fields don't constrain Default (they
# default to empty) but do constrain the equality family; patch
# companions keep Default (their fields are all Option) and share the
# equality-family pruning; deep-patch annotations on fields of a type
# that lost Default are dropped (struct_patch's none-as-default merge
# needs Default) with the same warning treatment.
[style.formats."string/date-time"]
type = "::time::OffsetDateTime"
field-attrs = ["serde(with = \"time::serde::iso8601\")"]
optional-field-attrs = ["serde(default, with = \"time::serde::iso8601::option\")"]
impls = ["serialize", "deserialize", "partial-eq", "eq", "hash", "ord"]  # no `default`

[types."Agency"]
derives-add = ["Eq"]
module = "shared/common"

# Map a named schema to an existing Rust type instead of generating a
# struct (upstream typify's `with_replacement`): nothing is emitted for
# the schema and every reference names the path — which, again, must
# implement Serialize/Deserialize. `replace-impls` declares the type's
# capabilities (same vocabulary and pruning semantics as the formats
# `impls` list); `field-attrs`/`optional-field-attrs` attach per-field
# attributes exactly like a formats table entry. Cannot be combined
# with `patch`/`derives-add`/`module` (nothing is generated to patch,
# derive on, or place).
[types."Money"]
replace = "::my_crate::Money"
replace-impls = ["display", "from-str", "default", "partial-eq"]

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

Field-scoped decisions layer across three tiers, most specific wins:
`[style.formats]`/`replace` mapping defaults, then ordered `[[rules]]`
(later rules override earlier ones key-by-key), then
`[fields."Type.field"]` — which beats every rule:

```toml
# Rules match fields by ANDed glob predicates (`*` and `?`): `module`
# (partition path — requires partitioned output), `struct` (schema key
# or Rust name), `field` (wire or Rust name), `format` (the property's
# spec-level "type/format" provenance; named schemas only, one $ref
# hop), and `type` (the resolved Rust type, Option/Box-unwrapped).
# The payload is the [fields] vocabulary. A rule matching nothing
# warns instead of erroring — globs are broad-brush, unlike exact
# selectors. `deep-patch` payloads feed generation, so they cannot be
# combined with the post-generation `type` predicate.
[[rules]]
match = { module = "*/request", format = "string/date-time", struct = "Create*", field = "*_date_time" }
apply = { field-attrs = ["serde(with = \"time::serde::iso8601\")"], deep-patch = false }

# `optional = true` drops the matching properties from their schema's
# `required` list before generation (they come out `Option<T>`), for
# specs that overstate required-ness relative to the live wire. Like
# `deep-patch` it rewrites generation input, so it cannot use the
# post-generation `type` predicate; a later `optional = false` restates
# the spec for a narrower match (it never ADDS required-ness).
[[rules]]
match = { module = "*/request" }
apply = { optional = true }

# `patch` is a TYPE-level payload: it strips (or, over a `patch = false`
# baseline, restores) the whole struct_patch surface — derive,
# companion, patch attrs, annotations referencing the type. Such a rule
# matches types, so only `module` and `struct` predicates are allowed,
# and it cannot be mixed with field-level payload keys in one rule.
# Precedence: style `patch` baseline → rules in order → exact
# `[types."X"] patch` beats all rules. Response envelopes are the
# canonical use: they are read-only, so patching them is meaningless.
[[rules]]
match = { module = "*/response" }
apply = { patch = false }

# [fields] grows the same vocabulary: `field-attrs` replaces (never
# merges with) mapping/rule attrs for exactly that field — an empty
# list clears them, an absent key inherits; the field's optionality is
# known here, so there is one list. `type` accepts the table form,
# whose `impls` joins the capability-pruning fixpoint (a required
# field overridden to a non-`default` type strips `Default` from its
# owner, transitively).
[fields."CreateBookingRequest.purchaseDateTime"]
field-attrs = ["serde(default, with = \"time::serde::iso8601::option\")"]

[fields."Order.total"]
type = { type = "::my_crate::Money", field-attrs = ["serde(with = \"my_crate::money_serde\")"], impls = ["serialize", "deserialize", "default"] }
```

```bash
cargo run -- generate --spec spec.yaml --profile api-client \
    --config codegen.toml --split-request-response --output-dir src/generated
```

How the granular keys land (the fork owns only wire-shape decisions,
so this crate owns everything per-type/per-field): the `deep-patch`
keys become the predicate the decoration pass consults per field,
deciding every `#[patch(name = ...)]` annotation; `derives-add` joins
the decoration pass's derive-list rewrite (riding the fork's per-type
`with_patch` mechanism when a kind's derive list is left native);
`patch = false` types get their `Patch` derive and `patch(...)`
attributes stripped in a post-generation AST pass; and `type`
replacements rewrite the field's AST (its deep-patch annotation is
withheld, since a replaced type has no known Patch companion).

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

Generation can be compile-gated: `--verify` (CLI),
`Generator::verify_compile(true)`, or `[verify] enabled = true` runs
`cargo check` over the output in a scratch crate — edition 2024, the
generated source mounted as the lib, dependencies defaulting to serde /
serde_with / struct-patch plus raw lines from
`[verify] dependencies = ['time = { version = "0.3", ... }']` (a user
line for a default crate replaces the default). Well-known crates the
output may reference are auto-declared when (and only when) the
rendered source mentions them: chrono and uuid (typify's format
defaults, with serde features), serde_json (free-form schemas), and
regress (validating string newtypes). The scratch crate also declares
an empty `schemars` feature so the cfg-gated derives don't trip
`unexpected_cfgs`. The gate runs before any file is written, fails
generation with the captured rustc output (keeping the scratch crate
for inspection) — unresolved-crate failures additionally get a
targeted hint naming the missing crate(s) and the
`[verify] dependencies` syntax — and needs the declared dependencies
resolvable by the user's cargo. Single-file and folder-tree outputs
are both covered. Types mapped via `[style.formats]`/`replace` still
need their crates declared explicitly (the gate can't guess versions
or features for arbitrary crates).

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
render as stacked `///`  lines — one leading doc space with each
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

- **One** `support` **module per generation unit** (a `support.rs` file at
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

- **Every module that used to duplicate** `pub mod error { ... }`
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

The *wire-shape* semantics live in the fork behind a small set of
opt-in `TypeSpaceSettings` mechanisms — subset-matching conversions
(`with_conversion`), the optional-properties policy
(`with_optional_properties`), `allOf` composition
(`with_all_of_strategy`), open string enums (`with_open_enum_variant`),
docs control (`with_schema_in_docs`), and subset emission
(`to_stream_for` / `iter_definitions`) — plus first-class OpenAPI
document ingestion. This crate sequences the pipeline, maps
[`StyleConfig`] data onto those mechanisms, and owns every decision
typify structurally can't host: all decoration concerns (the derive
lists, attribute stacks, `rename_all` + rename elision, option
serde-noise elision, `struct_patch` machinery, enum first-variant
defaults, string-newtype conveniences — `src/decorate.rs`), per-type /
per-field overrides, condensed emission, partitioning, and trees. See
the fork's `FORK_FEATURES.md` for the mechanism-by-mechanism mapping to
settings, macro keys, and CLI flags. The fork's defaults match upstream
byte-for-byte — its upstream test goldens are unchanged — so rebasing
it onto upstream `main` stays cheap; every deviation is an explicit
mechanism this crate drives.
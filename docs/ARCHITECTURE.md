# Architecture review: OpenAPI → Rust codegen stack

**Status:** proposal (design review, no code changed)
**Date:** 2026-07-03
**Scope:** the `typify` fork (`ergonomic-codegen` branch), `openapi-codegen`,
`openapi-codegen-examples`, with `sabre-types` as the style reference and the
six prior experiments in `miso/rust-api-2/crates/libs/` as the pain-point
record.

**TL;DR.** The current stack works and encodes real, hard-won knowledge, but
it is built on a strategically losing position: ~20 style knobs threaded
through the internals of a forked third-party schema compiler whose upstream
has announced a ground-up rewrite of exactly those internals. The
recommendation is to invert the dependency: own a small OpenAPI→IR→Rust
pipeline (types **and** operations in one IR, style as ordered passes over it,
emission as a backend), keep the genuinely good parts of today's design
(patch-first spec workflow, role partitioning policy, idempotent tree writer,
compile-the-output tests), and retire the typify fork behind a differential
test harness instead of syncing it forever. This is candidate (b) below, with
(c)'s policy seam as the customization API and (d)'s pass registry as the
extension mechanism.

---

## 1. Requirements recap

Judging everything against these, not against what the current code happens
to do:

1. **Any spec in, deliberately-styled Rust out.** Swagger-2.0-converted specs
   (the Sabre reality, with their `allOf` single-inheritance patterns),
   OpenAPI 3.0.x today, 3.1 (JSON Schema 2020-12 dialect) eventually.
2. **Client generation later, but architecturally anticipated now.**
   Operations, path/query/header parameters, auth, request builders —
   progenitor-style. The design must not stop at `components.schemas`.
3. **More granular control than progenitor / openapi-generator / oapi-codegen
   / oas3-gen:** per-spec patches, per-type and per-field overrides, style
   profiles, module partitioning, custom derives/attrs, custom passes.
4. **Three consumption surfaces:** library (`build.rs`), CLI, macro — with
   feature parity, not three dialects.
5. **Maintainable by one person long-term.** Fork-sync cost must be near zero
   or the fork must go away.

A useful framing for everything below: this project is not "a typify
configuration layer." It is **an SDK generator for Rust with a strong opinion
about style**, currently implemented by coercing a JSON-Schema-to-Rust
compiler into producing someone else's house style. The requirements (ops,
granularity, 3.1, one-person maintenance) are SDK-generator requirements.

---

## 2. Current architecture assessment

### 2.1 Inventory

**The typify fork** (4 commits over upstream `main`, ~2,000 net lines in
`typify-impl/src`, +1,700 lines of pinning tests):

- ~20 opt-in knobs on `TypeSpaceSettings`, implemented as `if settings.x`
  branches through typify's hottest files: `convert.rs` (+314), `structs.rs`
  (+157), `type_entry.rs` (+593), `lib.rs` (+1,040), plus touches to
  `enums.rs`, `defaults.rs`, `value.rs`, `util.rs`.
- Knob taxonomy (full mapping in the appendix):
  - *Type mapping:* `with_date_type` / `with_date_time_type` /
    `with_uuid_type` (string-typed Rust paths), `unconstrained_string`,
    `unconstrained_int`.
  - *Optionality policy:* `array_optionality`, `default_bool_optionality`,
    `defaulted_field_optionality` — three separate enums answering one
    question ("when is a non-required field `Option<T>`?").
  - *Serde surface:* `elide_option_field_defaults`, `struct_rename_all` with
    per-field rename elision, `serde_field_case` (dual wire/snake surfaces).
  - *Composition:* `allof_strategy` (`Merge` | `Compose` via
    `#[serde(flatten)]`).
  - *Attribute/derive engine:* conditional (cfg-gated) and unconditional
    derives and attrs, each with `TypeKindFilter` (structs/enums/newtypes)
    and `AttrPosition` (before/after derive), ordered-derive-list semantics
    that suppress the historical base set. A miniature attribute layout
    engine, configured through strings.
  - *Impl synthesis:* `enum_first_variant_default`, always-on string-newtype
    conveniences, `deep_patches` policy + `deep_patch_filter` closure
    (library-only).
  - *Docs:* `with_schema_in_docs` (one deliberate default change vs
    upstream).
  - *Output shape:* `to_stream_partitioned` (nested slash-path modules,
    per-module import preambles, `error`/`defaults` duplication into leaves),
    `definition_rust_names` (schema-key → Rust-name bridge).
- Every knob costs four surfaces: settings method, macro key
  (`typify-macro`, +149 lines), CLI flag (`cargo-typify`, +336 lines), and
  FORK_FEATURES.md documentation — plus its `if` branches and its pinning
  tests.

**The driver crate** (`openapi-codegen`, 2,158 lines):

```text
load (serde_json::Value; YAML/JSON)
  → patch (RFC 6902 files w/ required description + test ops; Rust hooks)
  → partition (operation-reachability BFS on the raw Value tree;
               optional request/response role split with boundary rules)
  → lower (hand-rolled OpenAPI 3.0 → JSON Schema draft-07 rewrites in place)
  → typify (fork knobs via StyleProfile preset + customize hooks)
  → post-process (syn pass: impl Default for untagged oneOf enums)
  → format (prettyplease) → write (single file or folder tree, idempotent)
```

- `load.rs` (90 lines) — spec parsing + patches. Patches carry mandatory
  descriptions and support `op: test` preconditions so spec drift fails
  loudly.
- `lower.rs` (145) — `$ref` rewrite (`#/components/schemas/` →
  `#/definitions/`), `nullable: true` → `anyOf [.., null]`, `format`-implies-
  `type` inference, boolean → numeric `exclusiveMinimum`/`Maximum`, and
  stripping of `example`/`examples`/`xml`/`externalDocs`/**`discriminator`**.
- `partition.rs` (677, the largest file) — per-operation `$ref` closures; in
  split mode, role classification (`requestBody`+parameters vs `responses`),
  cross-role boundary rule (a role's walk stops at entry points of the
  opposite role, recording the edge for a bridging glob import), shared
  buckets (`shared/{request,response,enums,common}`).
- `profile.rs` (165) — `Typify` | `ApiClient` presets: ~40 knob calls, plus
  `trait_imports()` (per-module `use` preamble tokens), plus
  `synthesize_enum_defaults()` (post-process opt-in).
- `postprocess.rs` (117) — the syn escape hatch: synthesizes `impl Default`
  for enums typify can't default (untagged `oneOf` with no unit variant),
  needed because ApiClient structs derive `Default`.
- `pipeline.rs` (313) — staged checkpoints `LoadedSpec → LoweredSchema →
  GeneratedTypes → syn::File`, each owning its data, byte-identical to the
  one-shot path when not mutated.
- `tree.rs` (212) — module-tree-to-directory writer: `// @generated`
  headers, idempotent writes, stale-file cleanup gated on the header.
- Tests: text-pattern assertions on generated output (`tests/pipeline.rs`)
  plus examples that **compile the checked-in generated code** for petstore
  and the 257-schema Sabre Booking spec and assert wire behavior.

**Consumers** (`openapi-codegen-examples`): `via-build-script` (Generator),
`via-cli` (regen script + checked-in output), `via-macro`
(`typify::import_types!` on a pre-lowered JSON Schema — wire-shape knobs
only).

**Style reference** (`sabre-types`): hand-written crate. Bare `Option<T>`,
`#[serde(rename_all = "camelCase")]`, `#[serde_with::skip_serializing_none]`,
exact ordered derive list, `struct_patch` deep patches
(`#[patch(name = "Option<InnerPatch>")]`), one folder per operation with
`request`/`response` split and `shared/{enums,request,response}`,
`crate::`-anchored imports.

**Prior experiments** (the graveyard, and what each proved):

| Experiment | Approach | Pain that killed it |
|---|---|---|
| `openapi-rust`, `-2` | 1,400-line `build.rs` driving the typify fork inline | Everything in one build script: pipeline, partitioning, sanitization passes; unshareable, unreviewable. Extracted into `openapi-codegen`. |
| `sabre-oas3-gen` | external `oas3-gen` CLI | Binary-only crate, no types-only mode, needed spec sanitization for case-colliding property names (`useCSL` vs `useCsl`), no style control. |
| `sabre-openapi-generator` | upstream typify + syn post-pass bolting on `bon::Builder` | Origin of "AST post-processing as customization" — works, but the pass matches types by identifier string with no schema context. |
| `sabre-openapi-to-rust` | `openapi_to_rust` crate | Someone else's opinions; no per-field control. |
| `sabre-progenitor` | progenitor | 3.1 unsupported (needed a Node down-converter), spec bugs needed patching before generation, and the output style ceiling is typify's stock settings — no path to the sabre-types shape. |

The consistent failure mode across all five: **granular style control and
real-world-spec repair are the actual requirements, and every off-the-shelf
tool treats both as afterthoughts.** That is the requirement the fork
answered — at the cost analyzed next.

### 2.2 What is genuinely good (keep these)

1. **The patch-first spec workflow.** RFC 6902 files with mandatory
   descriptions and `test` preconditions is exactly what Speakeasy (OpenAPI
   Overlays), oapi-codegen (`overlay:` config), and Stainless converge on.
   The `test`-op discipline is stricter than the industry norm. Keep
   verbatim; optionally accept Overlay-spec files later.
2. **The partitioning policy.** Operation-reachability closure, role
   classification, the cross-role boundary rule (responses echoing requests
   must not drag the request tree into `shared`), shared-enum routing. This
   is real domain insight that no comparable tool has. The *policy* is right;
   only its *implementation home* (raw-JSON BFS pre-typify + a post-typify
   fix-up phase) is wrong.
3. **The tree writer.** `@generated` headers, idempotent writes, marker-gated
   stale cleanup. Keep as-is.
4. **Compile-the-output tests.** The examples that build the checked-in
   petstore and Sabre trees and round-trip JSON are the highest-value tests
   in the stack. Any migration must keep them green at every step.
5. **The staged-pipeline concept.** Checkpoints with owned data and
   mutation points between stages is the right consumption shape. The
   problem is only that the artifacts are *foreign* types.
6. **The fork's feature specifications.** FORK_FEATURES.md is, in effect, a
   requirements document for "what ergonomic output means": derive ordering,
   attr positions, rename elision correctness (wire-name authority), the
   deep-patch type-check exclusions, flatten-vs-deny_unknown_fields
   interactions. That knowledge survives any re-architecture; it is the most
   valuable artifact the fork produced.

### 2.3 Where complexity concentrates, and which of it is accidental

**(1) The fork patches typify's hottest internals — and upstream is about to
rewrite them.** The knobs live as `if` branches inside `convert.rs`,
`structs.rs`, `type_entry.rs`, `lib.rs` — the exact files where upstream
activity lands. Worse, upstream has stated (issues #886 "Plan to upgrade to
schemars 1.0", #737, #579 "The Big Plan") that because schemars 1.0 deleted
the parse-model types typify is built on, **typify will roll its own internal
representation**. When that lands, every fork hunk sits on rewritten ground;
the rebase is a reimplementation, not a merge. Fork-sync cost today is
tolerable (4 commits over a current base); its expected future cost is
approximately "do the whole fork again."

**(2) The knobs are point-fixes where a policy model belongs.** Three
separate optionality enums, a boolean for string constraints, another for
integers, a rename-elision flag, an attribute position enum, a kind filter —
each is one hard-coded answer to a per-site question that a single policy
interface could answer generically: *"given this field (schema, required,
default, shape), what is its Rust type and serde attributes?"* and *"given
this type (kind, name, origin), what are its derives and attributes, in what
order?"* The knob-per-decision approach means every new style need is another
threading exercise across three files and three consumption surfaces. The
accretion is structural, not incidental.

**(3) Everything is stringly typed.** `"::std::string::String"`,
`"patch(attribute(serde(default, rename_all = \"camelCase\")))"` — invalid
values surface as rustc errors on generated output, not at configuration
time. Tolerable at the current scale; compounding as override surfaces grow
(per-field selectors as strings are next on that road).

**(4) Two-phase partitioning.** The partition is computed on the raw
`Value` tree (schema names) before typify runs, then *finished* after typify
runs (`to_rust_partition`: Rust names via `definition_rust_names`, shared-
enum routing via `iter_types`), because "did this schema become a simple
enum?" is only answerable post-generation. The role model lives in
`partition.rs`; the enum-ness answer lives inside typify; the bridge is a
reach-back API added by the fork. In an IR world this is one pass that sees
both facts at once.

**(5) Lowering is string surgery, and it discards information the future
needs.** `lower.rs` works for 3.0 basics but: it **drops `discriminator`**
(exactly the field a client generator and good oneOf mapping want), strips
`example`s (useful for doc/test synthesis later), and handles only
`components.schemas` (no `components.parameters/responses/requestBodies`, no
operation-level anything). 3.1 support on this path means more `Value`
surgery *and* is capped by typify's schemars-0.8 draft-07 parser regardless.

**(6) Profile semantics are smeared across three mechanisms in two repos.**
The ApiClient look = fork knob preset (`profile.rs`) + per-module import
preamble (`trait_imports()`) + a post-process flag
(`synthesize_enum_defaults`). Changing one style decision can touch fork +
driver + examples. Profiles should be one artifact.

**(7) The macro surface is a second dialect.** `import_types!` exposes only
the wire-shape knobs; partitioning, ordered derives, `rename_all` elision,
enum defaults, deep-patch filters are library-only. Macro consumers get
observably different output from build.rs consumers of the "same" tool.

**(8) Operations do not exist in the data model.** The pipeline's only
awareness of operations is the partitioner's BFS over raw JSON. There is no
type anywhere representing "operation with parameters, request body,
responses, auth." Client generation currently has nothing to attach to — it
would have to be a third bolt-on phase, which is precisely the progenitor
architecture whose customization ceiling started this project.

Of these, (1) is strategic risk, (2)(4)(5)(6)(7)(8) are accidental
complexity — consequences of building a style engine *inside someone else's
schema compiler* and an operation model *outside the data model*. Only part
of (3) plus the inherent difficulty of JSON Schema semantics is essential
complexity.

---

## 3. Landscape notes

What comparable tools actually do, and what to steal or avoid.

**progenitor (Oxide).** `Generator { type_space: TypeSpace, settings }`;
converts `components.schemas` to schemars types and calls
`TypeSpace::add_ref_types`; walks `openapiv3::OpenAPI` paths into an internal
`OperationMethod` list; emits a `Client` + `types` module via
`type_space.to_stream()`; interface styles (positional/builder), `TagStyle`,
pre/post hooks, CLI + httpmock generation. Customization is typify's stock
surface re-exported (`with_derive/patch/replace/conversion`) — no per-field
control, no module partitioning, no style policies. 3.0-only (the
`sabre-progenitor` experiment needed a Node down-converter for 3.1).
*Steal:* the layering proof — operations are a thin, tractable layer over a
type engine; builder-style interfaces; `httpmock` helpers as a later
deliverable. *Avoid:* customization ceiling = whatever the type engine
exposes; that ceiling is why the fork exists.

**typify upstream.** Existing extension points: `with_replacement` (named
type swap), `with_conversion` (schema-shape → named type — this *could* have
expressed the date/date-time/uuid overrides and arguably unconstrained
strings), `with_patch` (`TypeSpacePatch`: rename + extra derives per type),
`x-rust-type` + `with_crate` (spec-declared type substitution). None of the
optionality, derive-ordering, attr-position, rename-elision, partitioned-
output, or deep-patch features have an upstream seam. Upstream's declared
direction (issues #579, #737, #886) is: own IR replacing schemars 0.8 types,
eventually 2020-12 support, improved conformance. There is no policy/hook API
on their roadmap. **Upstreaming ~20 style knobs is implausible** (opinionated,
combinatorial, and upstream is mid-rewrite). Upstreaming *one* policy-trait
seam is conceivable but would land, if ever, after their IR rewrite — i.e.,
after the fork has to be redone anyway.

**openapi-generator (Java).** Parse → `CodegenModel`/`CodegenOperation`
template models → per-language Mustache templates + config + user template
overrides. *Lesson:* the template-model layer decouples parsing from
emission (good idea, poorly typed), but customization = "replace the whole
template," logic leaks into templates, per-language quality varies wildly.
Template-string emission of Rust in particular forfeits everything
`syn`/`quote`/`prettyplease` give us. Avoid.

**oapi-codegen (Go).** kin-openapi parse → internal model → `text/template`
with ~60 helpers; `output-options.user-templates` overrides by template name;
`overlay:` for spec patching; YAML config. Same lesson as openapi-generator:
wholesale template replacement is the only real escape hatch. Its config
file and overlay support validate our patch-first + config-file direction.

**Kiota (Microsoft).** The strongest architectural comparison. Four stages:
parse (OpenAPI → URL tree) → **build a language-agnostic CodeDOM** (the whole
client: namespaces, classes, methods, properties) → **refiners** (ordered,
language-specific mutation passes over the CodeDOM: naming, reserved words,
imports, idioms) → **writers** (type-keyed dumb renderers). *Steal:* IR of
the entire client (types + operations), style as ordered passes over IR,
emission as trivial writers. This is candidate (b)+(d) below, proven at
Microsoft-Graph scale.

**hey-api/openapi-ts.** Parses *all* OpenAPI versions into its own IR
(explicit "parser prepares canonical input" stage), then a plugin pipeline
walks IR events and emits through a TypeScript AST DSL; plugins compose
(types, SDK, Zod, TanStack Query, mocks). *Steal:* own IR normalizing
2.0/3.0/3.1 up front; generation targets as independent composable emitters
over one IR; their v2 lesson that plugins need typed IR access
(issue #1443) — expose the IR as a public, documented type from day one.

**Stainless.** OpenAPI + `stainless.yml` DSL: resource/method mapping,
per-endpoint config, per-language skips — "the config captures decisions
that don't belong in the spec." **Speakeasy.** OpenAPI-native: Overlay spec
for spec repair, `gen.yaml` for generation config, `x-speakeasy-*`
extensions for per-node behavior, custom-code regions preserved by 3-way
merge. *Steal from both:* the two-plane customization split — **spec plane**
(patches/overlays fix what the API truly is) vs **style plane** (config file
decides what the code looks like). We already have the spec plane right; the
style plane should become declarative data like theirs, not code-only.

**Fern.** OpenAPI (or their DSL) → a versioned, language-neutral IR →
independent per-language generators consuming the IR. Further validation of
IR-first with pluggable backends.

**Synthesis.** Every modern generator that needs granular, multi-target
output converged on the same skeleton: *typed parse → purpose-built IR
covering types and operations → ordered policy/refiner passes → thin
emitters*, with spec repair as overlays/patches and style as declarative
config. Nobody with these requirements runs style decisions through a forked
third-party schema compiler. The current stack's architecture is the outlier,
and the fork's growth pattern (knob accretion into foreign internals) is the
mechanism by which it will keep getting worse.

---

## 4. Candidate architectures

### (a) Status quo+: keep the knob-fork + driver split, tidy it

Consolidate the three optionality enums into one policy struct, factor the
attr/derive engine into its own module inside the fork, cut the macro/CLI
plumbing for library-only features, document invariants, automate a weekly
`git merge upstream/main` CI job.

- **Pros:** zero migration risk; everything already works and is pinned by
  tests; cheapest for the next six months; the fork base is current today
  and 4 commits deep.
- **Cons:** the upstream-IR-rewrite time bomb is untouched — the merge job
  will one day open a 2,000-line conflict that amounts to "reimplement the
  fork"; knob accretion continues at four surfaces per feature; operations
  still have no data model, so client-gen becomes a progenitor-shaped bolt-on
  *on top of a fork of progenitor's foundation* — two Oxide codebases to
  track; 3.1 is capped by schemars 0.8 until upstream's rewrite (the same
  event that breaks the fork); testing stays "grep the output."
- **Verdict:** rational only if the tool's scope freezes at "types for
  Sabre-style specs." Requirements 1, 2, and 5 all reject that premise.

### (b) Own intermediate representation

Parse OpenAPI with a typed model into a purpose-built IR — **types and
operations together** — apply style/policy as ordered IR→IR passes, emit
through pluggable backends (types now, client later). typify demoted to
inspiration plus a temporary differential-testing oracle.

- **Pros:** the fork is *deleted*, not maintained — zero sync cost forever;
  operations are first-class from day one (client-gen becomes an emitter,
  not a new architecture); 3.1 is a front-end normalization concern, fully
  decoupled from any upstream's schema-parser timeline; the two-phase
  partition collapses into one pass that sees roles, shapes, and names
  simultaneously; per-field overrides address IR nodes that carry their
  JSON-pointer origin (typed selectors, precise diagnostics); profiles
  become data compiled into pass configuration; macro parity is free
  (the macro invokes the same library on the same config file); testability
  jumps (IR snapshots + pass-level unit tests + compile tests, instead of
  string-grepping rendered output).
- **Cons:** you own JSON Schema semantics — allOf merging, oneOf/anyOf
  enum construction, inline-schema naming, recursion/boxing, untagged-enum
  dedup — the swamp typify spent four years draining. Honest sizing: the
  subset that appears in *API specs* (as opposed to arbitrary JSON Schema
  like Vega's) is bounded — primitives+formats, arrays, maps, objects,
  string enums, `oneOf`/`anyOf` (with the discriminator we currently
  throw away), `allOf` (whose Compose path the fork already implements
  from scratch), nullability, `$ref` cycles. Estimated 3–5 kLoC of
  well-understood compiler work, de-risked by differential testing against
  typify on the existing fixtures. It is real cost, paid once, versus fork
  tax paid forever.
- **Variant (b-lite), considered and rejected:** keep *upstream* typify
  (unforked) as the schema front-end and build the IR from `TypeSpace`
  introspection (`iter_types`, `TypeDetails`). Rejected because typify's
  public introspection is far narrower than its internal model: wire names,
  defaults, constraints, docs, and property schemas are partially or wholly
  invisible, and — decisively — upstream *bakes style decisions in before
  introspection* (optionality, newtype-ness, defaults handling). We would be
  reverse-engineering conclusions back into facts, which is the fork's
  inversion problem moved one layer later.

### (c) Policy/trait-based typify

Replace the ~20 knobs with a small number of seams inside the fork — e.g.
`trait StylePolicy` (per-site optionality, naming, native-type mapping,
derive/attr lists) and an `Emitter` abstraction for output shape — so the
fork shrinks to trait-object call sites, and pitch the seam upstream.

- **Pros:** dramatically better internal hygiene at maybe a third of the
  fork's current diff; keeps typify's battle-tested schema semantics; the
  policy-trait *interface design* is genuinely right (and is adopted into
  the recommendation).
- **Cons:** it is still a fork of the same hot files, so the strategic
  exposure to upstream's IR rewrite is *unchanged* — the seams cut through
  code that is scheduled to be rewritten; policies are code, so the macro
  surface degrades to "named built-in policies" (dialect problem remains);
  operations remain homeless, so half of (b) gets built anyway for
  client-gen; the partition stays two-phase; upstreaming odds are poor
  (upstream mid-rewrite, style hooks not on their roadmap), and even if a PR
  were accepted, the trait's evolution would be forever negotiated with
  upstream's priorities.
- **Verdict:** (a) with better internals. Same time bomb, nicer wiring. The
  right API shape hosted in the wrong repository.

### (d) Transform-pipeline purism

Generalize today's staged pipeline into a registered-pass system with
ordering/dependencies at every layer: `Value→Value` spec passes,
schema→schema passes, typify in the middle, `syn→syn` passes after.

- **Pros:** maximally incremental from today; arbitrary user hooks
  everywhere; no schema compiler to write.
- **Cons:** without an owned IR, the "stable interfaces" are other people's
  types — `serde_json::Value`, schemars-0.8 `RootSchema` (*deprecated
  upstream*), typify's `TypeSpace` (internal, about to change), `syn::File`.
  Passes get written against whichever layer accidentally exposes the needed
  information, in three different dialects (JSON pointers, knob calls, AST
  visitors). `postprocess.rs` is the cautionary tale already in-tree: a syn
  pass that matches types by identifier string because schema origin is
  gone by the time it runs. Middleware over foreign types multiplies
  coupling; it does not organize it.
- **Verdict:** rejected as the organizing principle; adopted as a
  *mechanism* — the recommendation's pass registry is exactly this idea,
  but running over one owned IR where every node knows its origin.

### Decision matrix

| Criterion | (a) knobs+fork | (b) own IR | (c) policy fork | (d) pass purism |
|---|---|---|---|---|
| Fork-sync cost | high, spiking at upstream rewrite | **zero** | high, same spike | medium (typify still inside) |
| Client-gen path | bolt-on, third phase | **native (ops in IR)** | bolt-on | bolt-on |
| 3.1 / 2020-12 | blocked on upstream | **own front-end** | blocked on upstream | blocked on upstream |
| Per-field granularity | strings + more knobs | **typed selectors on origin-carrying IR** | good (policy sites) | syn-layer, origin lost |
| Macro parity | 4-surface plumbing per knob | **config file, one surface** | named policies only | poor |
| Testability | output grepping | **IR snapshots + pass units + compile tests** | better | output grepping |
| Up-front cost | ~0 | **3–5 kLoC schema compiler** | ~1–2 kLoC refactor | ~1 kLoC |
| Risk character | deferred, compounding | up-front, bounded, de-riskable | deferred, compounding | diffuse, permanent |

---

## 5. Recommended target architecture

**(b), with (c)'s policy traits as the customization seam, (d)'s ordered-pass
registry as the extension mechanism, and (a)'s proven pipeline conventions
(patches, tree writer, staged checkpoints, compile-tests) carried over.**

### 5.1 Shape

```text
            spec plane                         style plane
  ┌────────────────────────────┐    ┌───────────────────────────────┐
  │ load (YAML/JSON, 2.0conv/  │    │ codegen.toml (profile presets │
  │ 3.0/3.1 sniff)             │    │ + per-type/per-field override │
  │  → RFC6902 patches + hooks │    │ tables) → CompiledConfig      │
  │  → normalize to Spec model │    │ + code hooks (StylePolicy /   │
  └──────────────┬─────────────┘    │   custom Pass impls)          │
                 ▼                  └───────────────┬───────────────┘
        lower: Spec → Ir  (schema compilation:      │
        shapes, enums, allOf merge/compose,         │
        inline naming, cycles → Box, discriminator) │
                 ▼                                  ▼
        pass pipeline over Ir (ordered):  Naming → TypeMap →
        Optionality → SerdeSurface → Derives/Attrs → ImplSynth →
        PatchCompanion → Partition(roles) → Imports → [user passes]
                 ▼
        emit backends:  types (syn/quote/prettyplease)   [now]
                        client (reqwest builder-style)    [later]
                 ▼
        write: single file | module tree (idempotent, @generated)
```

Crate layout (one workspace; start as modules inside `openapi-codegen` and
split when the macro crate forces it — a proc-macro crate must depend on the
library crate):

```text
openapi-codegen/            # workspace
  crates/
    oc-spec/       # load, patch, version-normalize → owned Spec model
    oc-ir/         # the IR types + arenas + origin/selector machinery
    oc-lower/      # Spec → Ir (the schema compiler)
    oc-passes/     # Pass trait, registry, all built-in passes, StylePolicy
    oc-config/     # codegen.toml schema, profile presets, selector parsing
    oc-emit/       # Ir → syn::File tree (types backend) + tree writer
    oc-emit-client/# (milestone 5) Ir → client backend
    openapi-codegen/       # facade lib + staged pipeline + Generator builder
    openapi-codegen-cli/   # thin clap binary
    openapi-codegen-macro/ # import_types!(spec = "...", config = "...")
```

### 5.2 Core data model (sketches, not implementations)

```rust
// ── oc-spec ──────────────────────────────────────────────────────────
/// Owned, version-normalized view of the document. 2.0-converted and 3.0
/// and 3.1 dialect differences are resolved *here* (nullable vs type:
/// ["T","null"], exclusive bounds, etc.); everything downstream sees one
/// model. Schemas stay close to JSON Schema but are typed.
pub struct Spec {
    pub meta: SpecMeta,                       // title, version, dialect
    pub schemas: BTreeMap<SchemaName, Schema>,   // components.schemas
    pub operations: Vec<OperationSpec>,          // flattened paths × methods
    pub security: BTreeMap<String, SecurityScheme>,
}
pub struct Schema { pub node: SchemaNode, pub origin: Origin, /* … */ }
pub enum SchemaNode {
    Object { properties: IndexMap<String, Schema>, required: BTreeSet<String>,
             additional: Option<Box<Schema>>, /* … */ },
    String { format: Option<String>, enumeration: Vec<String>,
             constraints: StringConstraints },
    Integer { format: Option<String>, bounds: NumBounds },
    Array { items: Box<Schema>, /* … */ }, Number {/*…*/}, Boolean,
    Ref(SchemaName),
    AllOf(Vec<Schema>), OneOf { variants: Vec<Schema>,
                                discriminator: Option<Discriminator> },
    AnyOf(Vec<Schema>), Null, Any,
}
/// JSON pointer into the *patched* document — the addressing spine for
/// overrides, diagnostics, and provenance comments.
pub struct Origin(pub String);

// ── oc-ir ────────────────────────────────────────────────────────────
pub struct Ir {
    pub types: TypeGraph,             // arena; TypeId-indexed
    pub operations: Vec<Operation>,
    pub modules: ModuleTree,          // populated by PartitionPass
    pub meta: SpecMeta,
}
pub struct TypeGraph { defs: Vec<TypeDef>, by_name: BTreeMap<String, TypeId> }

pub struct TypeDef {
    pub id: TypeId,
    pub name: TypeName,               // rust ident + originating schema key
    pub origin: Origin,
    pub shape: Shape,
    pub docs: Docs,                   // description; schema block opt-in
    pub derives: DeriveList,          // ORDERED; filled by DerivePass
    pub attrs: AttrSet,               // position-aware; filled by AttrPass
    pub impls: Vec<ImplSynth>,        // Default/Display/FromStr/AsRef/From
    pub module: Option<ModulePath>,   // filled by PartitionPass
    pub usage: UsageSummary,          // (op, Role) pairs; filled from ops
}
pub enum Shape {
    Struct(StructShape),   // fields, flatten_bases: Vec<TypeRef>, deny_unknown
    Enum(EnumShape),       // tagging: External | Internal{tag} | Adjacent{..}
                           //          | Untagged, variants w/ payloads
    Newtype(NewtypeShape), // inner: TypeRef, constraints, validating: bool
    Alias(TypeRef), Primitive(Primitive),
    List(TypeRef), Map(TypeRef, TypeRef), Tuple(Vec<TypeRef>),
    Any, Unit,
}
pub struct FieldDef {
    pub rust_name: Ident,
    pub wire_name: String,            // authoritative (rename-elision safe)
    pub ty: TypeRef,                  // Optional<…> decided by a pass
    pub required: bool,
    pub default: Option<serde_json::Value>,
    pub serde: FieldSerde,            // rename?/default?/skip_serializing_if?
    pub patch: Option<PatchMode>,     // deep-patch annotation
    pub origin: Origin,
}
pub struct Operation {
    pub id: OpId,                     // snake ident + raw operationId
    pub method: HttpMethod,
    pub path: PathTemplate,           // literal segments + typed params
    pub params: Vec<Param>,           // location: Path|Query|Header|Cookie
    pub request: Option<Body>,        // content-type → TypeRef
    pub responses: Vec<Response>,     // status range → content-type → TypeRef
    pub auth: Vec<SecurityRequirement>,
    pub docs: Docs, pub tags: Vec<String>, pub origin: Origin,
}

// ── oc-passes ────────────────────────────────────────────────────────
pub trait Pass {
    fn name(&self) -> &'static str;
    fn run(&self, ir: &mut Ir, cx: &mut PassCx<'_>) -> Result<()>;
}
/// Ordered; profiles compile to one of these. User passes append/insert.
pub struct Pipeline { passes: Vec<Box<dyn Pass>> }

/// The policy seam consumed by the built-in passes. The default impl reads
/// CompiledConfig (TOML); library users override methods with code.
pub trait StylePolicy {
    fn optionality(&self, f: FieldCx<'_>) -> Optionality;   // Bare | Wrap | WrapDropDefault
    fn native_type(&self, site: FormatCx<'_>) -> Option<RustPath>; // date/uuid/…
    fn constraints(&self, c: ConstraintCx<'_>) -> ConstraintMode;  // Validate | Ignore
    fn derives(&self, t: TypeCx<'_>) -> DeriveList;          // ordered, per kind
    fn attrs(&self, t: TypeCx<'_>) -> AttrSet;               // before/after derive
    fn rename(&self) -> RenameStyle;    // RenameAll(Case) | PerField | Dual(..)
    fn module_for(&self, t: TypeCx<'_>) -> Option<ModulePath>; // partition veto
    fn deep_patch(&self, owner: TypeCx, field: FieldCx, inner: TypeCx) -> bool;
    fn synth_default(&self, e: EnumCx<'_>) -> DefaultSynth;  // incl. untagged oneOf
}
```

Two properties do the heavy lifting:

- **Every IR node carries `Origin`.** Per-field config selectors resolve to
  nodes with exact error messages ("override at
  `#/components/schemas/Foo/properties/bar` matched nothing — schema renamed
  by patch 003?"). Partition, docs provenance, and future
  source-map-style diagnostics all ride the same field.
- **Passes see the whole IR.** The `shared/enums` routing that today needs a
  post-typify fix-up phase is a plain condition inside `PartitionPass`
  (`matches!(shape, Shape::Enum(e) if e.all_unit())` on a node whose `usage`
  is already populated). The untagged-oneOf `Default` that today is a syn
  post-process is `ImplSynth::Default` decided with full shape knowledge.

### 5.3 Style as data: profiles and overrides

`codegen.toml` (checked in next to the spec; the CLI takes `--config`, the
macro takes `config = "…"`, build.rs loads the same file — one surface, three
consumers):

```toml
profile = "api-client"        # named preset merged underneath this file

[spec]
path    = "specs/sabre-booking/spec.openapi.yaml"
patches = "specs/sabre-booking/patches"

[style]
optional-fields     = "always-option"   # non-required → Option<T>
constrained-strings = "plain"
integers            = "plain"
date = "::std::string::String"
date-time = "::std::string::String"
uuid = "::std::string::String"
rename-all = "camelCase"
allof = "compose"
enum-default = "first-unit-variant"

[style.derives]
structs  = ["Debug","Clone","Default","PartialEq","Serialize","Deserialize","Patch"]
enums    = ["Debug","Clone","PartialEq","Serialize","Deserialize"]
newtypes = ["Debug","Clone","Default","PartialEq","Serialize","Deserialize"]

[style.attrs.structs]
before-derive = ["#[serde_with::skip_serializing_none]"]
after-derive  = [
  '#[patch(attribute(serde_with::skip_serializing_none))]',
  '#[patch(attribute(derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)))]',
  '#[patch(attribute(serde(default, rename_all = "camelCase")))]',
]
[style.conditional.schemars]            # cfg_attr(feature = "schemars", …)
derives = ["schemars::JsonSchema"]

[modules]
partition    = "operation-role"          # off | operation | operation-role
shared-enums = true

[types."Agency"]                         # per-type override (schema name)
derives.add = ["Eq"]
module = "shared/common"

[fields."CancelBookingRequest.notification"]   # per-field (Type.field)
deep-patch = true

[fields."#/components/schemas/Foo/properties/bar"]  # or JSON pointer
type = "::my_crate::Bar"
```

Selector forms are typed (`SchemaName`, `TypeDotField`, `JsonPointer`,
later globs), parsed and validated at config-load time, resolved against
`Origin`. Unmatched selectors are hard errors — the config cannot silently
rot the way stringly-typed knob values can.

Code stays the escape hatch, not the primary surface: `Generator::policy(P)`
for a custom `StylePolicy`, `Generator::pass_after("partition", MyPass)` for
custom passes (the `deep_patch_filter` closure use case), and the staged
checkpoints (`Spec → Ir → syn tree`) remain for arbitrary surgery. Everything
today's `customize`/`patch_spec_with`/staged API can do has a home.

### 5.4 Fate of the typify fork

- **Dropped from the primary path.** Not shrunk, not synced — retired. The
  knowledge it encodes moves into: pass implementations (the appendix table
  maps every knob), the fixture corpus (its test schemas), and the parity
  harness.
- **During migration**, the fork remains in-tree as the *oracle*: a
  differential harness generates both ways and compares normalized output on
  the petstore + Sabre fixtures until the IR path matches, then the branch is
  tagged and frozen. No further upstream syncs, ever.
- **Optional legacy door:** the `lower` CLI subcommand (OpenAPI → draft-07
  JSON Schema) can survive for people who want to feed *upstream* typify or
  `cargo typify` — it is 145 lines with no fork dependency.
- **Upstream contributions**, if any, become à la carte gifts (e.g. the
  TryFrom<String>/blanket-impl E0119 fix is upstreamable today), not a
  survival strategy.

### 5.5 OpenAPI 3.1 / 2020-12 path

All dialect handling lives in `oc-spec` normalization: 3.1's
`type: ["string","null"]` → the same internal nullability as 3.0's
`nullable: true`; numeric `exclusiveMinimum`; `const` → single-value enum;
webhooks ignored until wanted; `prefixItems` if ever needed → `Shape::Tuple`.
The IR and everything downstream are dialect-free. No dependency on schemars
(0.8 or 1.0) or on typify's Big Plan. This is the single largest structural
win over every typify-hosted candidate.

### 5.6 Testing strategy

1. **Compile-the-output examples** (kept, highest value): checked-in
   generated trees for petstore + Sabre, built and behavior-asserted
   (serialization shape, patch merging, defaults).
2. **Differential harness** (migration only): new path vs frozen-fork path,
   normalized `syn` comparison per fixture.
3. **IR snapshots**: serialize the post-pass IR to a stable, readable format
   (`insta`); reviewable diffs when a pass changes — this replaces most
   "grep the rendered source" tests.
4. **Pass-level unit tests**: IR in → IR out, no rendering involved (e.g.
   OptionalityPass over a synthetic struct).
5. **Round-trip property tests**: generated types must round-trip the spec's
   `example` payloads (which the new lowering keeps instead of stripping).

---

## 6. Migration plan

Strangler pattern. Every step lands independently; the examples and the
checked-in generated trees stay green (byte-stable or deliberately
re-blessed) at each step. The fork is frozen at step 0 and deleted at step 6.

**Step 0 — freeze and fence (small).**
Tag the fork branch. Declare the current checked-in outputs golden. Add the
differential harness skeleton (`tests/parity.rs`: run engine A and engine B,
compare normalized token streams per fixture). No behavior change.

**Step 1 — owned spec model (`oc-spec`).**
Port `load.rs` + patches unchanged; replace `lower.rs`'s in-place `Value`
surgery with `Value → Spec` normalization (keeping `discriminator` and
`example`s this time). Feed typify by *rendering* `Spec` back to a draft-07
`RootSchema` so the old engine keeps working byte-identically. Partition
still reads the raw `Value` at this step. Examples green because output is
unchanged.

**Step 2 — IR + types emitter behind a flag (`oc-ir`, `oc-lower`,
`oc-emit`).**
The schema compiler: structs/enums/newtypes/aliases, allOf merge+compose
(port the fork's Compose logic — it is driver-adjacent code, not typify
code), oneOf/anyOf with discriminator, inline-schema naming, cycle boxing.
`Generator::engine(Engine::Ir)` opt-in. Exit criterion: parity harness passes
on petstore, then on Sabre (allowing an explicit, reviewed list of
*improvements* where byte-parity is undesirable). This is the long pole;
everything after it is porting, not inventing.

**Step 3 — passes + config (`oc-passes`, `oc-config`).**
Port the ApiClient profile from `profile.rs` knob calls to a preset
`codegen.toml` + default `StylePolicy`. Port partitioning into a single-phase
`PartitionPass` over the IR (delete the `to_rust_partition` reach-back and
`postprocess.rs` — enum defaults become `ImplSynth`). Flip the default engine
to IR once both example suites pass on it; keep `Engine::Typify` for one
release as the escape hatch.

**Step 4 — retire the fork; macro parity.**
Remove the typify dependency from the default path. Rewrite the macro as
`openapi_codegen::import_types!(spec = "…", config = "…")` — a thin
proc-macro over the same library, giving the macro the *full* feature set
(partitioning included) for the first time. Migrate `via-macro`;
`openapi-codegen-examples` becomes the three-surface parity proof.

**Step 5 — operations into the IR.**
Parse parameters/request bodies/responses/auth into `Operation` (they were
already in `Spec` since step 1). `PartitionPass` consumes real roles from IR
instead of raw-JSON BFS — delete `partition.rs`'s `Value` walking. Synthesize
`<Op>Request`/`<Op>Response` names for inline bodies, matching the
sabre-types layout. No client emission yet; types output is unchanged except
better naming for inline operation schemas.

**Step 6 — client emitter (`oc-emit-client`).** See §7.

Rough sequencing note: steps 1–3 are the investment (step 2 dominates);
steps 4–5 are mostly deletions and ports. If step 2 stalls, the project
degrades gracefully to today's working stack — the frozen fork keeps
working; nothing is broken mid-flight because the old engine remains the
default until step 3's exit criterion.

---

## 7. Client generation outlook

What the milestone looks like on top of the IR (progenitor-inspired,
consuming our IR instead of typify's TypeSpace):

- **Placement:** the partitioned module tree already mirrors the client's
  shape. Each operation module gains the operation function/builder; a root
  `client.rs` holds `Client { base_url, http: reqwest::Client, auth: … }`.
- **Interface style:** builder-first (matches the granular-control ethos):
  `client.cancel_booking().confirmation_id("…").body(&req).send().await`
  with typed setters generated from `Operation.params` (path/query/header,
  style/explode-aware) and `Body`. A positional variant can be a config
  toggle later — it is an emitter concern, not an IR concern.
- **Responses:** per-op `enum <Op>Error` from non-2xx responses; success
  type from the 2xx content map; `ResponseValue<T>`-style wrapper only if
  headers prove necessary (decide then, not now).
- **Auth:** `SecurityScheme` is normalized in `Spec` since step 1; emit
  header/query/bearer injectors per requirement; custom schemes drop to a
  user hook on the request builder.
- **Reuse:** the client emitter consumes the *same* `TypeRef`s the types
  emitter placed into modules, so imports resolve by construction; the
  role model that today only organizes files also tells the client which
  types are request-position (borrow, `impl Into`) vs response-position
  (owned).
- **Later, cheaply, because the IR is there:** `httpmock`-style test
  helpers per operation (progenitor proved the pattern), pagination
  conventions via per-operation config keys, retry/idempotency annotations.

The load-bearing point: none of this requires revisiting the type pipeline,
because operations entered the data model in step 5 and the module tree was
theirs from the start.

---

## 8. Risks

**R1 — Schema-semantics regressions (the big one).** Owning the schema
compiler means owning the corner cases: pathological `allOf` merges,
`anyOf` overlap, recursive boxing decisions, name-collision handling.
*Mitigation:* the API-spec subset is deliberately scoped (arbitrary JSON
Schema is a non-goal — that remains typify's turf, reachable via the `lower`
door); the differential harness pins the two real fixtures plus the fork's
own test schemas; unsupported constructs fail loudly with `Origin`-precise
errors plus the documented workaround (patch the spec — the tool's oldest
muscle). Accept: the first unfamiliar spec after migration will find bugs
typify would not have had. That is the price of the fork's deletion, paid
in debuggable, owned code.

**R2 — The rewrite stalls and two stacks rot.** One maintainer, a 3–5 kLoC
step 2, and a day job. *Mitigation:* the strangler order is designed so the
old engine stays default and shippable until the new one passes parity;
steps land independently; the fork is frozen (no sync work) rather than
deleted early, so a six-month pause costs nothing but delay. The explicit
tripwire: if step 2 exceeds ~2× its estimate, stop and re-evaluate (c) on
top of upstream's *post-rewrite* typify, which by then will have shown its
new internals.

**R3 — The knob farm regrows as a config farm / IR over-generalization.**
Twenty knobs could become forty TOML keys and an IR that tries to model all
of JSON Schema. *Mitigation:* two standing rules. (1) Every config key must
be consumed by exactly one named pass through the `StylePolicy` seam — if a
proposed key has no single home, it becomes a code hook instead. (2) The IR
models what the emitters need, not what JSON Schema can express; anything
else is normalized away in `oc-spec` or rejected with a pointer to the patch
mechanism. The escape hatch stays code (custom `Pass`), so config pressure
has a relief valve that does not widen the schema.

Minor risks, noted: `struct_patch` remains a consumer-side proc-macro
dependency (option later: synthesize `<T>Patch` companions directly in IR
and drop the dependency); prettyplease formatting drift across versions can
dirty golden files (pin it); the macro crate drags the whole pipeline into
proc-macro context (compile-time cost — acceptable, progenitor does the
same).

---

## Appendix A — every fork feature's new home

| Fork feature (FORK_FEATURES.md) | New home |
|---|---|
| Date / date-time / uuid type overrides | `StylePolicy::native_type` ← `[style]` keys |
| Unconstrained strings / ints | `StylePolicy::constraints` ← `constrained-strings`, `integers` |
| Array / bool / defaulted-field optionality (3 enums) | one `StylePolicy::optionality` with `FieldCx` (shape, required, default) |
| Elide `Option` serde noise | `SerdeSurfacePass` (implied by `skip_serializing_none` attr + policy) |
| `allOf` Merge/Compose | `oc-lower` composition (fork's Compose logic ported); `allof` key |
| Conditional derives/attrs (cfg-gated) | `DerivePass`/`AttrPass` ← `[style.conditional.<feature>]` |
| Unconditional ordered derives, kind filters | `DeriveList` (ordered by construction) ← `[style.derives.*]` |
| Unconditional attrs + `AttrPosition` | `AttrSet` (position-aware) ← `[style.attrs.*]` |
| `struct_rename_all` + per-field elision | `RenameStyle::RenameAll` — trivial because `FieldDef.wire_name` is authoritative in IR |
| Dual casing surfaces (`SerdeFieldCase`) | `RenameStyle::Dual` variant of the same pass |
| Enum first-variant `Default` | `ImplSynth::Default` in `ImplSynthPass` (also covers the untagged-oneOf case that today needs `postprocess.rs`) |
| Deep patches (bulk + closure filter) | `StylePolicy::deep_patch` (config bulk rule; code override) |
| Docs without embedded schema | `Docs` rendering flag in `oc-emit` |
| Partitioned output (nested modules, preambles, error/defaults duplication) | `ModuleTree` + `PartitionPass` + `oc-emit` (single-phase; no reach-back) |
| `definition_rust_names` bridge | unnecessary — IR nodes carry both names |
| String newtype conveniences (always-on) | `ImplSynthPass` default, policy-overridable (fixes the "always on" wart) |
| RFC 6902 patches, spec hooks | `oc-spec`, unchanged |
| Request/response role split, cross-role boundary + bridge imports | `PartitionPass` + `ImportPass` over IR `usage` — same policy, one phase |
| Staged pipeline checkpoints | `Spec → Ir → syn tree` checkpoints (owned types this time) |
| Idempotent tree writer, `@generated` cleanup | `oc-emit`, unchanged |

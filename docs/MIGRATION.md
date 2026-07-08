# Migration: typify-fork engine → owned Spec → IR → passes → emitter

**Status:** REVERSED — the IR engine was built, gated, audited, and then
retired in favor of building on typify (decision D15 at the bottom; the
`ir-migration` branch preserves the full implementation). Steps 0–2
below and decisions D1–D14 are the historical record of the build-out;
the features the IR engine pioneered (style-as-data, per-type/per-field
overrides, condensed emission) survive, re-implemented over the typify
engine.
**Date started:** 2026-07-03
**Branches:** `ir-migration` in this repo (the IR implementation);
`typify-base` (the reversal); `ergonomic-codegen` in `../typify` (the
fork, unfrozen)
**Parent design:** [ARCHITECTURE.md](ARCHITECTURE.md). This document records
the *actual* plan as re-derived against the code, plus every deviation.

## Scope of this migration (Steps 0–2 of the architecture doc)

1. **Step 0 — fence.** The checked-in outputs under `examples/generated*/`
   are declared golden. A differential parity harness (`tests/parity.rs`)
   compares engines over the fixtures and is `cargo test`-able.
2. **Step 1 — owned spec model.** `src/spec/`: typed `Value → Spec`
   normalization replacing `lower.rs`'s in-place `Value` surgery on the
   typify path, preserving `discriminator` and `example(s)` in the model.
   The old engine consumes a draft-07 render of `Spec` and its output stays
   **byte-identical** (guarded by tests).
3. **Step 2 — IR engine behind a switch.** `src/ir/`: purpose-built IR
   (named types + fields + provenance + operations-as-data), ordered policy
   passes, and a `syn`-based emitter reproducing the single-file and
   folder-tree outputs. `Generator::engine(Engine::Ir)` is opt-in; the
   typify engine remains the default. Style profiles become data
   (`StyleConfig` presets + optional `codegen.toml`).

Steps 3–6 of the architecture doc (macro parity, fork deletion, operations
consuming partitioning, client emitter) are explicitly out of scope here.

## Parity contract

What "the same output" means, per comparison level:

- **Old engine through `Spec` (Step 1):** byte-identical. The lowered
  draft-07 document rendered from `Spec` must equal the old string-surgery
  output as a `serde_json::Value` (order-insensitive map equality — all maps
  involved are sorted), and the final Rust must be byte-identical for both
  fixtures. This is a hard guard; any diff is a bug.
- **IR engine vs frozen fork (Step 2 gate):** compared as **normalized token
  streams** — both outputs parsed with `syn` and compared as
  `proc_macro2::TokenStream` text. This normalizes formatting only; names,
  field order, attribute order, impl bodies, and module structure all
  participate in the comparison. In practice the IR emitter targets
  byte-identical output (same formatter, same ordering rules), and the
  harness also reports byte equality; token-level is the contract so a
  future prettyplease bump cannot fail the gate spuriously.
- **Folder-tree mode:** the planned file map (relative path → content) is
  compared per file with the same contract; the file *sets* must be equal.
- **Divergence policy:** every difference is either a bug in the IR engine
  (fix it) or a deliberate improvement (list it in the decision log; if it
  changes goldens, regenerate them explicitly and re-run the consuming
  examples). The deliberate-improvements list is empty as of the gate.

Gate matrix (all must pass):

| Fixture | Profile | Modes |
|---|---|---|
| petstore | api-client | flat, partitioned, split (single file), split (tree) |
| sabre-booking (patched) | api-client | flat, partitioned, split (single file), split (tree) |

The `Typify` profile is deliberately **not** supported by the IR engine
(decision D3) and keeps running through the frozen fork.

## Architecture as built

```text
load (Value; YAML/JSON)
  → patch (RFC 6902 + Rust hooks)          [unchanged]
  → partition (Value BFS; both engines)    [unchanged; see D6]
  → Spec::from_value (typed normalization) [new, src/spec/]
      ├─ Engine::Typify: Spec::to_draft07() → RootSchema → typify fork
      │                   (byte-identical to the old surgery path)
      └─ Engine::Ir:     ir::lower (Spec → Ir)
                          → pass pipeline (ordered, config-driven)
                          → ir::emit (Ir → syn::File / file map)
  → format (prettyplease) → write (single file | tree)  [unchanged]
```

Modules (all inside the existing crate — see D2):

- `src/spec/` — `Spec`, `SchemaNode`, `Origin`, operations-as-data;
  `from_value` (3.0.x + Swagger-2.0-converted normalization; 3.1 seam) and
  `to_draft07` (the typify bridge).
- `src/ir/` — `Ir`, `TypeDef`, `Shape`, `FieldDef`, `ImplSynth`.
- `src/ir/lower.rs` — the schema compiler: named types, allOf
  compose/merge, oneOf/anyOf, string enums, inline naming, cycle boxing,
  collision handling.
- `src/ir/passes/` — `Pass` trait + built-in passes (naming, type map,
  patchability, optionality, serde surface, derives/attrs, impl
  synthesis, deep patch, partition, imports).
- `src/ir/emit.rs` — `Ir → syn::File` (single file) and file-map planning
  for tree output; reuses `tree.rs` for writing.
- `src/config.rs` — `StyleConfig` (the data form of a style profile),
  built-in presets, `codegen.toml` loading, per-type/per-field overrides.

## Plan / milestones

1. **M0 (fence):** parity harness asserting the *current* engine reproduces
   the checked-in goldens byte-for-byte (petstore + sabre, flat + tree).
   Locks the goldens and exercises the comparison machinery.
2. **M1 (Step 1):** `src/spec/` + old-engine reroute + byte guards.
3. **M2 (Step 2 core):** IR + lowering + passes + emitter; petstore
   api-client parity across all four modes.
4. **M3:** sabre parity across all four modes.
5. **M4:** style-as-data: `StyleConfig` presets + `codegen.toml` +
   per-type/field overrides; CLI `--engine` / `--config`; builder API.
6. **M5 (gates):** full test suite, examples, consumer workspace, clippy.
7. **Stretch:** flip the default engine only if parity is byte-tight
   everywhere and consumers are provably unaffected.

Fallback tripwire (from the architecture doc): if M2+M3 balloon past ~2× the
initial estimate with parity still red, stop, commit what is solid, leave
the old engine as default, and record where the wall is.

## Decision log

- **D1 — hand-rolled `Spec` model; no `openapiv3` dependency.** The fixtures
  are dialect-sloppy in ways a strict third-party 3.0 model resists
  (Swagger-2.0-converted `allOf` patterns, `format: number` on strings,
  `format` without `type`), `openapiv3` has no 3.1 story (the seam we
  explicitly want to own), and the schema half of the model must be
  hand-rolled anyway for lowering. The operations subset we need is small.
  Deserialization is serde-based with an `extra` catch-all per schema node
  so unknown keywords (vendor extensions) survive the round-trip; that
  catch-all is what makes the Step 1 byte-guard trustworthy.
- **D2 — modules, not workspace crates.** ARCHITECTURE.md sketches an
  eight-crate workspace but itself says to start as modules and split when
  the macro crate forces it. The macro milestone is out of scope, the
  consumer workspace depends on this crate by path, and one crate keeps the
  diff reviewable. Deviation from the sketch, not from the intent.
- **D3 — the IR engine rejects `StyleProfile::Typify`.** The Typify profile
  *means* "whatever upstream typify emits" — validating newtypes with regex
  machinery, `NonZero*` mapping, trait-capability derive computation
  (`Eq`/`Hash`/`Ord` propagation), `defaults::` helper-fn synthesis, manual
  `Default` impls. Reimplementing all of that inside the IR engine is
  precisely the "own arbitrary JSON Schema semantics" trap the architecture
  doc scopes out, and it produces zero value: consumers wanting upstream
  shape keep the frozen fork (which stays the default engine). `Engine::Ir`
  + `StyleProfile::Typify` is a loud, documented error. The pass/config
  architecture still replaces the *fork knobs* — every ApiClient knob is
  data — so the "profiles become data" goal holds for the profiles the IR
  engine owns.
- **D4 — optionality modes implemented as data, but only the modes the
  house style uses are emittable in v1.** `always-option` (ApiClient) and
  `required`/plain paths are implemented. The `Bare`-with-schema-default
  modes require `defaults::` helper-fn synthesis in the emitter; the config
  keys exist and are validated, but selecting them under `Engine::Ir` is an
  explicit unsupported-key error until the helper synthesis is built. Honest
  subset over silent wrong output.
- **D5 — schema semantics ported deliberately, unit-tested where fixtures
  are silent.** The fixtures exercise: structs, string enums (incl.
  schema-level `default` variant selection), allOf compose with sibling
  properties and `$ref` bases + inline subschemas, required/optional
  wire shapes, formats (`date`, `date-time`, `email`, `int32`,
  `number`-on-string), patterns (ignored under unconstrained strings), and
  every partition mode. NOT exercised by any fixture: oneOf/anyOf untagged
  enums, `nullable`/Option-via-null, `$ref` cycles → `Box`, inline-schema
  naming, name collisions, allOf merge fallback. Those are implemented
  against typify's documented semantics and pinned by unit tests
  (`tests/ir_unit.rs`) on synthetic schemas instead of the parity gate.
  Merge fallback is pragmatic (object-shaped property union; anything
  else fails loudly with the schema's `Origin`), not typify's full
  JSON-Schema intersection — the patch mechanism remains the documented
  escape for pathological specs.
- **D6 — the `Value`-based partitioner stays, consumed by both engines.**
  ARCHITECTURE.md wants partitioning as an IR pass with roles from IR
  operations; that is Step 5 (out of scope). Rewriting the reachability
  walk against `Spec` now would duplicate policy with drift risk while the
  old engine still consumes the `Value` walk. Instead both engines consume
  the same `Partition` (schema-name keyed). The IR engine's `PartitionPass`
  resolves the shared-enum routing and module imports internally — the
  post-typify `to_rust_partition` reach-back and its
  `definition_rust_names` bridge are not used on the IR path, which is the
  actual two-phase wart the doc complains about.
- **D7 — `xml` and `externalDocs` are dropped by `Spec` normalization**
  (matching the old lowering); `discriminator`, `example`, and `examples`
  are preserved in the model as typed fields and stripped only in the
  draft-07 render for typify. Nothing in the current emitters consumes them
  yet; they exist so per-node information the client generator needs stops
  being destroyed at the front door.
- **D8 — operations are captured in `Spec` (and summarized into `Ir`) but
  nothing consumes them yet.** Parameters, request/response content types
  with schema refs, and security requirement names are parsed; the client
  emitter (Step 6) and partition-from-IR (Step 5) attach here later.
- **D10 — two latent bugs of the string-surgery lowering are fixed, not
  reproduced.** (1) A numeric (draft-07 / 3.1 style) `exclusiveMinimum` /
  `exclusiveMaximum` was silently *deleted* by the old walker (its
  `remove` ran before the boolean pattern-match); the model passes numeric
  bounds through. (2) The old walker `remove`d any key named `nullable`
  anywhere — including an object *property* named `nullable`, which would
  have been silently dropped from generated types. Structural typing makes
  that impossible. Neither fixture exercises either case, so the byte
  guards are unaffected.
- **D9 — byte-parity quirks reproduced intentionally.** Two worth naming:
  schema-`default` enum `Default` impls reference the variant as
  `TypeName::Variant` while first-unit-variant synthesis uses
  `Self::Variant` (typify emits the former, the fork's knob the latter);
  and empty partition modules referenced by import preambles are
  materialized (error-mod-only leaf files). The emitter reproduces both.
- **D11 — name collisions are loud errors on the IR path.** typify
  silently reuses the first definition when two schema keys sanitize to
  the same Rust name (`useCSL`/`useCsl` → `UseCsl`). The IR lowering
  errors with both origins instead. Same for property-name collisions
  within a struct and non-unique untagged-variant names. The fixtures
  have no collisions, so parity is unaffected.
- **D12 — the default engine did NOT flip (stretch declined).** Parity
  is byte-exact, but flipping the default would change what
  `Generator::customize(|TypeSpaceSettings| …)` *means*: registered
  hooks would either error (breaking the `via-build-script` consumer,
  which calls it) or be silently ignored (worse). The default stays
  `Engine::Typify`; `Engine::Ir` is opt-in. Flip checklist for later:
  (1) decide the `customize` story on IR (hard error with a migration
  message is probably right, after consumers migrate to `style()`),
  (2) one release of soak with both engines available, (3) regenerate
  goldens through IR at the flip so the `@generated` provenance is
  honest.
- **D13 — patchability is resolved once into IR state, not consumed by
  the passes that act on it.** `struct_patch` support became optional
  and per-type (`[style] patch = true|false`, `[types."Name"] patch`,
  both mirrored on `StyleConfig`/`TypeOverride` for `style()` hooks).
  Patchability inherently spans two emission decisions — the `Patch`
  derive + `patch(...)` attribute lines (`derives-attrs` pass) and the
  `#[patch(name = "Option<{Inner}Patch>")]` field annotations
  (`deep-patch` pass) — which collides with the one-key-one-pass rule
  (R3). Resolution: a new early `patchability` pass is the single
  consumer of the `patch` keys; it materializes a `patchable` flag on
  every IR `TypeDef` (structs only — explicitly targeting a non-struct
  is a hard error), and downstream passes read that IR state, never the
  config. The alternative — one monolithic patch pass owning all patch
  emission — was rejected because it would carve the `Patch` derive and
  `patch(...)` attrs out of the declarative `derives`/`attrs` lists
  into pass-private knowledge, making the api-client preset no longer
  the whole truth about the output. Cross-type consistency is enforced
  where each side lives: `derives-attrs` strips derive + attrs on
  non-patchable owners; `deep-patch` prunes annotations when *either*
  the owner (its derive is gone) or the inner type (its `{Inner}Patch`
  companion won't exist) is non-patchable, and hard-errors when a
  forced `deep-patch = true` override demands an impossible annotation;
  `imports` drops `struct_patch`-rooted `use` statements when structs
  exist but none is patchable (so fully patch-free output doesn't
  require the dependency), and leaves the preamble untouched otherwise
  — with everything patchable (the default), output is byte-identical
  to before, and the parity gate + goldens are unaffected. IR-engine
  only: the typify engine's Patch behavior stays the frozen fork's
  all-or-nothing unconditional-derive mechanism.

- **D14 — readable output is an emit style resolved onto the IR, defaulting
  to parity.** The fork's shape buries types: every string enum drags a
  ~50-line `Display`/`FromStr`/`TryFrom<&str>`/`TryFrom<&String>`/
  `TryFrom<String>`/`Default` ladder behind it, and every partition module
  duplicates the `error` mod. `[style] emit-style = "expanded" | "condensed"`
  (kebab-case, `StyleConfig::emit_style`) picks the layout; its single
  consumer is the new `emit-style` pass, which materializes the choice as
  `Ir::emit_style` so the emitter reads IR state, never config (the D13
  pattern). Condensed hoists boilerplate instead of deleting it: one
  `support` module per generation unit (its own `support.rs` in tree mode)
  holds the single `error` mod plus a `macro_rules! impl_string_enum`
  whose expansion is token-equivalent to the expanded impls (pinned by a
  unit test against the hand-formatted rendering, and behaviorally by
  `examples/petstore_tree_condensed` + the consumer round-trip suites);
  each enum's ladder becomes one `impl_string_enum!(Name { Variant =>
  "wire", … } default = Variant);` invocation; and each module that
  duplicated `error` re-exports `support::error` instead, keeping every
  historical `<module>::error::ConversionError` path resolving (the N
  identical `ConversionError` types collapse into one — strictly more
  code compiles, none breaks). Macro scoping is the standard
  `pub(crate) use` re-export + per-module `use` import, path-anchored
  (`self::`/`super::…`) at every depth and independent of textual order;
  `self::error` inside expansions resolves via the invoking module's
  re-export. Two deliberate calls: (1) **presets keep
  `emit-style = "expanded"`** — the parity suite's byte-identity oracle
  against the frozen fork stays meaningful with zero test contortion,
  `profile = "api-client"` keeps meaning the fork recipe (no silent
  output churn for build-script consumers), and consumers flip the key in
  their `codegen.toml` (the examples workspace does); (2) the **D9
  `Self::Variant` vs `TypeName::Variant` quirk is normalized away** in
  condensed output (`$Type::$default` — semantically identical, and
  condensed is by definition off the byte-parity path). Because
  prettyplease flows macro tokens into an unreadable wall, rendering
  gets a token-verified post-pass (`polish_rendered`): the macro
  definition is substituted with a pinned hand-formatted rendering and
  invocations are reflowed one pair per line, each reflow re-parsed and
  token-compared before acceptance (a no-op for macro-free output, so
  the typify engine and expanded style are byte-untouched — the full
  suite passes unchanged). The condensed style additionally reserves the
  top-level `support` module name (loud error on collision, matching
  D11). One new mechanism was deliberately *not* built: an
  `impls.rs`-per-module / end-of-file impl grouping third lever — the
  macro + shared-support pair already removes the bulk (sabre tree:
  ~36% fewer lines, `shared/enums.rs` −67%), and "not convoluted" was a
  hard requirement. Newtype conversion chains are moot until the IR
  engine emits newtype shapes (D5). IR-engine-only, like every
  `StyleConfig` key: the typify engine and the frozen fork are
  untouched.

- **D15 — the migration is reversed: typify is the base; the IR engine
  is retired.** The [SPEC_COVERAGE.md](SPEC_COVERAGE.md) audit gave the
  empirical verdict: the IR engine failed 5 of 7 structurally diverse
  real-world specs and produced six silently-wrong lowerings — its
  schema-semantics core would need to re-earn, construct by construct,
  what typify's years of accumulated corpus already guarantee (it is
  what progenitor runs GitHub's spec through). Re-weighing the original
  motivation for the IR (upstream's announced rewrite invalidating the
  fork): the `typify2` branch turns out to be an early-stage prototype,
  stale for months and unable to name nested types yet, so "v1 is
  doomed ground" was overweighted — and the fork's actual rebase
  liability was concentrated in two default-behavior changes whose
  golden churn (~110k lines) has since been eliminated by restoring
  upstream defaults behind opt-in knobs. The reversal keeps everything
  the IR migration got right, re-homed:
  - the typed `Spec` model (step 1) stays — the typify path consumes
    its draft-07 render, and it remains the seam for operations, 3.1,
    and discriminators;
  - `StyleConfig` / `codegen.toml` / presets (M4) stay as THE style
    surface, now mapped onto `TypeSpaceSettings`
    (`config.rs::apply_to_settings`); `plain`-profile modes the IR
    hard-errored on (validating newtypes, bare optionality, D3/D4)
    now simply work, because typify implements them;
  - per-type patch opt-out and per-field deep-patch (D13) ride the
    fork's `with_deep_patch_filter` at the source plus a
    post-generation AST strip (`overrides.rs`), with the same
    hard-error validation semantics;
  - the condensed emit style (D14) becomes a token-verified AST
    transformation over typify output (`condense.rs`): a ladder is
    replaced only after the macro expansion is verified token-equal
    to the impls removed, so it degrades to expanded instead of ever
    changing behavior — the checked-in condensed golden reproduces
    byte-for-byte;
  - the parity harness's golden fence survives as `tests/goldens.rs`
    (the engine-vs-engine half retired with the engine); the
    patch-config and emit-style suites were ported unchanged in
    substance and pass against the typify path.
  The fork is unfrozen and rebase-clean: with no knobs set its output
  is byte-identical to upstream `main` (upstream test goldens
  unchanged), so upstream syncs conflict only on real feature hunks.
  Client generation (step 6) attaches to `Spec`'s operations data on
  the typify base. *(2026-07-04 follow-up: the successor audit —
  docs/SPEC_COVERAGE.md, now rewritten for the typify base with the IR
  audit as its appendix — validated the reversal empirically: every
  3.0.x corpus spec generates unpatched in seconds, compiles under a
  bounded mechanical workaround set, and 770 wire round-trips found no
  silent lowering defects; six lowering/fork bugs the audit surfaced
  were fixed with upstream goldens intact.)*

- **D16 — rendering separates items with blank lines.** prettyplease
  emits no blank line between items, so generated files read as a wall:
  a struct's closing brace with the next type's doc comment on the very
  next line. Every rendered output document (single-file and folder-tree
  alike — both paths share `render::render_body`) now gets an
  item-spacing post-pass after `polish_rendered`: a single blank line
  between adjacent items, at the top level and inside every inline
  module body, with runs of one-line declarations kept tight
  (consecutive `use` items, consecutive body-less `pub mod x;`
  declarations — so import preambles and the root `mod.rs` keep their
  block shape). Item boundaries come from span line numbers of the
  re-parsed source (proc-macro2 `span-locations`), which start at an
  item's doc/attribute stack, so docs move with their item; insertions
  are applied bottom-up by line index. Same safety posture as the D14
  polish: the pass is whitespace-only and token-verified — the spaced
  output must re-parse to the identical token stream or the input is
  returned unchanged. This is a deliberate output change for every emit
  style; all goldens and the examples workspace's checked-in outputs
  were regenerated (the diff is insertion-only).

- **D17 — rendering normalizes doc comments.** typify carries schema
  descriptions as raw `#[doc = "..."]` strings, so prettyplease
  rendered them cramped (`///text`, no space after the slashes), as
  `/** ... */` block comments whenever the description held newlines,
  and at whatever line length the spec author used (sabre has 150+
  column description lines). `render::normalize_docs` — an AST pass
  applied inside `render_body` before `prettyplease::unparse`, so both
  single-file and tree output get it — rewrites the outer name-value
  doc attributes on every item, struct field, and enum variant
  (recursing into inline module bodies): multi-line strings split into
  stacked single-line `#[doc]`s (adjacent doc attributes concatenate
  with newlines in rustdoc — rendering-equivalent), every non-empty
  line gets one normalized leading doc space with its own indentation
  preserved as content, and lines longer than 92 content characters
  soft-wrap at word boundaries (≈100 rendered columns at generated
  module depths; an unbreakable over-long word, e.g. a URL, stands
  alone). Wrapping is per input line — text is never re-flowed across
  the spec's own newlines, so deliberate line structure like
  `` `HALT_ON_ERROR` - … `` item lists survives in the source.
  Splitting alone is not enough for the *rendered* docs, though:
  CommonMark treats a single newline inside a paragraph as a soft
  break and collapses it to a space, so rustdoc/hover would re-flow
  the whole description into one paragraph. The spec's newlines are
  therefore made hard: every original line followed by another
  non-empty original line gets a trailing-backslash hard-break marker
  (blank lines are already paragraph breaks — no markers around them),
  and when a long original line soft-wraps, only the last segment
  inherits the marker; interior wrap points stay soft, keeping the
  distinction between the spec's real breaks and our cosmetic ones.
  The backslash spelling was forced: CommonMark's two-trailing-space
  hard break does not survive prettyplease, which unconditionally
  trims trailing spaces when printing `#[doc]` as `///`
  (`trim_trailing_spaces` in its `attr.rs`) — and invisible trailing
  whitespace would be fragile under editors and diff tooling anyway;
  an existing end-of-line backslash is folded into the marker, never
  doubled into a `\\` escape. Two exemptions: doc blocks
  containing a fenced code line (```` ``` ````) pass through
  byte-untouched — the `with_schema_in_docs` `<details>` sections
  depend on exact zero-leading-space alignment (pinned by
  `examples/custom_pipeline.rs`) — and non-name-value forms
  (`#[doc(hidden)]`) plus inner attributes are left alone. Unlike the
  D16 spacing pass this deliberately changes tokens, so there is no
  token-identity gate; the pass is pinned by unit tests (splitting,
  spacing, indentation preservation, wrap boundaries, hard-break
  placement — between non-empty lines, absent around blank lines, last
  wrap segment only — fence skip, field/variant coverage) plus an
  idempotence test, and the condensed style's quote-built docs
  (support module, `impl_string_enum` definition) are already in
  normalized shape, so the D14 pins hold with zero edits. All goldens
  and the examples workspace regenerated.

- **D18 — custom type mappings are config keys over fork/upstream
  knobs.** Two additions to the style surface. (1) `[style] formats`
  maps `"<instance-type>/<format>"` keys to Rust type paths
  (`"string/decimal" = "::rust_decimal::Decimal"`), backed by a new
  fork knob `with_format_type(instance_type, format, rust_type)` that
  generalizes the three dedicated date/date-time/uuid overrides:
  string (known and unknown formats), integer, and number conversions
  all consult one resolution point (`TypeSpaceSettings::format_type`),
  where the generic map wins and the `date`/`date-time`/`uuid` keys
  act as sugar for their `string/…` entries. The fork addition is
  additive and rebase-friendly — with no entries set, output and
  upstream test goldens are byte-unchanged. (2) `[types."Name"]
  replace = "::path::To::Type"` (plus optional kebab-case
  `replace-impls`) maps a named schema onto an existing Rust type via
  upstream typify's `with_replacement` — deliberately not reinvented.
  Both mapped paths are emitted verbatim and assumed to implement
  `Serialize`/`Deserialize` for the wire shape. Validation: a replaced
  schema generates no AST item, so `replace` selectors are exempt from
  the AST-match requirement and instead verified by the replacement
  path appearing in the output tokens (a replace nothing references is
  a hard error); combining `replace` with `patch`/`derives-add`/
  `module` is a config error (nothing is generated to patch, derive
  on, or place), as is `replace-impls` without `replace` and a
  malformed `formats` key; fields holding a replaced type never get
  deep-patch annotations (typify only annotates generated struct
  entries — pinned by test). No preset uses the new keys, so all
  goldens are byte-unchanged.

- **D19 — mappings carry field attributes and capabilities; output can
  be compile-gated.** Mapping `string/date-time` onto
  `::time::OffsetDateTime` (D18) left two hand-edit gaps: every field
  needed a `#[serde(with = "time::serde::iso8601[::option]")]`
  attribute, and structs deriving `Default` with a *required*
  date-time no longer compiled (`OffsetDateTime: Default` doesn't
  hold). Three additions, all config-driven and applied at the AST
  level (`src/mappings.rs`):
  (1) **table-form mappings** — `[style.formats."string/date-time"]`
  with `type`, `field-attrs`, `optional-field-attrs`, and `impls`
  (bare-string shorthand still works and declares no capabilities);
  `[types] replace` gets the same `field-attrs`/`optional-field-attrs`
  and its `replace-impls` adopts the shared `Capability` vocabulary.
  Attributes parse into `syn::Attribute`s (hard error naming the
  config key) and append to every struct field whose type — unwrapping
  `Option`/`Option<Box<...>>`, full-path comparison tolerating a
  leading `::` — is the mapped type; required and optional lists never
  substitute for each other (a `serde(with = ...)` module for `T`
  cannot handle `Option<T>`; the examples workspace also documents
  that optional fields need `serde(default, ...)` to keep missing keys
  deserializing as `None`). `Vec<T>` fields are out of scope for
  attributes.
  (2) **capability-aware derives** — `impls` declares what the
  external type provides (serde + `Debug`/`Clone` always assumed;
  unknown names are hard errors). An analysis over the generated item
  graph prunes `Default`/`PartialEq`/`Eq`/`Hash`/`Ord` derives the
  mapped types cannot satisfy, to a fixpoint: `Default` is constrained
  only by unwrapped required fields (`Option`/`Vec`/maps default to
  empty), the equality family by every field including wrapped ones;
  payload-enum `Default` *synthesis* (the `untagged-enum-defaults`
  pass) participates as a node — a first-variant payload that lost
  `Default` skips synthesis — and deep-patch annotations on fields of
  a type that lost `Default` are dropped (struct_patch's
  none-as-default merge materializes `T::default()`). Patch companions
  keep `Default` (their fields are all `Option`) but share the
  equality-family pruning. Every removal warns on stderr with the
  causing field/chain.
  (3) **opt-in compile gate** — `--verify` / `Generator::verify_compile`
  / `[verify] enabled`: the output is `cargo check`-ed in a scratch
  crate (edition 2024; serde/serde_with/struct-patch defaults plus
  `[verify] dependencies` raw lines, user lines winning on name
  collisions) *before* any file is written; failure carries the rustc
  output and keeps the scratch crate. Both single-file and tree modes
  are covered; the gate requires resolvable dependencies and the
  user's cargo. The examples workspace's `via-cli-config` is the
  living end-to-end: its sabre config uses the full table form, its
  regen script passes `--verify`, and the previously hand-edited
  `ManualApproval` now regenerates byte-equivalent (serde `with`
  attrs, no `Default` derive) and compiles. Presets are untouched, so
  openapi-codegen goldens are byte-unchanged.

- **D20 — field-scoped divergence is a three-tier layer cake.**
  Mapping defaults (D19) are per *type*; real configs diverge per
  *field*. Two tiers land above the mappings, resolved to one
  effective decision per generated struct field (`src/rules.rs`) that
  flows through the existing application machinery in `mappings.rs` —
  no second application path. (1) `[fields."Type.field"]` grows
  `field-attrs` (`Option<Vec<...>>`: present **replaces** every
  mapping/rule attr for that field, `[]` clears, absent inherits — the
  field's optionality is known here, so one list) and its `type` key
  gains a table form (`type = { type = "...", field-attrs = [...],
  impls = [...] }`, bare string still parsing via an untagged enum)
  whose `impls` joins the D19 capability fixpoint — a required field
  overridden to a no-`default` type strips `Default` from its owner
  transitively, same warning format. (2) `[[rules]]`: ordered
  pattern-scoped overrides between mappings and `[fields]`, evaluated
  per field in declaration order (later rules override key-by-key;
  `[fields]` beats all; a rule-applied `type` also strips the field's
  deep-patch annotation, which named the displaced type's companion).
  Predicates (ANDed, ≥1 required; hand-rolled `*`/`?` globs, no
  character classes): `module` (partition path + `[types] module`
  overrides; requires partitioning — hard error otherwise), `struct` /
  `field` (schema-key-or-Rust-name / wire-or-Rust-name), `format`
  (spec provenance: named schemas' direct and `allOf`-branch
  properties, one `$ref` hop to a named schema; `"type/format"` or
  bare `"type"`; inline/anonymous sub-schemas carry none and never
  match), `type` (resolved Rust type in the AST, `Option`/`Box`
  unwrapped). Timing: `deep-patch` payloads feed the fork's
  generation-time `with_deep_patch_filter`, so they may only use the
  pre-generation predicates — combining with the `type` predicate is a
  hard error; the rules tier resolves in `LoadedSpec::lower` (typed
  spec model + partition both in hand), deep-patch decisions install
  as an augmented filter in `build_types`, and attr/type payloads
  apply post-generation where every predicate is checkable. A rule
  matching zero fields **warns** (stderr, rule index + predicates)
  instead of erroring — globs are broad-brush, unlike the exact
  `[types]`/`[fields]` selectors. The examples workspace demonstrates
  a `module = "*/response"` / `deep-patch = false` rule (response
  envelopes are read-only; request-side annotations survive). Presets
  untouched; goldens byte-unchanged.

- **D21 — the verify gate declares what generated code actually
  references, and rules gain the type-level `patch` payload.** Driven
  by the second real-world spec (BFM): its verify run failed with 87×
  E0433 (`serde_json` — free-form schemas generate
  `::serde_json::Map`/`Value`, which the scratch crate didn't declare)
  and 792 `unexpected_cfgs` warnings (the cfg-gated schemars derives
  name a feature the scratch crate didn't have). Gate fixes
  (`src/verify.rs`): the scratch manifest declares an empty
  `schemars = []` feature (registering the cfg name; checks run with
  it off — cleaner than a lints table because the feature genuinely
  exists in consumer crates); well-known crates are auto-declared when
  and only when the rendered output references their marker
  (`::chrono` + serde, `::uuid::` + serde, `::serde_json`,
  `::regress`), with user `[verify] dependencies` lines still winning
  name collisions; and unresolved-crate failures (E0433 / undeclared
  crate / can't-find-crate) append a targeted hint naming the missing
  crates and the `dependencies = ['...']` syntax on top of the full
  rustc output. Arbitrary mapped-type crates still need explicit
  declaration — the gate cannot guess versions or features. Separately,
  `[[rules]] apply` gains `patch = true|false`, a *type-level* payload
  reusing the D13 per-type patchability machinery: it strips/keeps the
  whole struct_patch surface, evaluates pre-generation over named
  schemas (module/struct predicates only — `field`/`format`/`type`
  alongside `patch` is a hard error, as is mixing `patch` with
  field-level payload keys in one rule; rules stay single-scope), and
  layers as baseline → rules in order → exact `[types."X"] patch`
  (which wins). Patchability decisions install into the override
  plan's `is_patchable` in `lower()` and the deep-patch filter
  re-installs augmented in `build_types`, so cross-type annotation
  pruning into de-patched types keeps working.   The examples
  workspace's response-module rule upgraded from `deep-patch = false`
  to the stronger `patch = false` (response envelopes lose the entire
  patch surface), and the BFM generation is wired end-to-end through
  the gate.

- **D22 — the client emitter is a concrete reqwest-middleware client
  with generated auth, a typed hook seam, and marker-based
  ejection.** Step 6 lands as pure openapi-codegen (`src/client/`,
  zero fork changes): `[client] enabled` / `--client` /
  `Generator::client(true)` append a `pub mod client` — `Client`,
  `ClientBuilder`, one async method per operation, `auth` and
  `support` submodules — emitted via quote! into the normal AST →
  render pipeline so doc normalization, item spacing, and
  prettyplease apply for free (the spacing pass learned to separate
  impl/trait members; the pre-existing `type Err` + `fn from_str`
  ladders stay tight, pinned by the untouched goldens). The data
  path is the one D8 reserved: `Spec::operations` (now joined by
  `servers` and opaque `$ref`-parameter records) carries through
  `LoweredSchema` into `build_types`, where `ClientPlan::resolve`
  validates against the populated TypeSpace — the one point where
  spec model, resolved style, user-edited partition, and generated
  names all exist — so every v1 boundary fails loudly with its
  Origin before rendering: JSON bodies only, `$ref` body schemas
  only (inline errors hint "patch it into components.schemas"), one
  success schema per operation, scalar inline params only. Shape
  decisions, made with the consumer: **concrete client only** (no
  trait layer, no mockall — wiremock covers testing);
  **reqwest-middleware** for the pipeline seam (retries/tracing
  compose as middleware, `From<reqwest::Client>` keeps the plain
  case free); **auth generated from securitySchemes** (`NoAuth` /
  `StaticBearer` always; `BasicAuth` / `ApiKey` as declared;
  `OAuth2ClientCredentials` reproducing the hand-written Sabre
  client: form-encoded client-credentials grant, double-base64
  `Basic` header under `x-base64-encode-client-credentials`, TTL
  cache on `std::sync::Mutex` + `Instant` with the guard never held
  across an await — a racing double-fetch is accepted over
  serializing calls; no tokio in generated code), with spec'd auth
  header params folded out of signatures (`suppress-auth-headers`);
  **the body-hook seam** (`serde_json::Value` between serialization
  and send, `OperationInfo`-aware) so cross-cutting request edits
  like fill-if-unset PCC are a three-line `ext/` closure instead of
  a per-type trait; and **the text-first response path** (non-2xx →
  `Error::Status` with raw body; 2xx decoded from text so
  `Error::Decode` carries serde line/column plus the payload).
  Ownership is marker-based and general: the tree writer now skips
  (never overwrites, never deletes) any generated-path file whose
  first line lacks `// @generated`, the `eject` subcommand rewrites
  a generated header to `// @ejected — was generated from <spec>;
  delete this file and regenerate to restore.`, and `ext/mod.rs` is
  scaffolded once *without* the marker — born ejected, always
  declared from the generated root, a directory so `pcc.rs` and
  friends grow beside it untouched. openapi-codegen itself takes no
  reqwest/tokio dependencies: client goldens are pinned as text
  (petstore single-file, sabre split tree with ext), the verify gate
  auto-declares reqwest / reqwest-middleware / async-trait / base64
  off output markers (D21 mechanism), and real compilation + wire
  behavior live in the examples workspace's `via-cli-client` crate —
  wiremock pins one token fetch across two calls, the ext/ PCC hook
  filling `targetPcc` without clobbering a caller's value, and both
  error surfaces.

- **D23 — wire fidelity over spec fidelity: open enums and
  required-ness rewrites (plus the fork's automatic patch-companion
  rename mirror).** A live wire-compat audit of the Sabre consumer
  (hand-written DTO crate vs generated types, driven by real captured
  traffic) surfaced three classes of drift between what specs declare
  and what wires do; each got the architectural fix rather than a
  point patch. (1) *Renamed fields lost their wire key in patch
  companions*: `struct_patch` doesn't carry field serde attrs to the
  `{Type}Patch` companion, so an explicit `#[serde(rename =
  "GetHotelAvailRQ")]` left the companion listening on
  `getHotelAvailRq` and silently dropping the caller's entire patch.
  Fixed in the fork, unconditionally: any struct whose derive list
  carries `Patch` mirrors every field-naming serde option as
  `#[patch(attribute(serde(...)))]` — an unmirrored rename is never
  correct, so there is no knob. (2) *Closed enums reject live values*:
  `[style] open-enums = "Other"` (the fork's
  `with_open_string_enums`) gives every plain string enum a trailing
  `#[serde(untagged)] Other(String)` catch-all that round-trips
  undocumented values losslessly; the condensed emit style grew a
  matching `open = Other` invocation clause (second macro arm, open
  Display/irrefutable FromStr). (3) *Specs overstate required-ness*:
  the `[[rules]]` tier gained the pre-generation `optional = true`
  payload, which strips matching properties from the lowered schema's
  `required` lists (module/struct/field/format predicates only, like
  `deep-patch`; later `optional = false` restates the spec for a
  narrower match). Consumers point these at request modules where
  the server tolerates omission and at response fields observed
  absent on the live wire.

## Results (2026-07-03)

- **Parity gate: green, byte-identical** — not merely token-identical —
  for both fixtures × both engines across all four output modes (flat,
  partitioned, split single-file, split folder-tree), verified by
  `tests/parity.rs` (8 engine-vs-engine tests + 4 golden-fence tests)
  and by external `diff` against the checked-in goldens. The
  deliberate-improvements list is **empty**: no golden changed. (Still
  true after D14: the condensed emit style is opt-in and off the parity
  path; `tests/emit_style.rs` gives it its own golden fence —
  `examples/generated_tree/petstore_condensed/` — plus emission-shape
  and type-token-equivalence checks.)
- **Unit coverage for fixture-silent semantics** (`tests/ir_unit.rs`,
  15 tests): untagged oneOf enums + Default synthesis, anyOf-null →
  `Option`, `nullable` wrapping, self/mutual cycle boxing, inline-schema
  naming, collision errors, merge fallback, singleton-allOf aliasing,
  named-scalar inlining, config plumbing end to end.
- **Step-1 guard green**: the typed `Spec` render is `Value`-identical
  to the legacy lowering on both fixtures (`tests/spec_model.rs`), and
  the golden fence pins the final Rust bytes.
- **Consumers**: `openapi-codegen-examples` workspace tests pass; the
  five in-repo examples pass; clippy is clean.
- **The typify fork needed zero changes** during the migration; its
  `ir-migration` branch is an empty placeholder over the freeze tag.
- **What remains before the client emitter (step 6)**: operations are
  captured in `Spec`/`Ir` as data but nothing consumes them — step 5
  (partition from IR roles, inline request/response naming, deletion of
  the raw-`Value` BFS) comes first; auth schemes are parsed but untyped;
  `examples`/`discriminator` are preserved but unused (doc/test
  synthesis and discriminator-aware oneOf mapping are open).
  *(Updated 2026-07-05: step 6 landed as D22 — operations and auth
  schemes are consumed by the client emitter; the partition still
  walks the raw document, and `examples`/`discriminator` remain
  open.)*

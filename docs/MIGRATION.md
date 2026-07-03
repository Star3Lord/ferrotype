# Migration: typify-fork engine → owned Spec → IR → passes → emitter

**Status:** in progress (living document; decision log at the bottom)
**Date started:** 2026-07-03
**Branches:** `ir-migration` in this repo and in `../typify` (fork frozen at
tag `fork-freeze-20260703`)
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
  optionality, serde surface, derives/attrs, impl synthesis, deep patch,
  partition, imports).
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

## Results

(Filled in at the gate; see the final report.)

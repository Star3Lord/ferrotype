# IR engine spec-coverage audit

**Question audited:** *Can the IR engine handle all OpenAPI specs and produce
correct types that work with the API served using those specs?*

**Short answer:** No — not "all", and not unpatched. The IR engine is a
faithful, fast, loud-by-design compiler for the dialect the fixtures speak.
Outside that dialect it fails loudly on constructs that appear in essentially
every large public spec (non-string enums alone block 5 of the 7 specs
tested), and it has a small but real set of **silently-wrong** lowerings
(discriminator-less untagged dispatch, empty structs for named free-form
objects, dropped `additionalProperties` overflow, dropped `oneOf` sibling
properties, dropped nested-`allOf` fields, `uint64 → i64`) where output
compiles but does not faithfully speak the wire protocol. The details,
evidence, and a prioritized close-the-gap list follow.

- **Audit date:** 2026-07-03
- **Engine commit:** `4f5c0f2e92fb44b4e8189744e2efaaa65b701c27` (branch
  `ir-migration`, working tree clean)
- **typify fork baseline:** `../typify` @ `fork-freeze-20260703`
  (`984268b1970424747577f672b98ac8fef30dd456`)
- **Repo test suite at this commit:** 60/60 green (`cargo test --release`),
  including the 12-test parity gate and the 15 `ir_unit` semantics pins.
- **Scratch area:** `/tmp/spec-audit` (downloaded specs, generated output,
  compile-check crates, minimized repros). Nothing under the repo was
  modified except this document.

Reproduction commands used throughout (release build of this repo):

```bash
# IR engine (the subject)
cargo run --release -- generate --spec <spec> --engine ir \
    --profile api-client --output <out.rs> [--partition-by-operation]

# typify engine (the incumbent, for comparison)
cargo run --release -- generate --spec <spec> \
    --profile api-client --output <out.rs>
```

Compile checks used a scratch crate pinning the consumer dependencies the
same way this repo's `Cargo.toml` does: `serde 1` (derive), `serde_json 1`,
`serde_with 3`, `struct-patch 0.10` (default-features off; `status`, `op`,
`nesting`, `none_as_default`). `cargo check` was the bar. Wire-behavior
claims below were verified with actual `serde_json` round-trip tests
against the generated code (7 tests, all reproducing the claimed behavior).

---

## Part 1 — static coverage matrix

Derived from reading `src/spec/` (typed `Value → Spec` normalization) and
`src/ir/lower.rs` / `src/ir/passes.rs` (schema → IR semantics), then
confirmed with 30+ minimal probe specs. Four categories:

- **✅ supported** — lowered with correct wire semantics
- **⚠️ partial** — works with a specific caveat
- **⛔ loud error** — generation fails with an `Origin`-carrying message
  (the documented escape is the RFC 6902 patch mechanism)
- **🔥 silent-wrong** — generation succeeds but output is lossy or broken
  on the wire (the dangerous category)

### Document / dialect level

| Construct | Status | Notes |
|---|---|---|
| OpenAPI 3.0.x | ✅ | The primary dialect. |
| Swagger 2.0 | ⛔ | Version gate: `unsupported OpenAPI version "2.0"; convert … first`. Verified on the raw Docker Engine v1.47 spec. |
| OpenAPI 3.1 | ⚠️ | **Accepted, not rejected** (docs call it a "seam"). `type: [T, "null"]` folds to nullable ✅; `type: ["null"]` → `()` ✅; numeric `exclusiveMin/Max` ✅; `examples` arrays ✅. But `const` and `prefixItems` fall into the `extra` catch-all and are **🔥 silently ignored** (see below), and root `webhooks` are not parsed at all (types under `components.schemas` still generate; webhook operations are invisible, and `--partition-by-operation` on a paths-less webhook spec fails ⛔ `missing /paths`). |
| Missing `/components/schemas` | ⛔ | `Spec::from_value` requires it even in flat mode. |
| YAML numbers > `u64::MAX` | ⛔ | `serde_yaml` loader limitation, **both engines**: DigitalOcean's `maximum: 18446744073709552000` kills the parse. The JSON loader accepts the same number (as `f64`). Front-end asymmetry worth knowing. |
| Vendor extensions (`x-*`) | ✅ | Preserved in `extra`, ignored by the IR engine (benign). |

### `$ref` forms

| Construct | Status | Notes |
|---|---|---|
| `#/components/schemas/<name>` | ✅ | Including RFC 6901 `~0`/`~1` escapes in the key. |
| `$ref` with annotation siblings | ✅ | Siblings ignored per 3.0 semantics; sibling `nullable: true` honored (Option-wrapped). |
| Non-schema refs (`#/components/responses/…`, `#/$defs/…`) in schema position | ⛔ | `unsupported $ref … (only #/components/schemas/)`. |
| External-file / URL refs | ⛔ | Same error. No resolver exists. |
| Ref-shaped **operation parameters** (`$ref` to `components.parameters`) | ⚠️ | Tolerated and skipped (operations are data-only today, D8). |
| Pure alias cycles (`A: {$ref: B}`, `B: {$ref: A}`) | ⛔ | Correctly loud — no type exists. |
| **Self-referential named array/map schemas** (`Trace: {type: array, items: {… $ref: Trace}}`) | ⛔ *(false positive)* | Classified as an alias, so classification re-enters itself and reports "alias cycle" even though `Vec` provides indirection. Kills Cloudflare unpatched (`request-tracer_trace`). typify handles this shape. |

### Composition

| Construct | Status | Notes |
|---|---|---|
| `allOf: [single]` (± `description`) | ✅ | Collapses into the subschema under the outer name (typify's singleton rule). |
| `allOf` Compose: `$ref` bases + inline objects + sibling props | ✅ | `#[serde(flatten)]` base fields; Swagger-conversion sibling-property pattern covered; pinned by parity fixtures. |
| `allOf` merge fallback (no `$ref` base) | ⚠️ | Property/required union. Colliding property names must be **exactly identical draft-07 renders — descriptions included**. A description-only difference is ⛔. On GitHub this single rule accounts for **226 of the 228 patches** needed (e.g. `webhook-fork.properties.forkee`: two inline members both define `allow_forking`, one with a `description`, one without). typify union-merges these fine. |
| `allOf` member that resolves to a non-object (`oneOf` ref, enum-refinement, bare constraint on a scalar ref) | ⛔ | `allOf merge requires object subschemas`. Cloudflare uses `allOf: [$ref-to-string-enum-schema, {enum: [add]}]` as an enum-refinement idiom — 69 occurrences had to be patched away. |
| **Nested `allOf` inside a merge branch** | 🔥 | A merge member that itself carries `allOf` + `properties` contributes only its own `properties`; the nested bases' fields are **silently dropped**. Verified: probe below generates `Merged {other, top}` with `nested` gone (typify keeps it). |
| **Compose with a scalar `$ref` base** (`allOf: [$ref-to-string, {description}]`) | 🔥 | Produces `#[serde(flatten)] pub id: String` — compiles, but **every serialize/deserialize fails at runtime** ("can only flatten structs and maps"). Verified by wire test. |

```jsonc
// merge_nested_allof — the `nested` field silently vanishes
"Merged": {"allOf": [
  {"type": "object", "properties": {"top": {"type": "string"}},
   "allOf": [{"type": "object", "properties": {"nested": {"type": "string"}}}]},
  {"type": "object", "properties": {"other": {"type": "integer"}}}]}

// allof_scalar_ref — compiles; serde errors at runtime on every use
"Id": {"type": "string"},
"Thing": {"allOf": [{"$ref": "#/components/schemas/Id"}, {"description": "x"}]}
```

### `oneOf` / `anyOf`

| Construct | Status | Notes |
|---|---|---|
| `oneOf`/`anyOf` → untagged enum | ⚠️ | `#[serde(untagged)]`, variants named from `title` or `Variant{i}`. Deserialization is **first-match-wins**; see the discriminator row and Part 3 for why this is the biggest wire-correctness risk. |
| `[T, {type: null}]` (either order) | ✅ | `Option<T>`, at named and use-site positions; 3.0 `nullable` normalizes into the same path. |
| Singleton `anyOf`/`oneOf` (`anyOf: [{$ref: X}]`) | ⛔ *(indirectly)* | **Not collapsed** (only `allOf` singletons are). Mints a one-variant untagged enum named `{Parent}{Property}` — which then collides ⛔ with real schemas named `parent_property`. This is why unpatched Stripe fails (`account.business_profile` vs `account_business_profile`): 412 singleton-`anyOf` collapses were needed. typify collapses these. |
| **`discriminator` (± `mapping`)** | 🔥 | Parsed and preserved in the model (D7) but **consumed by nothing**: output is a plain untagged enum. Wire test proves the misdispatch: `{"petType": "dog", "barks": true}` parses as the `Cat` variant (first match; every non-required field is `Option` under `api-client`), silently dropping `barks` on round-trip. |
| **Sibling `properties` next to `oneOf`/`anyOf`** | 🔥 | The enum wins; sibling properties are **silently dropped** (JSON Schema says both apply). Verified: `{"a": "x", "common": "y"}` round-trips without `common`. typify distributes the shared property into each variant. |
| Untagged variants with duplicate/missing titles | ⛔ | Loud (add titles via patch). |

### Enums

| Construct | Status | Notes |
|---|---|---|
| String enums | ✅ | typify's exact variant naming incl. the `X`-substitution collision fallback; `+1`/`-1` special cases (GitHub reactions) verified. |
| Schema-level `default` on an enum | ✅ | Selects the `Default` variant; a default **not in the values list** is ⛔ (Cloudflare has one such spec bug). |
| **`enum` containing `null`** | ⛔ | `enum value null … is not a string`. This is the single most frequent blocker in the corpus: GitHub 78, Cloudflare 214 (+129 enums whose *only* value is null). typify prunes the null. |
| Integer / mixed / boolean enums | ⛔ | `only string enums are modeled`. Corpus: Cloudflare 967, Stripe 24, DigitalOcean 5, GitHub 4, Docker 1. |
| Symbol-only enum values (`"<", "<=", …`) | ⛔ | Do not sanitize to unique Rust names even with the fallback (Cloudflare: 12). |
| `const` (3.1) | 🔥 | Lands in `extra`, silently ignored: `{const: "user"}` → `Option<serde_json::Value>`; `{type: string, const: "admin"}` → plain `String`. No single-value enforcement, no doc note. (typify via the draft-07 bridge is equally lossy here.) |

### Objects, maps, arrays

| Construct | Status | Notes |
|---|---|---|
| Plain object schemas → structs | ✅ | BTreeMap-ordered fields, wire-name renames, required→bare / optional→`Option` (`api-client`). |
| `required` vs optional | ✅ | Faithful; flatten bases always required. |
| `additionalProperties: false` | ⚠️ | `#[serde(deny_unknown_fields)]` — spec-faithful but brittle against a served API that adds fields (deserialization hard-fails). Same on typify. |
| Pure map (`additionalProperties: <schema>`, no props) | ✅ | `HashMap<String, V>` at named and inline positions. |
| Inline `{type: object}` (free-form) | ✅ | `HashMap<String, serde_json::Value>`. |
| **Named `{type: object}` (free-form)** | 🔥 | Becomes **`pub struct X {}`** — deserializes anything, serializes `{}`; total silent data loss on round-trip. Inconsistent with the inline case. typify emits a transparent newtype over `serde_json::Map`. Verified by wire test. |
| **`properties` + `additionalProperties: <schema>`** | 🔥 | The overflow map is **silently dropped** — unknown keys are ignored on read and lost on write. typify emits a `#[serde(flatten)] extra: HashMap<…>` field. Verified by wire test. |
| `patternProperties` / `propertyNames` | ⛔ | Loud unsupported-keyword errors (0 occurrences in the tested corpus). |
| `items` (single schema) | ✅ | `Vec<T>`; inline item types named `{Parent}Item`. |
| Tuple `items: [...]` (3.0/draft-07) | ⛔ | Loud (GitHub has 8, DigitalOcean 1). |
| `prefixItems` (3.1) | 🔥 | Silently ignored via `extra` → array lowers to `Vec<serde_json::Value>`; tuple structure erased. |
| `not` / `if` / `then` / `else` / `contains` / `dependencies` / `additionalItems` / `definitions` | ⛔ | Loud. Reassuringly rare: **zero occurrences across all 7 corpus specs**. |
| Boolean schemas (`true`/`false`) | ⛔ | Loud (`patch the spec to an explicit object schema`). |

### Scalars, formats, bounds

| Construct | Status | Notes |
|---|---|---|
| `nullable: true` (3.0) | ✅ | `Option<T>` everywhere, incl. `$ref` siblings; nullable + otherwise-empty node stays fully permissive (historical behavior). |
| Required **and** nullable | ⚠️ | Lowers to `Option<T>`; under `api-client`'s `skip_serializing_none`, `None` serializes as *absent*, not `null` — a server that validates `required` rejects it. Wire-tested. Both engines share this profile semantics. |
| `format: int32` | ✅ | `i32`. |
| `format: int64` / no format | ✅ | `i64`. |
| **`format: uint64` / `uint32` / `uint16` / `uint8` / `int8` / `int16`** | 🔥 | All collapse to `i64`. `u64::MAX` on the wire fails deserialization at runtime; wire-tested (`18446744073709551615` errors on IR output, parses as `u64` on typify's, which maps the full `int8…uint64` ladder). DigitalOcean uses `uint64` 44×, Docker 9×. |
| `number` (any format) | ⚠️ | Always `f64` (typify maps `float` → `f32`; wire-compatible either way). |
| `type` inferred from bare `format` | ✅ | `int32/int64`→integer, `float/double`→number, anything else→string — the historical inference, ported. |
| `date`/`date-time`/`uuid` formats | ✅ | Style-config override points (`api-client` → plain `String`; defaults chrono/uuid). Unknown formats (`binary`, `byte`, `CIDR`, `dateTime`, …) → `String` ✅ (wire-safe). |
| Numeric bounds, `multipleOf`, `pattern`, length/item/property counts | ⚠️ | Parsed, preserved, **deliberately unenforced** (`Plain` constraint mode; `validate` is a loud D4 error). Matches the `api-client` recipe on typify. Numeric-form exclusive bounds survive normalization (D10 fixed the old deletion bug). |
| `default` on fields | ⚠️ | Carried as data; under always-`Option` it does not change the wire shape. Enum defaults do (Default impl). |
| `readOnly` / `writeOnly` / `deprecated` | ⚠️ | Silently ignored (both engines): one type serves both directions, no `#[deprecated]`. Benign for round-tripping, lossy for direction-aware codegen. |
| `example` / `examples` / `discriminator` | ⚠️ | Preserved in the model (D7), consumed by nothing yet. |
| `xml` / `externalDocs` | dropped | Documented D7 drop, matching the historical lowering. |

### Naming, cycles, operations

| Construct | Status | Notes |
|---|---|---|
| Unicode / keyword / degenerate identifiers | ✅ | `héllo` passes through (Rust unicode idents), keywords get `_` suffix (`type_`… — verified `type`, `self`, `match`), `+1`/`-1` → `plus1`/`minus1`, empty → `X`-prefix. |
| Schema-key collisions (`useCSL` vs `useCsl`) | ⛔ | Loud with both origins (D11; typify silently reuses the first — arguably worse). |
| **Inline-synthetic vs named-schema collisions** | ⛔ | The `{Parent}{Property}` synthetic name collides with real schemas named `parent_property` — a *systemic* pattern in GitHub/Stripe/DigitalOcean/Cloudflare naming conventions. Every large corpus spec needed renames (Stripe 29, Cloudflare 27, GitHub 2, DO 1). |
| **`{Name}Patch` companion collisions** | 🔥→⛔ *(at consumer compile time)* | The D11 collision check does not cover the `struct_patch`-derived `{Name}Patch` companions. Cloudflare has sibling schemas `X` and `X_patch`; output **compiles-fails** with ~104 errors (`E0428` duplicate definitions, `E0119` conflicting impls). Same hazard exists on the typify engine (same naming mechanism). |
| Reference cycles (self / mutual struct refs) | ✅ | SCC-based `Box`ing (broader than typify's minimal boxing but sound); `Vec`/`Map` edges correctly unboxed. |
| **Deep-patch through `Box` on recursive types** | 🔥→⛔ *(at consumer compile time)* | `#[patch(name = "Option<XPatch>")]` through a boxed cycle makes `XPatch` an infinite-size type (`E0072`). **Both engines** (typify's Stripe output fails identically: `ApiErrorsPatch`/`PaymentIntentPatch`). IR's broader boxing surfaces it more often (15 boxed deep-patch edges on Stripe vs typify's 5). Workaround verified: `[style] patch = false` → both Stripe and Cloudflare IR outputs compile clean. |
| Operations (params/requestBody/responses) | ⚠️ | Parsed into `Spec`/`Ir` as data (D8); **nothing consumes them**. Parameter and body schemas *are* validated — an unsupported construct inside a parameter fails the run even in flat mode. Path-level (shared) `parameters` and `trace` operations are not captured; `callbacks` are ignored entirely (their component schemas still generate; partition marks them unreachable → `shared`). |
| `--partition-by-operation` | ⚠️ | Requires `operationId` on **every** operation; Cloudflare (41 missing) is forced into flat mode. Paths-less specs (3.1 webhooks-only) fail partitioned mode. |
| Config surface | ✅ | `optional-fields = "bare"`, `constrained-strings/integers = "validate"` and `--engine ir --profile typify` are loud, documented errors (D3/D4) — verified. Unmatched `[types]`/`[fields]` selectors are loud (pinned by tests). |

---

## Part 2 — empirical runs on real-world specs

### Corpus

Seven structurally diverse public specs, downloaded 2026-07-03 into
`/tmp/spec-audit/specs/`:

| Spec | Version | Size | Schemas | Ops | Traits |
|---|---|---|---|---|---|
| GitHub `api.github.com` | 3.0.3 | 12.6 MB | 951 | 1,194 | 3,859 `nullable`, 283 `oneOf`+`anyOf`, 82 non-string-enum hits, 8 tuple-`items`, 5 `discriminator` |
| Stripe `spec3.json` | 3.0.0 | 7.9 MB | 1,431 | 587 | 2,002 `anyOf` (expandables), 4,118 `enum`, 923 `additionalProperties` |
| DigitalOcean public v2 | 3.0.0 | 3.1 MB | 883 | 663 | 257 `allOf`, 13 `discriminator`, 44 `uint64`, YAML big-int |
| Cloudflare `api-schemas` | 3.0.3 | 10.8 MB | 6,251 | 3,243 (41 no `operationId`) | 3,600 `allOf`, 1,203 non-string-enum hits, 49 `discriminator` |
| Docker Engine v1.47 (2.0 → 3.0 via swagger-converter) | 3.0.1 | 0.36 MB | 117 | 107 | conversion-shaped `allOf`/`nullable`, `uint8/16/32/64` |
| Redocly Museum | 3.1.0 | 23 KB | 22 | 8 + 1 webhook | deliberate 3.1 |
| OAI webhook-example | 3.1.0 | 1 KB | 1 | 0 paths | webhooks-only, adversarial for partitioning |

### Method

1. Run **both engines** unpatched; record the first failure verbatim.
2. Apply *mechanical* patches mimicking the documented RFC 6902 escape
   hatch (scripted; each rewrite counted) until IR generation succeeds or
   an unpatchable error appears — this measures the **patch burden**.
3. `cargo check` every generated artifact in a pinned scratch crate.
4. Round-trip wire tests for every suspected silent-wrong lowering.
5. Time everything (release binary, Apple-silicon laptop).

### Results

| Spec | IR unpatched | typify unpatched | Patch burden → IR success | IR gen time / output | IR output compiles? |
|---|---|---|---|---|---|
| GitHub | ⛔ `enum value null at …/check-suite/properties/conclusion` | ✅ 0.8 s, 91,919 ln | **315** (78 null-enums, 4 int-enums, 5 singleton-`oneOf`, 2 collision renames, **226 description-only merge conflicts**) | 1.1 s flat / 1.2 s partitioned; 266,576 ln | ✅ |
| Stripe | ⛔ collision `AccountBusinessProfile` (inline `anyOf`-singleton vs named schema) | ✅ 0.6 s, 128,178 ln | **465** (24 int-enums, 412 singleton-`anyOf`, 29 collision renames) | 0.5 s; 127,174 ln | ⛔ with patch derives (`E0072` recursive `*Patch`); ✅ with `[style] patch = false`. typify output fails the same way. |
| DigitalOcean | ⛔ YAML loader (`maximum: 18446744073709552000` > `u64::MAX`) | ⛔ same loader; after conversion: schemars reject + panic (below) | **6** (+ YAML→JSON convert; 5 int-enums, 1 collision rename) | 0.2 s; 38,944 ln | ✅ (typify cannot generate this spec at all) |
| Cloudflare | ⛔ false alias-cycle on self-referential array `request-tracer_trace` | ⛔ `value does not conform to the given schema` (schemars, no origin info) | **1,619** (214 null-enums, 967 int-enums, 129 null-only enums, 198 singletons, 69 non-object `allOf` members, 27 renames, 12 symbol-enums, 1 array hoist, 1 bad default, 1 field collision) | 0.9 s; 168,234 ln | ⛔ with patch derives (~104 errors: `X` + `X_patch` schemas both emit `XPatch`); ✅ with `patch = false` |
| Docker (converted) | ⛔ integer enum `ChangeType` | ✅ 0.1 s, 9,515 ln | **1** | 0.1 s; 9,274 ln (also `--split-request-response --output-dir` tree: compiles ✅) | ✅ |
| Docker (raw 2.0) | ⛔ version gate (correct, helpful message) | ⛔ same | n/a | n/a | n/a |
| Museum (3.1) | ✅ | ✅ | **0** | <0.1 s; 204 ln (8 types; 14 named scalars correctly alias away; webhook op invisible) | ✅ |
| webhook-example (3.1) | ✅ flat; ⛔ partitioned (`missing /paths`) | same | **0** | <0.1 s; 51 ln | ✅ |

**Performance:** no pathological behavior anywhere — worst case 1.2 s for
the 12.6 MB GitHub spec partitioned into 1,194 operation modules. Both
engines are comparable; drop-in usability is not throughput-limited.

**Where the typify engine fails and IR succeeds** (fairness check —
minimized repros, both fixed shapes handled fine by IR):

```jsonc
// typify bridge: "value does not conform to the given schema" (schemars)
"T": {"type": "object", "nullable": true, "default": null,
      "properties": {"includeUsage": {"type": "boolean"}}}

// typify panic (type_entry.rs:915) — DigitalOcean chat_completion_request.stop
"stop": {"default": null, "oneOf": [
  {"type": "string"},
  {"type": "array", "minItems": 1, "maxItems": 4, "items": {"type": "string"}}]}

// typify panic (convert.rs:807 "not yet implemented") — Cloudflare
{"enum": ["block", "challenge"], "maxLength": 12}
```

The IR engine's error messages are a genuine improvement: every failure
carries a JSON-pointer `Origin`; the typify bridge often dies with
`value does not conform to the given schema` (no location) or a panic.

### Minimized repros for the corpus blockers

```jsonc
// 1. GitHub-class: enum with null entry (78–214 per big spec) — ⛔
"Conclusion": {"type": "string", "enum": ["success", "failure", null], "nullable": true}

// 2. Stripe-class: singleton anyOf synthesizes a colliding type — ⛔
"account": {"type": "object", "properties": {
  "business_profile": {"anyOf": [{"$ref": "#/components/schemas/account_business_profile"}],
                        "nullable": true}}},
"account_business_profile": {"type": "object", "properties": {}}
// → `AccountBusinessProfile` minted twice → loud collision

// 3. Stripe expandables: two-variant anyOf mints {Parent}{Prop} name — ⛔ (collision)
"ownership": {"anyOf": [{"maxLength": 5000, "type": "string"},
               {"$ref": "#/components/schemas/financial_connections.account_ownership"}]}

// 4. Cloudflare: self-referential named array — ⛔ false "alias cycle"
"Trace": {"type": "array", "items": {"type": "object", "properties": {
  "sub": {"$ref": "#/components/schemas/Trace"}}}}

// 5. Cloudflare: allOf enum-refinement over a scalar $ref — ⛔ merge error
"operation": {"allOf": [{"$ref": "#/components/schemas/rulesets_RewriteHeaderOperation"},
                         {"enum": ["add"]}]}   // target: {"type": "string"}

// 6. GitHub: description-only allOf merge conflict (226×) — ⛔
"forkee": {"allOf": [
  {"type": "object", "properties": {"allow_forking": {"description": "…", "type": "boolean"}}},
  {"type": "object", "properties": {"allow_forking": {"type": "boolean"}}}]}
```

### Wire-behavior tests (the silent-wrong evidence)

Seven `serde_json` round-trip tests against generated IR output, all
demonstrating the claimed defect (scratch crate `/tmp/spec-audit/wire-tests`):

1. **Untagged misdispatch**: `{"petType": "dog", "barks": true}` parses as
   the `Cat` variant of a `discriminator`-carrying `oneOf`; `barks` is
   silently dropped on re-serialization.
2. **Flattened scalar base**: `serde_json::to_string(&Thing { id })` fails
   at runtime — "can only flatten structs and maps".
3. **Named free-form object**: `{"tenant": "acme", "flags": [1,2,3]}` →
   `Metadata {}` → re-serializes as `{}`.
4. **`uint64` → `i64`**: `18446744073709551615` fails to deserialize on IR
   output; parses as `u64::MAX` on typify output.
5. **`additionalProperties` overflow dropped**: extra keys vanish on IR
   output; preserved via flattened map on typify output.
6. **`oneOf` sibling properties dropped**: `common` never round-trips.
7. **Required + nullable**: `None` serializes as *absent* (`{}`), not
   `null` — a `required`-validating server rejects it (both engines;
   `api-client` profile semantics).

---

## Part 3 — the verdict

### 1. What "handle" means today

**The proven envelope** (regression-guarded): byte-identical parity with
the frozen typify fork on the two checked-in fixtures (petstore,
sabre-booking) × four output modes, plus 15 unit-pinned semantics for
fixture-silent constructs (untagged enums, Option-via-null, cycle boxing,
inline naming, collision errors, merge fallback, config plumbing) and the
loud D3/D4 mode gates. All 60 tests green at this commit. That envelope is
real but narrow: it is *one dialect* of OpenAPI — 3.0, string enums,
ref-based `allOf` composition, no discriminators, ASCII-ish naming, specs
that name every operation.

**The tested-now envelope** (this audit): of 7 real-world specs, the IR
engine generates **2 unpatched** (the two smallest, both 3.1), and **0 of
the 5 large ones**. With mechanical patches it generates all 5, in ~0.1–1.2 s
even at 12 MB / 6,251 schemas. Patch burden spans three orders of
magnitude: Docker 1, DigitalOcean 6, GitHub 315, Stripe 465, Cloudflare
1,619. Of the five patched outputs, three `cargo check` clean as-is; Stripe
and Cloudflare compile only with `patch = false` (the `struct_patch`
companions break on recursive types and `*_patch` schema names — the
Stripe failure reproduces identically on the typify engine). The typify
engine, for calibration, generates GitHub/Stripe/Docker unpatched but
cannot generate DigitalOcean or Cloudflare at all (schemars rejects /
panics without location info).

### 2. The gap list, ranked by real-world frequency

Corpus counts are exact rewrite counts from the audit scripts.

| # | Gap | Corpus frequency | Category |
|---|---|---|---|
| 1 | Enums with `null` / non-string values | GH 82, Stripe 24, CF 1,310, DO 5, Docker 1 — **blocks 5/7 specs**, first failure on 3 | ⛔ |
| 2 | Untagged-only `oneOf`/`anyOf` (discriminator ignored; first-match dispatch) | 3,997 `oneOf`+`anyOf` sites corpus-wide; 67 explicit `discriminator`s (CF 49, DO 13, GH 5) | 🔥 |
| 3 | Singleton `anyOf`/`oneOf` not collapsed → phantom types + collisions | Stripe 412, CF 198, GH 5 | ⛔ (via collision) |
| 4 | Inline-synthetic `{Parent}{Prop}` names collide with `parent_property` schemas; no dedup/interning of identical inline types (GitHub: 44 copies of one shape; 5,893 types vs typify's 2,565) | renames needed: Stripe 29, CF 27, GH 2, DO 1 | ⛔ + output bloat |
| 5 | `allOf` merge strictness: description-only conflicts ⛔; non-object members ⛔; nested-`allOf` members 🔥 dropped | GH 226 (desc-only), CF 69 (non-object) | ⛔ + 🔥 |
| 6 | `struct_patch` machinery unsound on recursion / `*_patch` names (both engines) | Stripe 15 boxed deep-patch edges, CF 5 colliding pairs | 🔥→compile-fail |
| 7 | Named free-form objects → empty structs; `properties`+`additionalProperties` overflow dropped | free-form/`additionalProperties`: GH 182, Stripe 923, CF 730, Docker 56 sites (subset dangerous) | 🔥 |
| 8 | Unsigned/short integer formats → `i64` | DO 44 `uint64`, Docker 22 `uintN`, CF 14 | 🔥 |
| 9 | Self-referential named arrays → false alias-cycle | CF 1 (first failure) | ⛔ |
| 10 | Tuple `items` / 3.1 `prefixItems` / `const` | GH 8 + DO 1 tuples; corpus 3.1 usage ~0 | ⛔ / 🔥 |
| 11 | `patternProperties`, `not`, `if/then/else`, external refs | **0 in corpus** | ⛔ (cheap to leave loud) |
| 12 | YAML numbers > `u64::MAX` (loader, both engines) | DO 1 (fatal) | ⛔ |

### 3. Will the generated types work against the *served* API?

Even where generation + compilation succeed, four classes of wire-level
risk stand between "compiles" and "works", all evidenced above:

- **Untagged-enum ambiguity is the big one.** Every `oneOf`/`anyOf` is
  first-match-wins, and the `api-client` all-`Option` style makes early
  variants maximally greedy — a discriminated payload deserializes as the
  wrong variant and silently sheds fields on the next serialize. Stripe's
  string-or-object expandables happen to be shape-disjoint (safe); GitHub
  webhook unions and Cloudflare's 49 discriminated unions are not.
- **Silent shape erasure.** Named free-form objects (`pub struct X {}`),
  dropped `additionalProperties` overflow, dropped `oneOf` siblings, and
  dropped nested-`allOf` fields all *round-trip losing data* — the most
  dangerous failure mode for a client that echoes objects back (PATCH/PUT
  flows). The `#[serde(flatten)] String` compose case fails on first use.
- **Integer and strictness edges.** `uint64 → i64` overflows on real
  values (IDs, byte counters — exactly what `uint64` is used for);
  `deny_unknown_fields` on `additionalProperties: false` structs turns any
  server-side field addition into a client-side deserialization failure;
  required-nullable fields serialize as absent rather than `null`.
- **Spec-vs-reality drift.** The repo's patch mechanism (RFC 6902 +
  `op: test` preconditions, or `patch_spec_with` hooks) is the designed
  mitigation and it *does* scale operationally — every blocker in this
  audit was expressible as a patch, and `op: test` guards against silent
  spec-revision drift. But a 1,600-patch burden (Cloudflare) is not
  "drop-in"; it is a porting project. For the two fixtures the mechanism
  is proven (sabre ships a real-world-discrepancy patch).

### To become truly drop-in (prioritized)

1. **Null-tolerant and non-string enums** — prune `null` + set nullable
   (typify parity); integer/mixed enums as typed newtypes or unit-variant
   maps. *(spec model: keep; IR lowering)* — removes the #1 blocker across
   the corpus.
2. **Singleton `anyOf`/`oneOf` collapse** — one-line semantic, kills the
   Stripe/Cloudflare phantom types and most collisions. *(IR lowering)*
3. **Discriminator-driven tagged enums** (`propertyName`/`mapping` →
   internally-tagged serde) and, failing that, variant ordering/disjointness
   checks for untagged. The model already preserves `discriminator`; only
   the lowering and emitter work remain. *(IR lowering + emitter)*
4. **allOf merge robustness** — annotation-insensitive property equality
   (−226 GitHub patches), recursive merge of nested `allOf` members
   (currently silent drops), and scalar-refinement members (enum-over-`$ref`)
   either supported or loud in *compose* too (currently a runtime-broken
   flatten). *(IR lowering)*
5. **Map/free-form fidelity** — named `{type: object}` → map alias, not
   empty struct; `properties` + `additionalProperties` → flattened overflow
   map (typify parity). *(IR lowering)*
6. **Name management** — structural interning of identical inline types
   (GitHub: 5,893 → ~2,565 types, 2.9× output shrink), deterministic
   `{Parent}{Prop}{n}` de-collision instead of hard error, and reserving
   the `{Name}Patch` namespace in the D11 collision check. *(IR lowering +
   passes)*
7. **Integer format ladder** — `int8…uint64` mapping as in the fork.
   *(IR lowering; one match arm)*
8. **Self-referential named arrays/maps** — classify through `Vec`/`Map`
   indirection before declaring an alias cycle. *(IR lowering)*
9. **Patch-machinery soundness on recursion** — skip or `Box` deep-patch
   annotations inside SCCs; this un-breaks Stripe/Cloudflare with
   `patch = true` and fixes the frozen fork's identical latent bug.
   *(passes: `DeepPatchPass`; emitter for companions)*
10. **3.1 completeness** — `const` (single-value enum), `prefixItems`
    (tuple or loud error), root `webhooks` into operations-data. The
    current *silent* `extra`-swallowing of schema-bearing 3.1 keywords
    violates the engine's own loud-errors design rule. *(spec model)*
11. **Loader tolerance** — big-integer YAML numbers (clamp or arbitrary
    precision), matching the JSON path. *(load)*
12. Leave loud (rare in the wild, per the corpus): `patternProperties`,
    `propertyNames`, `not`/`if`/`then`/`else`, external refs, boolean
    schemas, tuple `items` in 3.0.

### Bottom line

For the fixtures and specs shaped like them, the IR engine is a
byte-faithful, better-diagnosed, equally-fast replacement for the frozen
fork, and its loud-error design mostly holds (nothing in the corpus
*parsed* wrong silently — the silent failures are all in *lowering*
semantics). For arbitrary public specs it is not yet drop-in: expect a
loud stop on first contact with ~5 of 7 real specs, a patch burden that
scales with spec size (1 → 1,619 in this corpus), and — after patching —
six known lowering behaviors that compile but do not faithfully speak the
wire protocol, of which untagged-enum dispatch and the two map-erasure
cases are the ones most likely to corrupt real traffic. Items 1–5 above
are the difference between "compiles after patching" and "works against
the served API" for the majority of the tested corpus.

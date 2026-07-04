# Typify-base spec-coverage audit

**Question audited:** *Can the typify-based pipeline handle real-world
OpenAPI specs and produce correct types that speak the wire protocol those
specs describe?*

**Short answer:** Yes, with two qualifications. Every 3.0.x spec in the
corpus — GitHub (12 MB), Stripe (7.5 MB), Plaid, DigitalOcean, plus a 3.1
document — **generates in all three profile/output modes in seconds**, and
after this audit's six pipeline/fork fixes, all of them **compile** either
as-is (museum) or under a small, mechanical, config-expressible workaround
set (2–29 schema renames for name collisions; `patch = false` and
no-`Default`-derive where the spec's shapes require it). Wire round-trips
over 770 example payloads pass at 87–94% per spec, and — the headline —
**every failing pair traces to the spec's own example/schema drift or to
documented profile semantics, not to a silent lowering defect**. No
silent-wrong lowering was found. The residual gaps are name management
(typify's `{Parent}{Prop}` inline naming colliding with real schemas) and
the known deep-patch-recursion limitation — both loud at compile time,
both with mechanical escapes, both quantified below.

- **Audit date:** 2026-07-04
- **openapi-codegen:** branch `typify-base` (this document's commit; the
  audit-driven fixes land immediately before it — see "fixes" below)
- **typify fork:** `../typify` @ `ergonomic-codegen`
  (`6402a63b3e795dd86bac7ca70dbafd8308153002`; four audit-driven commits —
  upstream test goldens byte-unchanged throughout)
- **Repo test suite:** all green, including this audit's harness pins
- **Old audit:** the IR-engine audit that motivated decision D15 is
  preserved as the appendix at the bottom; comparisons below reference it.

## Re-running the audit

```bash
./specs/external/fetch.sh                 # ~26 MB corpus, gitignored
cargo test --release --test real_world -- --ignored --nocapture --test-threads 1
```

`tests/real_world.rs` holds the whole harness: the generation/compile
matrix per spec, the workaround ladder, and the wire round-trip runner
(scratch crates under the system temp dir; failures keep their crate for
inspection). Real captured payloads live in `tests/fixtures/real_world/`
(five live GitHub API responses; six of Stripe's own published fixtures).
Full run ≈ 25 minutes on an Apple-silicon laptop, dominated by scratch
`cargo check`/`run` builds.

---

## Part 1 — the matrix

Generation modes: **ApiClient flat**, **ApiClient split-tree**
(`--split-request-response` planned in memory), **Typify flat** (upstream
semantics through the `Spec` → draft-07 path). Compile: the `--verify`
gate (scratch crate, auto-deps). Workarounds: the documented, mechanical,
config-expressible ladder — RFC-6902-style schema renames for duplicate
Rust names, `[style] patch = false`, dropping `Default` from the derive
lists — applied by the harness exactly as a consumer's `codegen.toml`
would.

| Spec | Version / size | ApiClient flat | Split tree | Typify flat | Compiles as-is | Compiles with workarounds | Wire pairs OK/total |
|---|---|---|---|---|---|---|---|
| GitHub | 3.0.3 / 12 MB | ✅ 2.7 s, 162,595 ln | ✅ 3.8 s, 3,588 files | ✅ 6.3 s, 523,011 ln | ⛔ 3 dup names (E0428 family) | ✅ 3 renames + `patch = false` + no `Default` derive | **282 / 342** |
| Stripe | 3.0.0 / 7.5 MB | ✅ 2.2 s, 147,363 ln | ✅ 3.3 s, 1,767 files | ✅ 7.8 s, 462,759 ln | ⛔ 29 dup names | ✅ 29 renames + `patch = false` | **4 / 6** (fixtures) |
| Plaid | 3.0.0 / 2.9 MB | ✅ 1.4 s, 88,790 ln | ✅ 1.6 s, 999 files | ✅ 2.1 s, 179,400 ln | ⛔ 2 dup names | ✅ 2 renames (patch stays on) | **313 / 348** |
| DigitalOcean | 3.0.0 / 2.9 MB | ✅ 0.7 s, 44,251 ln | ✅ 1.4 s, 1,995 files | ✅ 1.3 s, 103,139 ln | ⛔ 2 dup names | ✅ 2 renames (patch stays on) | **59 / 63** |
| Museum (3.1) | 3.1.0 / 23 KB | ✅ 12 ms, 838 ln | ✅ 30 files | ✅ 1,404 ln | ✅ as-is | — | **10 / 11** |
| Docker (raw 2.0) | 2.0 / 431 KB | ⛔ loud version gate: `unsupported OpenAPI version "2.0"; convert Swagger 2.0 documents to 3.0.x first` | same | same | — | — | — |

Wire tests run the ApiClient profile with `rename-all` unset (these are
snake_case APIs; keeping the spec's wire names is the config decision any
real consumer makes — the camelCase default is the sabre house style).
Durations: wire scratch build+run 35–64 s per spec; GitHub's flat
generation for wire (renames applied, 5-round convergence loop) 224 s
worst case.

**3.1 status** (characterized, not asserted away): 3.1 documents are
*accepted* — `type: [T, "null"]` folds, numeric exclusive bounds pass —
and the museum spec works end-to-end. `const` / `prefixItems` still pass
through unmodeled (they land in `extra`), and webhooks-only documents
fail partitioned modes on `missing /paths`. Unchanged from the old audit;
the 3.1 seam remains a seam, not a gate.

## Part 2 — what the audit found and fixed

Six real defects surfaced, all in our lowering or the fork — each fixed,
test-pinned, and verified against the corpus (upstream typify goldens
byte-unchanged for the fork fixes):

1. **Nullable named schemas self-collided** *(lowering,
   `src/spec/schema.rs`)*. `nullable: true` rendered as
   `anyOf [inner, null]`; for a *named* definition typify then named the
   inner after the definition itself: `X(Option<X>)` — ~50 E0428 + ~50
   E0072 per profile on GitHub/Stripe (the `nullable-*` schema family).
   Typed nullables now render as draft-07 `type: [T, "null"]` (typify's
   collision-free `X(Option<XInner>)` path), nullable `oneOf`/`anyOf`
   gain a `{type: null}` member, and a title on the nullable node is
   withheld (it re-introduced the collision through title-derived
   naming). Fixture specs carry zero `nullable` — why this class was
   invisible before.
2. **`type: string` enums with boolean/number members failed generation**
   *(lowering)*. Plaid writes `enum: [true, false]` under
   `type: string` (YAML parses `- true` as a boolean). The declared type
   wins: members stringify. Was a hard generation failure
   ("unexpected value type").
3. **`default: null` on non-nullable nodes killed generation**
   *(lowering)*. DigitalOcean writes `default: null` on plain `oneOf`
   unions — it means "no default" and now drops; typify used to panic
   (`type_entry.rs:915`) or reject. On *nullable* nodes the null default
   is kept (it's the Option's intrinsic default) — paired with a fork fix
   keeping such defaults on the Option side of the type-array split.
4. **Fork: Option inners reused required names** *(fork `convert_option`,
   `convert_enum_string`)*. Two more spellings of the same collision:
   named nullable compositions (Plaid's `nullable: true` + `allOf`
   wrappers) and null-member string enums (`enum: [..., null]`) handed
   the definition's name to the inner type. Both now use the
   `{name}Inner` convention the type-array arm already had.
5. **Fork: newtype schema-`Default` clashed with the derive list**
   *(fork `output_newtype`)*. DigitalOcean's integer-enum newtypes with
   schema defaults emitted a hand-written `impl Default` alongside the
   api-client profile's `#[derive(Default)]` (E0119). Now deduped,
   mirroring the existing struct-path rule.
6. **Fork: `From<String>` on untagged variants of native String types**
   *(fork untagged `From` emission)*. GitHub's integer-or-date-time
   unions under `date-time → String` mapping emitted `From<String>`
   colliding with the `TryFrom<String>` ladder through core's blanket
   impl. The existing String-variant skip now covers native
   `::std::string::String` entries too.

Also fixed on the way: the fork's `BadValue` error now prints the
expected type and offending value (locating Plaid's mistyped enums in a
3 MB YAML was impossible without it), and `fetch.sh` normalizes
DigitalOcean's above-`u64::MAX` integer literal to its exact float form
(`serde_yaml` loader limitation, unchanged from the old audit).

## Part 3 — wire round-trips: the silent-wrong hunt

770 (type, payload) pairs across five specs: whole-schema `example`s,
request/response `content` examples resolved through
`components.examples`, five live GitHub API captures, and six of
Stripe's own published fixtures. Each pair asserts (a) deserialization,
(b) `from_value(to_value(v)) == v` stability, (c) re-serialization
equality against the null-stripped original (absent-vs-`None` is the
documented `skip_serializing_none` elision; integer/float lexeme
equality is numeric).

| Spec | OK | DESER | LOSSY | UNSTABLE |
|---|---|---|---|---|
| GitHub | 282 | 46 | 14 | 0 |
| Plaid | 313 | 21 | 14 | 0 |
| DigitalOcean | 59 | 0 | 4 | 0 |
| Stripe (fixtures) | 4 | 0 | 2 | 0 |
| Museum 3.1 | 10 | 0 | 1 | 0 |
| **Total** | **668** | **67** | **35** | **0** |

**Zero UNSTABLE results** — every value that deserializes re-serializes
to a fixed point. **Every failure was traced and classified; none is a
silent lowering defect:**

- **DESER (67, all loud):** the spec's own examples don't conform to
  their schemas — missing `required` fields (Plaid's Wallet examples
  lack `balance`; GitHub's `Migration` example lacks `exclude_git_data`),
  literal typos (GitHub Classroom examples contain the string
  `"false,"` — trailing comma — where a boolean is declared), and
  wrong-shaped `BasicError` examples. The types correctly *refuse* these
  payloads. Untagged-union mismatches surface here too
  ("data did not match any variant") — loud, not silent.
- **LOSSY (35):** every single one is the example carrying properties
  its schema never declares (GitHub's `FullRepository.parent` example
  embeds `network_count`, but `parent` references the `repository`
  schema, which doesn't declare it — verified against the schema text;
  museum's confirmation example carries an undeclared `eventName`;
  DigitalOcean's backup-policy example writes `day` where the schema
  says `weekday`), plus two documented profile semantics: non-required
  **map** fields are bare `HashMap` +
  `skip_serializing_if = "HashMap::is_empty"`, so an empty `{}` on the
  wire round-trips to *absent* (Stripe `metadata`), the map-flavored
  sibling of the absent-vs-`null` elision. Typed structs dropping
  *undeclared* extra keys is inherent to struct codegen — a client that
  must echo unknown fields needs `additionalProperties` in the schema
  (which generates the flattened overflow map) or a patch adding it.
- One earlier harness-level finding worth recording: Stripe's expandable
  unions are deep enough that untagged deserialization overflows an 8 MB
  stack — real consumers of the generated Stripe types need bigger
  stacks or boxed recursion; the harness runs its checks on a 512 MB
  stack thread.

## Part 4 — the residual gap list

Ranked; all loud at compile time, none silent:

| # | Gap | Corpus impact | Escape |
|---|---|---|---|
| 1 | **Inline-name collisions**: typify's `{Parent}{Prop}` synthetic names collide with real schemas of the same Rust name (`code-scanning-variant-analysis` + `-status`), and distinct schema keys can share one Rust ident (Plaid's `ExternalPaymentScheduleGet` family) | GitHub 3, Stripe 29, Plaid 2, DO 2 duplicate names | mechanical schema renames (the harness's converging rename loop is the RFC-6902 escape, 1 round in practice); a structural fix needs fork-side name interning / de-collision |
| 2 | **Deep-patch recursion** (D15-known, both engines): `#[patch(name = ...)]` through `Box`ed cycles makes `*Patch` companions infinite-size | GitHub, Stripe (E0072 with patch on) | `[style] patch = false`, or per-type `[types] patch = false` on the cyclic types |
| 3 | **`Default` derive vs never-typed required fields**: GitHub webhooks declare required fields of unsatisfiable schemas (empty enums); a `Default`-deriving struct can't hold them | GitHub only | drop `Default` from the derive lists (+ `untagged-enum-defaults = false`) |
| 4 | **Untagged-union dispatch**: `oneOf`/`anyOf` remain first-match untagged; discriminators are preserved but unconsumed | corpus-wide; loud in wire tests, no misdispatch *observed* in 770 pairs | discriminator-driven tagged enums remain the top structural improvement |
| 5 | **3.1 keywords** `const`/`prefixItems` unmodeled (silent pass-through into `extra`); webhooks-only documents fail partitioned modes | ~0 in corpus | model or reject loudly |
| 6 | **Loader**: YAML integer literals > `u64::MAX` | DO 1 literal | `fetch.sh`-style normalization; loud error otherwise |

## Part 5 — verdict vs. the IR audit

The IR engine (appendix below) failed 5 of these 7 specs at *generation*
and carried six verified silent-wrong lowerings. The typify base
generates **everything 3.0.x in the corpus, unpatched, in seconds**
(where the old audit needed 1–1,619 mechanical spec patches per spec to
even generate — and the typify engine of that era couldn't generate
DigitalOcean or Plaid at all). Compilation needs a *bounded, mechanical,
config-expressible* workaround set whose size we can now state exactly
(2–29 renames; two style keys), and wire behavior — measured for the
first time at corpus scale — shows **no silent lowering defects**: the
failure mass sits in the specs' own examples, and the profile semantics
that do diverge (absent-vs-null, absent-vs-empty-map) are deliberate,
documented, and symmetric.

The honest gap between this and "drop in any spec and go": duplicate
Rust names must stop being the consumer's problem (fork-side
de-collision — gap #1), and discriminated unions deserve real tags
(gap #4). Both are typify-side structural work. Everything else on the
old audit's close-the-gap list — null-member enums, singleton-`anyOf`
collapse, allOf merge robustness, map fidelity, integer ladders,
self-referential arrays — is *already handled* by typify or was fixed in
this audit.

---

# Appendix: the IR-engine audit (2026-07-03, historical)

The following is the audit of the retired IR engine, preserved verbatim
as the record behind decision D15. Its "typify engine" comparison column
describes the fork *before* the D16–D21 work and this audit's fixes.

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
compiles but does not faithfully speak the wire protocol.

- **Audit date:** 2026-07-03
- **Engine commit:** `4f5c0f2e92fb44b4e8189744e2efaaa65b701c27` (branch
  `ir-migration`)
- **typify fork baseline:** `fork-freeze-20260703` (`984268b`)

## IR empirical results (historical table)

| Spec | IR unpatched | typify unpatched (pre-audit fork) | Patch burden → IR success | IR output compiles? |
|---|---|---|---|---|
| GitHub | ⛔ `enum value null` | ✅ 0.8 s | **315** | ✅ |
| Stripe | ⛔ name collision | ✅ 0.6 s | **465** | ⛔ (`E0072` `*Patch`); ✅ with `patch = false` |
| DigitalOcean | ⛔ YAML loader | ⛔ schemars reject + panic | **6** | ✅ |
| Cloudflare | ⛔ false alias-cycle | ⛔ schemars reject | **1,619** | ⛔; ✅ with `patch = false` |
| Docker (converted) | ⛔ integer enum | ✅ 0.1 s | **1** | ✅ |
| Docker (raw 2.0) | ⛔ version gate | ⛔ same | n/a | n/a |
| Museum (3.1) | ✅ | ✅ | **0** | ✅ |
| webhook-example (3.1) | ✅ flat | same | **0** | ✅ |

## IR silent-wrong lowerings (all fixed-by-retirement; verified by wire test at the time)

1. Untagged misdispatch on discriminated `oneOf` (first-match-wins).
2. `#[serde(flatten)]` over a scalar `allOf` base — runtime serde failure.
3. Named free-form objects → `pub struct X {}` — total round-trip loss.
4. `uint64 → i64` — overflow at runtime.
5. `additionalProperties` overflow silently dropped.
6. `oneOf` sibling properties silently dropped.
7. Required + nullable serializing as *absent* (both engines; profile
   semantics, still true today and documented above).

The IR engine's full construct matrix and gap list are preserved in git
history (`git log --follow docs/SPEC_COVERAGE.md`); the parts that remain
relevant to the typify base are restated in Parts 4–5 above.

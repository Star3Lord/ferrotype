//! Operation-reachability partitioning.
//!
//! For specs with many operations it's useful to group generated types into
//! one `pub mod <snake_operation_id>` per operation, with everything
//! reachable from two or more operations (plus orphan schemas) landing in a
//! `pub mod shared`. This module computes that partition on the
//! **pre-lowering** OpenAPI document, where `$ref`s are still keyed by
//! `#/components/schemas/<Name>`.
//!
//! Two modes are supported:
//!
//! - [`Partition::compute`] — the flat mode: one `pub mod <op>` per
//!   operation plus `pub mod shared`.
//! - [`Partition::compute_split`] — the request/response mode: every
//!   operation's `$ref` entry points are classified by role (`requestBody`
//!   and parameter schemas → request, `responses` → response) and each
//!   role's closure is walked separately, yielding nested
//!   `<op>/request` / `<op>/response` module paths plus a role-classified
//!   `shared/{request,response,enums,common}` subtree (see
//!   [`Partition::compute_split`] for the exact policy).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use anyhow::{Context, bail};
use heck::ToSnakeCase;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use serde_json::Value;

use crate::Result;

/// Name of the catch-all module hosting types reachable from two or more
/// operations (plus any orphan / unreachable schemas and typify-generated
/// types for inline schemas) in the flat (non-split) mode.
pub const SHARED_MODULE: &str = "shared";

/// Split mode: shared types used only in request positions.
pub const SHARED_REQUEST_MODULE: &str = "shared/request";

/// Split mode: shared types used only in response positions.
pub const SHARED_RESPONSE_MODULE: &str = "shared/response";

/// Split mode: shared simple (all-unit-variant) enums, regardless of the
/// roles they appear in. Mirrors the hand-written `shared/enums.rs`
/// convention: enums are atomic and safe to share across request and
/// response shapes.
pub const SHARED_ENUMS_MODULE: &str = "shared/enums";

/// Split mode: the catch-all for shared non-enum types used in **both**
/// request and response positions, for orphan / unreachable schemas, and
/// for typify-generated types for inline schemas (the default module).
/// The hand-written reference layout has no equivalent bucket (it
/// duplicates such types per role), so `shared/common` is this crate's
/// convention.
pub const SHARED_COMMON_MODULE: &str = "shared/common";

/// A schema's usage role within an operation, in split mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Role {
    Request,
    Response,
}

impl Role {
    fn leaf(self) -> &'static str {
        match self {
            Role::Request => "request",
            Role::Response => "response",
        }
    }
}

/// The partition computed from an OpenAPI spec: schema-name → module-name.
#[derive(Debug, Default)]
pub struct Partition {
    /// Schema name (verbatim, as it appears in `components/schemas`) →
    /// module name: a snake_case operationId or [`SHARED_MODULE`] in flat
    /// mode; a slash-separated module path (`<op>/request`,
    /// `<op>/response`, or one of the `shared/*` leaves) in split mode.
    pub by_schema: BTreeMap<String, String>,
    /// Module name → set of schema names; the inverse of
    /// [`Self::by_schema`], kept for logging.
    pub by_module: BTreeMap<String, BTreeSet<String>>,
    /// Schema names that were not reachable from any operation. They are
    /// still generated, placed in [`SHARED_MODULE`] (flat mode) or
    /// [`SHARED_COMMON_MODULE`] / [`SHARED_ENUMS_MODULE`] (split mode).
    pub unreachable: Vec<String>,
    /// Every per-operation module name (excluding the shared modules).
    /// In split mode these are the operation names; the actual leaf
    /// modules are `<op>/request` and `<op>/response`.
    pub op_modules: BTreeSet<String>,
    /// Whether this partition was computed by [`Self::compute_split`].
    /// Split partitions classify shared types by role, route simple
    /// enums to [`SHARED_ENUMS_MODULE`] (resolved in
    /// [`Self::to_rust_partition`], where typify's view of each type
    /// exists), and emit nested import preambles.
    pub split_request_response: bool,
    /// Split mode: `(from_schema, to_schema)` references that cross role
    /// boundaries — a schema in one role's closure referencing an entry
    /// root of the opposite role (e.g. a response echoing the request it
    /// answers). The role walk does not traverse these edges (see
    /// [`Self::compute_split`]); instead [`Self::module_imports`] adds a
    /// targeted glob import so the reference still resolves.
    pub cross_role_refs: BTreeSet<(String, String)>,
}

impl Partition {
    /// Compute the reachability-based partition from a parsed OpenAPI spec.
    ///
    /// Every operation must carry an `operationId`; the snake_case form
    /// becomes the module name. Each operation's entry-point schemas
    /// (request body + all response bodies) are expanded through `$ref`
    /// closure; schemas reachable from exactly one operation go in that
    /// operation's module and everything else goes in [`SHARED_MODULE`].
    pub fn compute(spec: &Value) -> Result<Self> {
        let schemas = component_schemas(spec)?;
        let all_schemas: BTreeSet<String> = schemas.keys().cloned().collect();

        // BFS each operation through `$ref` to get its reachable closure,
        // then record ownership per schema.
        let mut ownership: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut op_modules = BTreeSet::new();
        for_each_operation(spec, |module, operation| {
            let mut entry_points = BTreeSet::new();
            collect_component_refs(operation, &mut entry_points);
            op_modules.insert(module.to_string());
            for schema in bfs_reachable(schemas, &entry_points) {
                ownership
                    .entry(schema)
                    .or_default()
                    .insert(module.to_string());
            }
            Ok(())
        })?;

        // Assign each schema: one owner → that op's module; zero or many
        // owners → shared.
        let mut partition = Partition {
            op_modules,
            ..Default::default()
        };
        for schema in &all_schemas {
            let owners = ownership.get(schema);
            let target = match owners.map(BTreeSet::len).unwrap_or(0) {
                0 => {
                    partition.unreachable.push(schema.clone());
                    SHARED_MODULE.to_string()
                }
                1 => owners.unwrap().iter().next().unwrap().clone(),
                _ => SHARED_MODULE.to_string(),
            };
            partition.record(schema, target);
        }
        Ok(partition)
    }

    /// Compute the role-aware request/response partition from a parsed
    /// OpenAPI spec.
    ///
    /// Like [`Self::compute`], but each operation's entry points are
    /// classified by role first — `$ref`s under `requestBody` and under
    /// the operation's `parameters` are request entry points, `$ref`s
    /// under `responses` are response entry points — and each role's
    /// `$ref` closure is walked separately. Every schema's usages are
    /// aggregated into a set of `(operation, role)` pairs across the
    /// whole spec, then assigned a nested module path:
    ///
    /// - exactly one `(op, role)` usage → `<op>/request` or
    ///   `<op>/response`
    /// - several operations, request-only → [`SHARED_REQUEST_MODULE`]
    /// - several operations, response-only → [`SHARED_RESPONSE_MODULE`]
    /// - both roles (any number of operations) → [`SHARED_COMMON_MODULE`]
    /// - unreachable from every operation → [`SHARED_COMMON_MODULE`]
    ///   (also recorded in [`Self::unreachable`])
    ///
    /// On top of that, any **shared** type whose generated Rust type
    /// turns out to be a simple (all-unit-variant) enum is routed to
    /// [`SHARED_ENUMS_MODULE`] instead. That last step needs typify's
    /// view of the generated types, which only exists after
    /// `add_root_schema`, so it is resolved in
    /// [`Self::to_rust_partition`]; [`Self::by_schema`] holds the
    /// role-derived paths until then.
    ///
    /// # Role boundaries
    ///
    /// A role's closure walk stops at entry points of the *opposite*
    /// role. APIs routinely echo the request inside the response (e.g.
    /// Sabre's `CancelBookingResponse.request` is the
    /// `CancelBookingRequest` that was answered); traversing that edge
    /// would drag the entire request tree into the response role and
    /// collapse nearly every schema into [`SHARED_COMMON_MODULE`].
    /// Instead the walk records the edge in [`Self::cross_role_refs`]
    /// and [`Self::module_imports`] bridges it with a glob import of the
    /// referenced root's module — exactly how the hand-written layout
    /// handles it (`cancel_booking/response` imports
    /// `cancel_booking::request::CancelBookingRequest`). A schema that
    /// is an entry point of **both** roles bounds neither walk and
    /// classifies as dual-role via the normal rules.
    pub fn compute_split(spec: &Value) -> Result<Self> {
        let schemas = component_schemas(spec)?;
        let all_schemas: BTreeSet<String> = schemas.keys().cloned().collect();

        // Pass 1: collect every operation's per-role entry points, and
        // the global per-role root sets that bound the opposite role's
        // walks.
        struct OpEntries {
            module: String,
            request: BTreeSet<String>,
            response: BTreeSet<String>,
        }
        let mut ops: Vec<OpEntries> = Vec::new();
        for_each_operation(spec, |module, operation| {
            let mut request = BTreeSet::new();
            if let Some(request_body) = operation.get("requestBody") {
                collect_component_refs(request_body, &mut request);
            }
            if let Some(parameters) = operation.get("parameters") {
                collect_component_refs(parameters, &mut request);
            }
            let mut response = BTreeSet::new();
            if let Some(responses) = operation.get("responses") {
                collect_component_refs(responses, &mut response);
            }
            ops.push(OpEntries {
                module: module.to_string(),
                request,
                response,
            });
            Ok(())
        })?;

        let request_roots: BTreeSet<String> =
            ops.iter().flat_map(|op| op.request.iter().cloned()).collect();
        let response_roots: BTreeSet<String> =
            ops.iter().flat_map(|op| op.response.iter().cloned()).collect();
        let request_walk_boundary: BTreeSet<String> =
            response_roots.difference(&request_roots).cloned().collect();
        let response_walk_boundary: BTreeSet<String> =
            request_roots.difference(&response_roots).cloned().collect();

        // Pass 2: BFS each (operation, role) pair separately — bounded
        // by the opposite role's roots — and aggregate every schema's
        // usages across the whole spec.
        let mut usage: BTreeMap<String, BTreeSet<(String, Role)>> = BTreeMap::new();
        let mut partition = Partition {
            split_request_response: true,
            ..Default::default()
        };
        for op in &ops {
            partition.op_modules.insert(op.module.clone());
            for (role, entries, boundary) in [
                (Role::Request, &op.request, &request_walk_boundary),
                (Role::Response, &op.response, &response_walk_boundary),
            ] {
                let (reachable, edges) = bfs_reachable_bounded(schemas, entries, boundary);
                for schema in reachable {
                    usage
                        .entry(schema)
                        .or_default()
                        .insert((op.module.clone(), role));
                }
                partition.cross_role_refs.extend(edges);
            }
        }

        for schema in &all_schemas {
            let usages = usage.get(schema);
            let target = match usages.map(BTreeSet::len).unwrap_or(0) {
                0 => {
                    partition.unreachable.push(schema.clone());
                    SHARED_COMMON_MODULE.to_string()
                }
                1 => {
                    let (op, role) = usages.unwrap().iter().next().unwrap();
                    format!("{op}/{}", role.leaf())
                }
                _ => {
                    let usages = usages.unwrap();
                    let request = usages.iter().any(|(_, role)| *role == Role::Request);
                    let response = usages.iter().any(|(_, role)| *role == Role::Response);
                    match (request, response) {
                        (true, false) => SHARED_REQUEST_MODULE.to_string(),
                        (false, true) => SHARED_RESPONSE_MODULE.to_string(),
                        _ => SHARED_COMMON_MODULE.to_string(),
                    }
                }
            };
            partition.record(schema, target);
        }
        Ok(partition)
    }

    /// Record `schema` as belonging to `target` in both directions.
    fn record(&mut self, schema: &str, target: String) {
        self.by_module
            .entry(target.clone())
            .or_default()
            .insert(schema.to_string());
        self.by_schema.insert(schema.to_string(), target);
    }

    /// The module that typify-generated types for inline schemas (and any
    /// type missing from the partition map) land in:
    /// [`SHARED_COMMON_MODULE`] in split mode, [`SHARED_MODULE`]
    /// otherwise.
    pub fn default_module(&self) -> &'static str {
        if self.split_request_response {
            SHARED_COMMON_MODULE
        } else {
            SHARED_MODULE
        }
    }

    /// Translate this schema-name → module-name partition into the
    /// Rust-type-name → module-name map that the partitioned emitter
    /// ([`crate::modules`]) consumes.
    /// [`typify::TypeSpace::iter_definitions`] is the bridge: it pairs
    /// each definition key with its generated type, accounting for
    /// typify's Pascal-case sanitization of schema keys.
    ///
    /// In split mode this is also where the [`SHARED_ENUMS_MODULE`]
    /// routing happens: a schema assigned to one of the shared role
    /// leaves whose generated Rust type is a simple (all-unit-variant)
    /// enum moves to `shared/enums`. Enum-ness is a property of the
    /// *generated* type, so it can only be decided here, with the
    /// populated [`typify::TypeSpace`] in hand — not in
    /// [`Self::compute_split`], which runs before typify.
    pub fn to_rust_partition(&self, type_space: &typify::TypeSpace) -> HashMap<String, String> {
        let simple_enums = if self.split_request_response {
            simple_enum_names(type_space)
        } else {
            BTreeSet::new()
        };
        type_space
            .iter_definitions()
            .filter_map(|(schema_key, ty)| {
                let module = self.by_schema.get(schema_key)?;
                let rust_name = ty.name();
                let module = if self.split_request_response
                    && is_shared_role_module(module)
                    && simple_enums.contains(&rust_name)
                {
                    SHARED_ENUMS_MODULE.to_string()
                } else {
                    module.clone()
                };
                Some((rust_name, module))
            })
            .collect()
    }

    /// Build the per-module `use ...;` preamble.
    ///
    /// Every module receives `trait_imports` (paths that generated bare
    /// derives rely on, e.g. `use serde::{Serialize, Deserialize};`).
    /// Cross-module glob imports follow a two-way visibility pattern so any
    /// pair of generated types can reference each other unqualified:
    ///
    /// - per-operation modules `use super::shared::*;`
    /// - the shared module imports every operation module's glob, because
    ///   typify-generated types for *inline* schemas always land in the
    ///   default (shared) module and may reference op-owned types.
    ///
    /// In split mode the same two-way pattern is expressed over the
    /// nested layout with `super`-chain globs (the mount point of the
    /// generated tree is caller-chosen, so `crate::`-anchored paths are
    /// not an option):
    ///
    /// - `<op>/request` leaves glob `shared/request`, `shared/enums`,
    ///   and `shared/common` (`<op>/response` analogously) — an op-owned
    ///   type only ever references its own leaf or shared types of a
    ///   compatible role;
    /// - `shared/request` and `shared/response` glob `shared/enums` and
    ///   `shared/common` (their types may hold shared enums and inline
    ///   types, which land in `shared/common`);
    /// - `shared/common`, as the default module hosting inline-schema
    ///   types (which may reference op-owned types), globs every other
    ///   shared leaf **and** every `<op>/{request,response}` leaf —
    ///   the reverse direction of the two-way pattern;
    /// - `shared/enums` holds atomic enums that reference nothing, so it
    ///   receives only `trait_imports`;
    /// - every cross-role reference recorded in
    ///   [`Self::cross_role_refs`] adds a targeted glob of the
    ///   referenced module (e.g. `<op>/response` globs `<op>/request`
    ///   when the response echoes the request).
    ///
    /// The resulting glob-import cycles between sibling leaves are fine
    /// in Rust as long as no two glob-imported names collide, and
    /// generated type names are globally unique (typify's namespace is
    /// flat).
    pub fn module_imports(&self, trait_imports: &TokenStream) -> HashMap<String, TokenStream> {
        if self.split_request_response {
            return self.split_module_imports(trait_imports);
        }
        let mut imports = HashMap::new();

        let op_globs: Vec<TokenStream> = self
            .op_modules
            .iter()
            .map(|module| {
                let ident = quote::format_ident!("{}", module);
                quote! { use super::#ident::*; }
            })
            .collect();
        imports.insert(
            SHARED_MODULE.to_string(),
            quote! {
                #trait_imports
                #(#op_globs)*
            },
        );

        for module in &self.op_modules {
            imports.insert(
                module.clone(),
                quote! {
                    #trait_imports
                    use super::shared::*;
                },
            );
        }
        imports
    }

    /// The split-mode import preambles; see [`Self::module_imports`] for
    /// the full policy. Built as a per-module set of glob targets
    /// (deterministically ordered) so the cross-role bridge imports from
    /// [`Self::cross_role_refs`] merge in without duplication.
    fn split_module_imports(&self, trait_imports: &TokenStream) -> HashMap<String, TokenStream> {
        let mut targets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        // Op leaves see the shared leaves of their own role.
        for op in &self.op_modules {
            for role in [Role::Request, Role::Response] {
                targets.entry(format!("{op}/{}", role.leaf())).or_default().extend([
                    format!("shared/{}", role.leaf()),
                    SHARED_ENUMS_MODULE.to_string(),
                    SHARED_COMMON_MODULE.to_string(),
                ]);
            }
        }

        // Shared role leaves see the enums and the common pool (their
        // types may hold shared enums and inline-schema types).
        for leaf in [SHARED_REQUEST_MODULE, SHARED_RESPONSE_MODULE] {
            targets.entry(leaf.to_string()).or_default().extend([
                SHARED_ENUMS_MODULE.to_string(),
                SHARED_COMMON_MODULE.to_string(),
            ]);
        }

        // Enums are atomic: trait imports only.
        targets.entry(SHARED_ENUMS_MODULE.to_string()).or_default();

        // The common pool hosts inline-schema types, which may reference
        // anything — the reverse direction of the two-way pattern.
        let common = targets.entry(SHARED_COMMON_MODULE.to_string()).or_default();
        common.extend([
            SHARED_ENUMS_MODULE.to_string(),
            SHARED_REQUEST_MODULE.to_string(),
            SHARED_RESPONSE_MODULE.to_string(),
        ]);
        for op in &self.op_modules {
            for role in [Role::Request, Role::Response] {
                common.insert(format!("{op}/{}", role.leaf()));
            }
        }

        // Bridge the recorded cross-role references: the referencing
        // schema's module gets a targeted glob of the referenced root's
        // module (e.g. `cancel_booking/response` → `cancel_booking/request`).
        for (from, to) in &self.cross_role_refs {
            let (Some(from_module), Some(to_module)) =
                (self.by_schema.get(from), self.by_schema.get(to))
            else {
                continue;
            };
            if from_module != to_module {
                targets
                    .entry(from_module.clone())
                    .or_default()
                    .insert(to_module.clone());
            }
        }

        targets
            .into_iter()
            .map(|(module, globs)| {
                let globs = globs
                    .iter()
                    .filter(|target| **target != module)
                    .map(|target| glob_use(&module, target));
                let preamble = quote! {
                    #trait_imports
                    #(#globs)*
                };
                (module, preamble)
            })
            .collect()
    }

    /// Log a human-readable bucket summary to stderr.
    ///
    /// In split mode the buckets show the role-derived assignment; simple
    /// enums headed for [`SHARED_ENUMS_MODULE`] are resolved later (in
    /// [`Self::to_rust_partition`]) and still counted under their role
    /// bucket here.
    pub fn log_summary(&self, label: &str) {
        let total: usize = self.by_module.values().map(BTreeSet::len).sum();
        eprintln!("openapi-codegen[{label}]: partition summary ({total} schemas)");
        for (module, schemas) in &self.by_module {
            eprintln!("  {module}: {} schemas", schemas.len());
        }
        if !self.unreachable.is_empty() {
            eprintln!(
                "openapi-codegen[{label}]: warning: {} schemas unreachable from \
                 any operation; placed in `{}`: {:?}",
                self.unreachable.len(),
                self.default_module(),
                self.unreachable,
            );
        }
    }
}

/// The `components/schemas` object of `spec`.
fn component_schemas(spec: &Value) -> Result<&serde_json::Map<String, Value>> {
    spec.pointer("/components/schemas")
        .and_then(Value::as_object)
        .context("OpenAPI spec is missing /components/schemas")
}

/// Invoke `f` with the snake_case module name and operation object of
/// every operation in the spec, validating that each has a usable
/// `operationId`. Fails if the spec has no operations at all.
fn for_each_operation(
    spec: &Value,
    mut f: impl FnMut(&str, &Value) -> Result<()>,
) -> Result<()> {
    let paths = spec
        .pointer("/paths")
        .and_then(Value::as_object)
        .context("OpenAPI spec is missing /paths")?;

    const METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];
    let mut seen_any = false;
    for (path, path_item) in paths {
        let Value::Object(path_methods) = path_item else {
            continue;
        };
        for method in METHODS {
            let Some(operation) = path_methods.get(*method) else {
                continue;
            };
            let Some(op_id) = operation.get("operationId").and_then(Value::as_str) else {
                bail!(
                    "operation {method} {path} has no operationId; add one \
                     to keep per-operation partitioning stable",
                );
            };
            let module_name = op_id.to_snake_case();
            if module_name == SHARED_MODULE {
                bail!(
                    "operationId {op_id:?} snake-cases to the reserved \
                     `{SHARED_MODULE}` module name",
                );
            }
            seen_any = true;
            f(&module_name, operation)?;
        }
    }
    if !seen_any {
        bail!("no operations found in spec");
    }
    Ok(())
}

/// Whether `module` is one of the shared role leaves that
/// [`Partition::to_rust_partition`] may re-route to
/// [`SHARED_ENUMS_MODULE`].
fn is_shared_role_module(module: &str) -> bool {
    module == SHARED_REQUEST_MODULE
        || module == SHARED_RESPONSE_MODULE
        || module == SHARED_COMMON_MODULE
}

/// The Rust names of every generated simple enum: an enum whose variants
/// all carry no data. These are the types that belong in
/// [`SHARED_ENUMS_MODULE`] when shared, mirroring the hand-written
/// convention that atomic enums are safe to share across request and
/// response shapes.
fn simple_enum_names(type_space: &typify::TypeSpace) -> BTreeSet<String> {
    type_space
        .iter_types()
        .filter_map(|ty| match ty.details() {
            typify::TypeDetails::Enum(details) => details
                .variants()
                .all(|(_, variant)| matches!(variant, typify::TypeEnumVariant::Simple))
                .then(|| ty.name()),
            _ => None,
        })
        .collect()
}

/// A `use` statement glob-importing the module at slash-separated path
/// `target`, written relative to the module at slash-separated path
/// `from`: a chain of `super`s climbing to the common root (the depth of
/// `from`), then `target`'s segments. `crate::`-anchored paths are not an
/// option because the caller chooses where the generated tree is mounted.
fn glob_use(from: &str, target: &str) -> TokenStream {
    let supers = std::iter::repeat_n(quote! { super }, from.split('/').count());
    let segments = target.split('/').map(|segment| format_ident!("{}", segment));
    quote! { use #(#supers::)*#(#segments)::*::*; }
}

/// Append every `#/components/schemas/<Name>` ref found anywhere under
/// `value` to `acc`.
fn collect_component_refs(value: &Value, acc: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && let Some(rest) = reference.strip_prefix("#/components/schemas/")
            {
                acc.insert(rest.to_string());
            }
            for child in map.values() {
                collect_component_refs(child, acc);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_component_refs(child, acc);
            }
        }
        _ => {}
    }
}

/// Compute the set of schema names reachable from any of `start` via
/// `$ref: #/components/schemas/<Name>` traversal of `schemas`.
fn bfs_reachable(
    schemas: &serde_json::Map<String, Value>,
    start: &BTreeSet<String>,
) -> BTreeSet<String> {
    bfs_reachable_bounded(schemas, start, &BTreeSet::new()).0
}

/// [`bfs_reachable`], except the walk does not traverse **into** any
/// schema in `boundary`; each skipped `(from, boundary_schema)` edge is
/// returned alongside the reachable set so the caller can bridge it with
/// an import instead.
fn bfs_reachable_bounded(
    schemas: &serde_json::Map<String, Value>,
    start: &BTreeSet<String>,
    boundary: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeSet<(String, String)>) {
    let mut visited: HashSet<String> = start.iter().cloned().collect();
    let mut queue: VecDeque<String> = start.iter().cloned().collect();
    let mut skipped_edges = BTreeSet::new();

    while let Some(name) = queue.pop_front() {
        let Some(schema) = schemas.get(&name) else {
            continue;
        };
        let mut refs = BTreeSet::new();
        collect_component_refs(schema, &mut refs);
        for reference in refs {
            if boundary.contains(&reference) {
                skipped_edges.insert((name.clone(), reference));
                continue;
            }
            if visited.insert(reference.clone()) {
                queue.push_back(reference);
            }
        }
    }
    (visited.into_iter().collect(), skipped_edges)
}

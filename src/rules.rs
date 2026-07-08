//! The field-scoped override tiers (docs/MIGRATION.md D20): ordered
//! `[[rules]]` between the style-level mappings and the `[fields]`
//! tier, plus the resolution step that materializes one effective
//! decision per generated struct field.
//!
//! Layering, least to most specific:
//!
//! 1. `[style.formats]` / `[types] replace` mapping attrs and
//!    capabilities — the base (applied per *type*);
//! 2. `[[rules]]` in declaration order, later rules overriding earlier
//!    ones key-by-key (applied per matching *field*);
//! 3. `[fields."Type.field"]` — most specific, beats every rule.
//!
//! The winning decision per field is materialized **once** here
//! ([`FieldRules::field_plans`]) and flows through the existing
//! application machinery in [`crate::mappings`] — attr attachment,
//! type replacement, and the capability-pruning fixpoint — rather
//! than a second application path.
//!
//! Timing: `deep-patch` payloads feed the fork's generation-time
//! `with_deep_patch_filter`, so rules carrying them may only use
//! pre-generation predicates (`module` from the partition, `struct` /
//! `field` from spec names, `format` from spec provenance); combining
//! `deep-patch` with the post-generation `type` predicate is a hard
//! config error. Attribute/type payloads apply post-generation, where
//! every predicate — the resolved-Rust-type glob included — is
//! checkable.
//!
//! A rule matching zero fields warns on stderr instead of erroring:
//! globs are broad-brush, unlike the exact `[types]` / `[fields]`
//! selectors whose typos must fail loudly.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::bail;
use quote::ToTokens;

use crate::Result;
use crate::config::{Capability, RuleMatch, StyleConfig, TypeReplacement};
use crate::partition::Partition;
use crate::spec::{Schema, Spec};

/// The effective decision for one generated struct field, consumed by
/// [`crate::mappings::Mappings::apply_to_file`].
pub(crate) struct FieldPlan {
    /// Attribute bodies replacing any mapping-derived attrs (`Some`
    /// even when empty — an explicit empty list clears; `None`
    /// inherits the mapping's per-type attrs).
    pub(crate) attrs: Option<Vec<syn::Attribute>>,
    /// A rule-applied Rust type replacement (the `[fields]` tier's
    /// `type` is applied earlier, by `overrides::apply_to_file`).
    /// Replacing a field's type also strips any deep-patch annotation
    /// it carried — the annotation names the old type's companion.
    pub(crate) replace_type: Option<String>,
    /// The declared capabilities of this field's (possibly overridden)
    /// type, with the declaring config origin: overrides the mapping's
    /// per-type declaration in the pruning analysis, for this field
    /// only.
    pub(crate) capabilities: Option<(BTreeSet<Capability>, String)>,
}

/// Per-field decisions keyed by `(owner Rust type, Rust field name)`.
#[derive(Default)]
pub(crate) struct FieldPlans {
    plans: BTreeMap<(String, String), FieldPlan>,
}

impl FieldPlans {
    pub(crate) fn get(&self, owner: &str, field: &str) -> Option<&FieldPlan> {
        self.plans
            .get(&(owner.to_string(), field.to_string()))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }
}

/// Spec-level metadata for one named schema property, keyed by the
/// Rust names the AST will use.
struct RowMeta {
    schema_key: String,
    wire_name: String,
    /// `"type"` or `"type/format"` provenance; `None` when the
    /// property's shape doesn't resolve to an instance type.
    format: Option<String>,
    /// Slash-separated module path from the partition (plus `[types]
    /// module` overrides); `None` without partitioning or for types
    /// the partition doesn't know.
    module: Option<String>,
}

/// One validated `[[rules]]` entry with its payload parsed.
struct ResolvedRule {
    match_: RuleMatch,
    attrs: Option<Vec<syn::Attribute>>,
    type_override: Option<ResolvedTypeOverride>,
    impls: Option<BTreeSet<Capability>>,
    deep_patch: Option<bool>,
    patch: Option<bool>,
    optional: Option<bool>,
}

struct ResolvedTypeOverride {
    path: String,
    attrs: Vec<syn::Attribute>,
    capabilities: BTreeSet<Capability>,
}

/// The `[[rules]]` tier of one run, resolved against the spec: rule
/// payloads parsed and validated, pre-generation metadata (module,
/// provenance) indexed, and the generation-time deep-patch decisions
/// computed. Built in `LoadedSpec::lower` (where the typed spec model
/// and the partition exist) and consumed at both ends of generation.
pub(crate) struct FieldRules {
    rules: Vec<ResolvedRule>,
    meta: BTreeMap<(String, String), RowMeta>,
    /// (owner Rust type, Rust field) → forced deep-patch decision from
    /// the rules tier (the `[fields]` tier still wins inside the
    /// filter).
    deep_patch: BTreeMap<(String, String), bool>,
    /// Rust type → forced patchability from type-level `patch` rules
    /// (the exact `[types]` entry still wins inside `is_patchable`).
    patch: BTreeMap<String, bool>,
    /// (schema key, wire property) → forced optionality from `optional`
    /// rules, applied to the lowered schema's `required` lists by
    /// [`Self::apply_optionality`] before typify runs.
    optional: BTreeMap<(String, String), bool>,
    /// Rules that definitively matched during the pre-generation pass
    /// (only meaningful for rules without a `type` predicate).
    matched_pre: Vec<bool>,
}

impl FieldRules {
    /// Validate the `[[rules]]` tier and evaluate its pre-generation
    /// half: index spec provenance and module placement, and compute
    /// the deep-patch decisions the generation-time filter consumes.
    pub(crate) fn resolve(
        style: &StyleConfig,
        spec: &Spec,
        partition: Option<&Partition>,
    ) -> Result<Self> {
        let mut rules = Vec::with_capacity(style.rules.len());
        for (index, rule) in style.rules.iter().enumerate() {
            let origin = format!("[[rules]] #{index}");
            let m = &rule.match_;
            if m.module.is_none()
                && m.struct_.is_none()
                && m.field.is_none()
                && m.format.is_none()
                && m.type_.is_none()
            {
                bail!("{origin} has no `match` predicates; at least one is required");
            }
            if m.module.is_some() && partition.is_none() {
                bail!(
                    "{origin} matches on `module`, but partitioning is off — enable \
                     --partition-by-operation / --split-request-response or drop the \
                     predicate",
                );
            }
            let a = &rule.apply;
            if a.field_attrs.is_none()
                && a.type_.is_none()
                && a.impls.is_none()
                && a.deep_patch.is_none()
                && a.patch.is_none()
                && a.optional.is_none()
            {
                bail!("{origin} applies nothing; give `apply` at least one key");
            }
            if a.patch.is_some() {
                if a.field_attrs.is_some()
                    || a.type_.is_some()
                    || a.impls.is_some()
                    || a.deep_patch.is_some()
                    || a.optional.is_some()
                {
                    bail!(
                        "{origin} mixes the type-level `patch` payload with field-level \
                         keys; a rule is single-scope — split it into one type-level and \
                         one field-level rule",
                    );
                }
                if m.field.is_some() || m.format.is_some() || m.type_.is_some() {
                    bail!(
                        "{origin} carries the type-level `patch` payload, which matches \
                         types — only the `module` and `struct` predicates apply; drop \
                         `field`/`format`/`type` or move them to a field-level rule",
                    );
                }
            }
            if a.deep_patch.is_some() && m.type_.is_some() {
                bail!(
                    "{origin} combines a `deep-patch` payload with the `type` predicate: \
                     deep-patch decisions feed typify at generation time, before resolved \
                     Rust types exist — match on `module`/`struct`/`field`/`format` \
                     instead",
                );
            }
            if a.optional.is_some() && m.type_.is_some() {
                bail!(
                    "{origin} combines an `optional` payload with the `type` predicate: \
                     optionality rewrites the lowered schema before generation, before \
                     resolved Rust types exist — match on `module`/`struct`/`field`/\
                     `format` instead",
                );
            }
            if a.deep_patch == Some(true) && a.type_.is_some() {
                bail!(
                    "{origin} combines `deep-patch = true` with a `type` replacement; a \
                     replaced type has no known Patch companion, so the annotation cannot \
                     be emitted",
                );
            }

            let attrs = a
                .field_attrs
                .as_ref()
                .map(|bodies| crate::mappings::parse_attr_bodies(&origin, "field-attrs", bodies))
                .transpose()?;
            let type_override = a
                .type_
                .as_ref()
                .map(|replacement| resolve_type_override(&origin, replacement))
                .transpose()?;
            rules.push(ResolvedRule {
                match_: m.clone(),
                attrs,
                type_override,
                impls: a.impls.as_ref().map(|caps| caps.iter().copied().collect()),
                deep_patch: a.deep_patch,
                patch: a.patch,
                optional: a.optional,
            });
        }

        let modules = module_map(style, partition);
        let meta = index_spec(spec, &modules);

        // Pre-generation rule evaluation: deep-patch and optionality
        // decisions, in declaration order (later rules override), over
        // every spec row. A `type` payload implies deep-patch
        // suppression — the annotation would name the displaced type's
        // companion.
        let mut deep_patch: BTreeMap<(String, String), bool> = BTreeMap::new();
        let mut optional: BTreeMap<(String, String), bool> = BTreeMap::new();
        let mut matched_pre = vec![false; rules.len()];
        for (key, row) in &meta {
            for (index, rule) in rules.iter().enumerate() {
                if rule.match_.type_.is_some() || rule.patch.is_some() {
                    continue; // post-generation predicate / type-level rule.
                }
                if !matches_pre(&rule.match_, row) {
                    continue;
                }
                matched_pre[index] = true;
                if let Some(forced) = rule.deep_patch {
                    deep_patch.insert(key.clone(), forced);
                } else if rule.type_override.is_some() {
                    deep_patch.insert(key.clone(), false);
                }
                if let Some(forced) = rule.optional {
                    optional.insert(
                        (row.schema_key.clone(), row.wire_name.clone()),
                        forced,
                    );
                }
            }
        }

        // Type-level `patch` rules evaluate over every named schema
        // (module + struct predicates only, validated above), in
        // declaration order — later rules override; the exact
        // `[types."X"] patch` entry beats them inside
        // `Overrides::is_patchable`.
        let mut patch: BTreeMap<String, bool> = BTreeMap::new();
        for (index, rule) in rules.iter().enumerate() {
            let Some(forced) = rule.patch else { continue };
            for schema_key in spec.schemas.keys() {
                let rust_type = typify::rust_type_ident(schema_key);
                if let Some(pattern) = &rule.match_.module {
                    let Some(module) = modules.get(&rust_type) else {
                        continue;
                    };
                    if !glob_match(pattern, module) {
                        continue;
                    }
                }
                if let Some(pattern) = &rule.match_.struct_
                    && !glob_match(pattern, schema_key)
                    && !glob_match(pattern, &rust_type)
                {
                    continue;
                }
                matched_pre[index] = true;
                patch.insert(rust_type, forced);
            }
        }

        Ok(FieldRules {
            rules,
            meta,
            deep_patch,
            patch,
            optional,
            matched_pre,
        })
    }

    /// Apply the rules tier's `optional = true` decisions to the lowered
    /// schema: each targeted wire property is removed from its schema's
    /// `required` list (the schema's own and any inline `allOf`
    /// branch's), so typify emits `Option<T>`. `optional = false`
    /// restates the spec, i.e. removes nothing.
    pub(crate) fn apply_optionality(&self, root: &mut schemars::schema::RootSchema) {
        for ((schema_key, wire_name), forced) in &self.optional {
            if !forced {
                continue;
            }
            let Some(schemars::schema::Schema::Object(schema)) =
                root.definitions.get_mut(schema_key)
            else {
                continue;
            };
            unrequire(schema, wire_name);
        }
    }

    /// The rules tier's generation-time deep-patch decisions, for
    /// [`crate::overrides::Overrides::deep_patch_filter_with_rules`].
    pub(crate) fn deep_patch_overrides(&self) -> &BTreeMap<(String, String), bool> {
        &self.deep_patch
    }

    /// The rules tier's type-level patchability decisions, for
    /// [`crate::overrides::Overrides::set_rule_patchability`].
    pub(crate) fn patch_overrides(&self) -> &BTreeMap<String, bool> {
        &self.patch
    }

    /// The post-generation half: evaluate every rule (the resolved
    /// `type` predicate included) against every generated struct
    /// field, layer the payloads in order, overlay the `[fields]`
    /// tier, and warn about rules that never matched. `file` must be
    /// the post-`overrides` AST (field `type` overrides applied).
    pub(crate) fn field_plans(
        &self,
        file: &syn::File,
        style: &StyleConfig,
    ) -> Result<FieldPlans> {
        if self.rules.is_empty() && style.fields.is_empty() {
            return Ok(FieldPlans::default());
        }

        // The `[fields]` tier, keyed like the AST walk sees it.
        let mut field_tier: BTreeMap<(String, String), &crate::config::FieldOverride> =
            BTreeMap::new();
        for (selector, override_) in &style.fields {
            if let Some((type_part, field_part)) = selector.split_once('.') {
                field_tier.insert(
                    (
                        typify::rust_type_ident(type_part),
                        typify::rust_field_ident(field_part),
                    ),
                    override_,
                );
            }
        }

        let mut plans: BTreeMap<(String, String), FieldPlan> = BTreeMap::new();
        let mut matched_post = vec![false; self.rules.len()];
        self.walk_fields(&file.items, &mut |owner, field, ty| {
            let key = (owner.to_string(), field.to_string());
            let meta = self.meta.get(&key);
            let type_tokens = unwrapped_type_tokens(ty);

            // Layer the rules in order.
            let mut attrs: Option<(Vec<syn::Attribute>, usize)> = None;
            let mut type_override: Option<(usize, &ResolvedTypeOverride)> = None;
            let mut impls: Option<(BTreeSet<Capability>, usize)> = None;
            for (index, rule) in self.rules.iter().enumerate() {
                if !self.matches_post(&rule.match_, &key, meta, &type_tokens) {
                    continue;
                }
                matched_post[index] = true;
                if let Some(rule_attrs) = &rule.attrs {
                    attrs = Some((rule_attrs.clone(), index));
                }
                if let Some(override_) = &rule.type_override {
                    type_override = Some((index, override_));
                    // The override's attrs and capabilities ride with
                    // the new type (the old ones described the old
                    // type); a later rule's keys still override.
                    attrs = Some((override_.attrs.clone(), index));
                    impls = Some((override_.capabilities.clone(), index));
                }
                if let Some(rule_impls) = &rule.impls {
                    impls = Some((rule_impls.clone(), index));
                }
            }

            // Overlay the `[fields]` tier, key by key.
            let mut replace_type = type_override.map(|(_, o)| o.path.clone());
            let mut capabilities = impls
                .map(|(caps, index)| (caps, format!("[[rules]] #{index}")));
            let mut final_attrs = attrs.map(|(list, _)| list);
            if let Some(override_) = field_tier.get(&key) {
                if let Some(replacement) = &override_.type_path {
                    // Applied earlier by `overrides::apply_to_file`;
                    // a rule-applied type never displaces it.
                    replace_type = None;
                    final_attrs = Some(
                        crate::mappings::parse_attr_bodies(
                            &format!("[fields] {owner}.{field}"),
                            "type.field-attrs",
                            replacement.field_attrs(),
                        )?,
                    );
                    capabilities = Some((
                        replacement.capabilities().iter().copied().collect(),
                        format!("[fields] {owner}.{field} type"),
                    ));
                }
                if let Some(bodies) = &override_.field_attrs {
                    final_attrs = Some(crate::mappings::parse_attr_bodies(
                        &format!("[fields] {owner}.{field}"),
                        "field-attrs",
                        bodies,
                    )?);
                }
            }

            if final_attrs.is_some() || replace_type.is_some() || capabilities.is_some() {
                plans.insert(
                    key,
                    FieldPlan {
                        attrs: final_attrs,
                        replace_type,
                        capabilities,
                    },
                );
            }
            Ok(())
        })?;

        for (index, rule) in self.rules.iter().enumerate() {
            if !self.matched_pre[index] && !matched_post[index] {
                eprintln!(
                    "openapi-codegen: warning: [[rules]] #{index} matched no field \
                     (predicates: {})",
                    describe_match(&rule.match_),
                );
            }
        }
        Ok(FieldPlans { plans })
    }

    /// Every predicate of `rule` against one generated field.
    fn matches_post(
        &self,
        m: &RuleMatch,
        key: &(String, String),
        meta: Option<&RowMeta>,
        type_tokens: &str,
    ) -> bool {
        if let Some(pattern) = &m.module {
            let Some(module) = meta.and_then(|row| row.module.as_deref()) else {
                return false;
            };
            if !glob_match(pattern, module) {
                return false;
            }
        }
        if let Some(pattern) = &m.struct_ {
            let schema = meta.map(|row| row.schema_key.as_str());
            if !glob_match(pattern, &key.0)
                && !schema.is_some_and(|schema| glob_match(pattern, schema))
            {
                return false;
            }
        }
        if let Some(pattern) = &m.field {
            let wire = meta.map(|row| row.wire_name.as_str());
            if !glob_match(pattern, &key.1)
                && !wire.is_some_and(|wire| glob_match(pattern, wire))
            {
                return false;
            }
        }
        if let Some(pattern) = &m.format {
            let Some(format) = meta.and_then(|row| row.format.as_deref()) else {
                return false;
            };
            if !glob_match(pattern, format) {
                return false;
            }
        }
        if let Some(pattern) = &m.type_ {
            let pattern = pattern.strip_prefix("::").unwrap_or(pattern);
            if !glob_match(pattern, type_tokens) {
                return false;
            }
        }
        true
    }

    /// Walk every named struct field of the file.
    fn walk_fields(
        &self,
        items: &[syn::Item],
        visit: &mut impl FnMut(&str, &str, &syn::Type) -> Result<()>,
    ) -> Result<()> {
        for item in items {
            match item {
                syn::Item::Mod(module) => {
                    if let Some((_, children)) = &module.content {
                        self.walk_fields(children, visit)?;
                    }
                }
                syn::Item::Struct(item_struct) => {
                    let owner = item_struct.ident.to_string();
                    for field in &item_struct.fields {
                        if let Some(ident) = &field.ident {
                            visit(&owner, &ident.to_string(), &field.ty)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Remove `property` from a schema object's `required` list, and from
/// any inline `allOf` branch's (referenced branches are separate named
/// definitions and get their own rows).
fn unrequire(schema: &mut schemars::schema::SchemaObject, property: &str) {
    if let Some(object) = &mut schema.object {
        object.required.remove(property);
    }
    if let Some(subschemas) = &mut schema.subschemas
        && let Some(branches) = &mut subschemas.all_of
    {
        for branch in branches {
            if let schemars::schema::Schema::Object(branch) = branch {
                unrequire(branch, property);
            }
        }
    }
}

/// The pre-generation subset of the predicates (everything except
/// `type`) against one spec row.
fn matches_pre(m: &RuleMatch, row: &RowMeta) -> bool {
    if let Some(pattern) = &m.module {
        let Some(module) = &row.module else {
            return false;
        };
        if !glob_match(pattern, module) {
            return false;
        }
    }
    if let Some(pattern) = &m.struct_ {
        let rust = typify::rust_type_ident(&row.schema_key);
        if !glob_match(pattern, &row.schema_key) && !glob_match(pattern, &rust) {
            return false;
        }
    }
    if let Some(pattern) = &m.field {
        let rust = typify::rust_field_ident(&row.wire_name);
        if !glob_match(pattern, &row.wire_name) && !glob_match(pattern, &rust) {
            return false;
        }
    }
    if let Some(pattern) = &m.format {
        let Some(format) = &row.format else {
            return false;
        };
        if !glob_match(pattern, format) {
            return false;
        }
    }
    true
}

fn resolve_type_override(
    origin: &str,
    replacement: &TypeReplacement,
) -> Result<ResolvedTypeOverride> {
    let path = replacement.type_path().to_string();
    syn::parse_str::<syn::Type>(&path).map_err(|error| {
        anyhow::anyhow!("{origin}: `type` value {path:?} is not a valid Rust type: {error}")
    })?;
    Ok(ResolvedTypeOverride {
        attrs: crate::mappings::parse_attr_bodies(
            origin,
            "type.field-attrs",
            replacement.field_attrs(),
        )?,
        capabilities: replacement.capabilities().iter().copied().collect(),
        path,
    })
}

/// Module map: partition placement (schema keys) with `[types] module`
/// overrides on top, keyed by Rust type name. Empty when partitioning
/// is off.
fn module_map(style: &StyleConfig, partition: Option<&Partition>) -> BTreeMap<String, String> {
    let mut modules: BTreeMap<String, String> = BTreeMap::new();
    if let Some(partition) = partition {
        for (schema_key, module) in &partition.by_schema {
            modules.insert(typify::rust_type_ident(schema_key), module.clone());
        }
        for (selector, override_) in &style.types {
            if let Some(module) = &override_.module {
                modules.insert(typify::rust_type_ident(selector), module.clone());
            }
        }
    }
    modules
}

/// Index every named schema's properties: Rust-name keys, spec names,
/// `"type/format"` provenance, and module placement. Properties come
/// from the schema itself and its `allOf` branches; provenance
/// resolves one `$ref` hop to a named schema. Inline/anonymous
/// sub-schemas (typify synthesizes their types) carry no row and never
/// match `module`/`format` predicates.
fn index_spec(
    spec: &Spec,
    modules: &BTreeMap<String, String>,
) -> BTreeMap<(String, String), RowMeta> {
    let mut meta = BTreeMap::new();
    for (schema_key, schema) in &spec.schemas {
        let rust_type = typify::rust_type_ident(schema_key);
        let module = modules.get(&rust_type).cloned();
        let mut properties: Vec<(&String, &Schema)> = schema.properties.iter().collect();
        for branch in &schema.all_of {
            if branch.reference.is_none() {
                properties.extend(branch.properties.iter());
            }
        }
        for (wire_name, property) in properties {
            meta.insert(
                (rust_type.clone(), typify::rust_field_ident(wire_name)),
                RowMeta {
                    schema_key: schema_key.clone(),
                    wire_name: wire_name.clone(),
                    format: provenance(spec, property),
                    module: module.clone(),
                },
            );
        }
    }
    meta
}

/// A property's `"type"` / `"type/format"` provenance, resolving one
/// `$ref` hop to a named schema.
fn provenance(spec: &Spec, property: &Schema) -> Option<String> {
    let resolved = match &property.reference {
        Some(reference) => {
            let name = reference.rsplit('/').next()?;
            spec.schemas.get(name)?
        }
        None => property,
    };
    let ty = resolved.ty?;
    Some(match &resolved.format {
        Some(format) => format!("{}/{format}", ty.as_str()),
        None => ty.as_str().to_string(),
    })
}

/// The field's constraint-relevant type rendering for the `type`
/// predicate: `Option`/`Box` unwrapped, whitespace-free, leading `::`
/// stripped — `::std::option::Option<::time::OffsetDateTime>` matches
/// as `time::OffsetDateTime`.
fn unwrapped_type_tokens(ty: &syn::Type) -> String {
    let mut inner = ty;
    if let Some(unwrapped) = crate::mappings::unwrap_wrapper(inner, "Option") {
        inner = unwrapped;
    }
    if let Some(unwrapped) = crate::mappings::unwrap_wrapper(inner, "Box") {
        inner = unwrapped;
    }
    let tokens = inner.to_token_stream().to_string().replace(' ', "");
    tokens.strip_prefix("::").unwrap_or(&tokens).to_string()
}

/// Human-readable predicate list for the zero-match warning.
fn describe_match(m: &RuleMatch) -> String {
    let mut parts = Vec::new();
    if let Some(v) = &m.module {
        parts.push(format!("module = {v:?}"));
    }
    if let Some(v) = &m.struct_ {
        parts.push(format!("struct = {v:?}"));
    }
    if let Some(v) = &m.field {
        parts.push(format!("field = {v:?}"));
    }
    if let Some(v) = &m.format {
        parts.push(format!("format = {v:?}"));
    }
    if let Some(v) = &m.type_ {
        parts.push(format!("type = {v:?}"));
    }
    parts.join(", ")
}

/// Minimal glob matching: `*` matches any sequence (empty included,
/// crossing `/` and `::`), `?` any single character; everything else
/// is literal and case-sensitive. No character classes.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(pattern: &[char], text: &[char]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some(('*', rest)) => {
                (0..=text.len()).any(|skip| inner(rest, &text[skip..]))
            }
            Some(('?', rest)) => text
                .split_first()
                .is_some_and(|(_, text_rest)| inner(rest, text_rest)),
            Some((expected, rest)) => text
                .split_first()
                .is_some_and(|(actual, text_rest)| actual == expected && inner(rest, text_rest)),
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    inner(&pattern, &text)
}

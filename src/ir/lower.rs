//! `Spec → Ir`: the schema compiler.
//!
//! Ports the schema semantics the pipeline actually needs (see
//! docs/MIGRATION.md D5) from the typify fork, over the typed
//! [`Spec`](crate::spec::Spec) model:
//!
//! - named-schema classification (structs, string enums, untagged enums,
//!   aliases that vanish at use sites),
//! - `allOf` composition (`$ref` bases → `#[serde(flatten)]` fields,
//!   sibling-properties pattern, pragmatic merge fallback),
//! - `oneOf`/`anyOf` untagged enums and the `[T, null]` → `Option<T>`
//!   pattern (3.0 `nullable: true` arrives pre-normalized on the node),
//! - enum-of-strings with typify's exact variant naming (including the
//!   collision fallback),
//! - inline-schema naming (`{Parent}{Property}`, `…Item` for array
//!   items),
//! - reference cycles broken with `Box` (see [`box_cycles`]),
//! - name collisions as loud errors instead of typify's silent
//!   first-wins reuse (D11).
//!
//! Anything outside the modeled subset fails with the schema's
//! [`Origin`]; the documented workaround is the patch mechanism.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail};
use heck::{ToPascalCase, ToSnakeCase};
use unicode_ident::{is_xid_continue, is_xid_start};

use crate::Result;
use crate::config::{AllOfMode, StyleConfig};
use crate::spec::{AdditionalProperties, Origin, Schema, Spec, TypeHint};

use super::{
    EnumVariant, FieldDef, Ir, OperationIr, Shape, StringEnumShape, StructShape, TypeDef,
    TypeRef, UntaggedShape, UntaggedVariant,
};

/// Identifier case for [`sanitize`], matching typify's `util::Case`.
#[derive(Clone, Copy)]
enum Case {
    Pascal,
    Snake,
}

/// Port of typify's `util::sanitize` — the exact naming rules the golden
/// output was generated with.
fn sanitize(input: &str, case: Case) -> String {
    let to_case = |value: &str| match case {
        Case::Pascal => value.to_pascal_case(),
        Case::Snake => value.to_snake_case(),
    };

    // If every case was special then none of them would be.
    let out = match input {
        "+1" => "plus1".to_string(),
        "-1" => "minus1".to_string(),
        _ => to_case(
            &input
                .replace('\'', "")
                .replace(|c: char| !is_xid_continue(c), "-"),
        ),
    };

    let prefix = to_case("x");
    let out = match out.chars().next() {
        None => prefix,
        Some(c) if is_xid_start(c) => out,
        Some(_) => format!("{prefix}{out}"),
    };

    if accept_as_ident(&out) {
        out
    } else {
        format!("{out}_")
    }
}

/// Port of typify's keyword check (note `gen` in the list).
fn accept_as_ident(ident: &str) -> bool {
    !matches!(
        ident,
        "_" | "abstract"
            | "as"
            | "async"
            | "await"
            | "become"
            | "box"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "do"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "final"
            | "fn"
            | "for"
            | "gen"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "macro"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "override"
            | "priv"
            | "pub"
            | "ref"
            | "return"
            | "Self"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "try"
            | "type"
            | "typeof"
            | "unsafe"
            | "unsized"
            | "use"
            | "virtual"
            | "where"
            | "while"
            | "yield"
    )
}

/// How a `$ref` to a named schema reads at a use site.
#[derive(Clone)]
enum RefState {
    /// Classification in progress — hitting this means a pure alias
    /// cycle (`A: $ref B`, `B: $ref A`), which has no representation.
    InProgress,
    Done(TypeRef),
}

struct Lowerer<'a> {
    spec: &'a Spec,
    style: &'a StyleConfig,
    /// Finished type definitions, keyed by Rust name.
    types: BTreeMap<String, TypeDef>,
    /// Use-site meaning of each named schema.
    ref_targets: BTreeMap<String, RefState>,
}

/// Lower a normalized [`Spec`] into the [`Ir`]. The result's types carry
/// shapes and provenance only; the passes fill in style decisions.
pub fn lower_spec(spec: &Spec, style: &StyleConfig) -> Result<Ir> {
    let mut lowerer = Lowerer {
        spec,
        style,
        types: BTreeMap::new(),
        ref_targets: BTreeMap::new(),
    };

    // Phase A: classify every named schema (shallow — establishes each
    // schema's use-site meaning and Rust name without lowering bodies).
    let keys: Vec<&String> = spec.schemas.keys().collect();
    for key in &keys {
        lowerer.classify_named(key)?;
    }

    // Phase B: lower every named schema's body.
    for key in &keys {
        lowerer.lower_named(key)?;
    }

    let operations = lowerer.summarize_operations();

    let mut ir = Ir {
        types: lowerer.types.into_values().collect(),
        operations,
        ..Ir::default()
    };
    box_cycles(&mut ir);
    Ok(ir)
}

impl<'a> Lowerer<'a> {
    fn schema(&self, key: &str) -> Result<&'a Schema> {
        self.spec
            .schemas
            .get(key)
            .with_context(|| format!("$ref to unknown schema {key:?}"))
    }

    /// The use-site meaning of `$ref: #/components/schemas/<key>`,
    /// memoized. Struct/enum shapes classify as `Named` without
    /// recursion; alias chains resolve recursively.
    fn classify_named(&mut self, key: &str) -> Result<TypeRef> {
        match self.ref_targets.get(key) {
            Some(RefState::Done(target)) => return Ok(target.clone()),
            Some(RefState::InProgress) => {
                bail!("alias cycle through schema {key:?} (pure $ref/allOf loops have no type)")
            }
            None => {}
        }
        self.ref_targets
            .insert(key.to_string(), RefState::InProgress);

        let schema = self.schema(key)?;
        let rust_name = sanitize(key, Case::Pascal);
        let target = self
            .classify_schema(schema, &rust_name)
            .with_context(|| format!("in schema {}", schema.origin))?;

        self.ref_targets
            .insert(key.to_string(), RefState::Done(target.clone()));
        Ok(target)
    }

    /// Shallow classification of one schema node: what does a reference
    /// to it mean? `rust_name` is the name the node's type would take.
    fn classify_schema(&mut self, schema: &Schema, rust_name: &str) -> Result<TypeRef> {
        let base = if let Some(reference) = &schema.reference {
            let target_key = reference_key(reference, &schema.origin)?;
            self.classify_named(&target_key)?
        } else if !schema.all_of.is_empty() {
            match singleton_all_of(schema) {
                // `allOf: [single]` with nothing else: the subschema in
                // place, under the outer name (typify's
                // maybe_singleton_subschema).
                Some(single) => self.classify_schema(single, rust_name)?,
                None => TypeRef::Named(rust_name.to_string()),
            }
        } else if !schema.one_of.is_empty() || !schema.any_of.is_empty() {
            let subschemas = if schema.one_of.is_empty() {
                &schema.any_of
            } else {
                &schema.one_of
            };
            match option_via_null(subschemas) {
                Some(inner) => self.classify_schema(inner, rust_name)?.optional(),
                None => TypeRef::Named(rust_name.to_string()),
            }
        } else if !schema.enumeration.is_empty() {
            TypeRef::Named(rust_name.to_string())
        } else if is_object_like(schema) {
            if schema.properties.is_empty()
                && let Some(AdditionalProperties::Schema(value_schema)) =
                    &schema.additional_properties
            {
                // A pure map: `{ additionalProperties: <schema> }`.
                let value = self.lower_use_site(
                    value_schema,
                    &format!("{rust_name}Value"),
                    &value_schema.origin,
                )?;
                TypeRef::Map(Box::new(TypeRef::String), Box::new(value))
            } else {
                TypeRef::Named(rust_name.to_string())
            }
        } else {
            // Scalar / array / empty: an alias — resolve the target
            // expression now (may create synthetic item types).
            self.scalar_or_array_ref(schema, rust_name)?
        };

        Ok(if schema.nullable { base.optional() } else { base })
    }

    /// The alias target for scalar, array, and empty schemas.
    fn scalar_or_array_ref(&mut self, schema: &Schema, rust_name: &str) -> Result<TypeRef> {
        let effective_type = effective_type(schema);
        Ok(match effective_type {
            Some(TypeHint::String) => self.string_ref(schema),
            Some(TypeHint::Integer) => integer_ref(schema),
            Some(TypeHint::Number) => TypeRef::F64,
            Some(TypeHint::Boolean) => TypeRef::Bool,
            Some(TypeHint::Null) => TypeRef::Unit,
            Some(TypeHint::Array) => {
                let item = match &schema.items {
                    Some(items) => self.lower_use_site(
                        items,
                        &format!("{rust_name}Item"),
                        &items.origin,
                    )?,
                    None => TypeRef::JsonValue,
                };
                TypeRef::Vec(Box::new(item))
            }
            Some(TypeHint::Object) => unreachable!("object schemas classify as Named"),
            None => TypeRef::JsonValue,
        })
    }

    /// The Rust type for a string schema: format overrides from the
    /// style config, everything else plain `String` (constraint
    /// validation is not a supported mode — config validation rejects
    /// `validate` before lowering runs).
    fn string_ref(&self, schema: &Schema) -> TypeRef {
        let override_for = |configured: &Option<String>, upstream_default: &str| {
            TypeRef::Custom(
                configured
                    .clone()
                    .unwrap_or_else(|| upstream_default.to_string()),
            )
        };
        match schema.format.as_deref() {
            Some("date") => override_for(&self.style.date, "::chrono::naive::NaiveDate"),
            Some("date-time") => override_for(
                &self.style.date_time,
                "::chrono::DateTime<::chrono::offset::Utc>",
            ),
            Some("uuid") => override_for(&self.style.uuid, "::uuid::Uuid"),
            _ => TypeRef::String,
        }
    }

    /// Fully lower one named schema into its `TypeDef` (no-op for
    /// schemas that classified as non-`Named` aliases).
    fn lower_named(&mut self, key: &str) -> Result<()> {
        let Some(RefState::Done(target)) = self.ref_targets.get(key).cloned() else {
            unreachable!("phase A classified every named schema");
        };

        let schema = self.schema(key)?;
        let rust_name = sanitize(key, Case::Pascal);

        // Only schemas whose classification is exactly `Named(their own
        // name)` (possibly nullable-wrapped) own a definition; pure
        // aliases record an Alias TypeDef for lookups but emit nothing.
        let named_self = matches!(
            &target,
            TypeRef::Named(name) if name == &rust_name
        ) || matches!(
            &target,
            TypeRef::Option(inner)
                if matches!(inner.as_ref(), TypeRef::Named(name) if name == &rust_name)
        );

        if !named_self {
            let def = TypeDef::new(
                rust_name.clone(),
                Some(key.to_string()),
                schema.origin.clone(),
                schema.description.clone(),
                Shape::Alias(target),
            );
            self.register(def)?;
            return Ok(());
        }

        let mut def = self
            .lower_shape(schema, &rust_name)
            .with_context(|| format!("in schema {}", schema.origin))?;
        def.schema_key = Some(key.to_string());
        self.register(def)?;
        Ok(())
    }

    /// Register a finished type definition; name collisions are errors
    /// (typify silently reused the first definition — D11).
    fn register(&mut self, def: TypeDef) -> Result<()> {
        if let Some(existing) = self.types.get(&def.name) {
            bail!(
                "type name {:?} generated for both {} and {} — rename one schema \
                 (via a patch) or move one type",
                def.name,
                existing.origin,
                def.origin,
            );
        }
        self.types.insert(def.name.clone(), def);
        Ok(())
    }

    /// Fully lower a schema that owns a definition (named or synthetic)
    /// into a `TypeDef` with a Struct / StringEnum / Untagged shape.
    fn lower_shape(&mut self, schema: &Schema, rust_name: &str) -> Result<TypeDef> {
        // allOf singleton: the subschema in place, with the outer
        // schema's name and description.
        if !schema.all_of.is_empty()
            && let Some(single) = singleton_all_of(schema)
        {
            let mut def = self.lower_shape(single, rust_name)?;
            if let Some(description) = &schema.description {
                def.description = Some(description.clone());
            }
            def.origin = schema.origin.clone();
            return Ok(def);
        }

        let shape = if !schema.all_of.is_empty() {
            self.lower_all_of(schema, rust_name)?
        } else if !schema.one_of.is_empty() {
            self.lower_untagged(schema, &schema.one_of.clone(), rust_name)?
        } else if !schema.any_of.is_empty() {
            self.lower_untagged(schema, &schema.any_of.clone(), rust_name)?
        } else if !schema.enumeration.is_empty() {
            Shape::StringEnum(self.lower_string_enum(schema)?)
        } else if is_object_like(schema) {
            Shape::Struct(self.lower_struct(schema, rust_name, &[])?)
        } else {
            bail!(
                "schema at {} does not lower to a definition (unexpected shape)",
                schema.origin,
            );
        };

        Ok(TypeDef::new(
            rust_name.to_string(),
            None,
            schema.origin.clone(),
            schema.description.clone(),
            shape,
        ))
    }

    /// `allOf` handling for struct-producing shapes: Compose when
    /// configured and the shape allows, pragmatic merge otherwise.
    fn lower_all_of(&mut self, schema: &Schema, rust_name: &str) -> Result<Shape> {
        if self.style.allof == AllOfMode::Compose
            && let Some(shape) = self.try_compose(schema, rust_name)?
        {
            return Ok(shape);
        }
        self.merge_all_of(schema, rust_name)
    }

    /// The Compose strategy, ported from the fork: `$ref` subschemas
    /// become `#[serde(flatten)]` base fields; inline object subschemas
    /// and the outer schema's sibling properties contribute ordinary
    /// fields. Returns `None` (→ merge fallback) when the shape doesn't
    /// compose cleanly.
    fn try_compose(&mut self, schema: &Schema, rust_name: &str) -> Result<Option<Shape>> {
        let mut ref_bases: Vec<String> = Vec::new();
        let mut inline: Vec<&Schema> = Vec::new();
        for sub in &schema.all_of {
            if let Some(reference) = &sub.reference {
                let Ok(key) = reference_key(reference, &sub.origin) else {
                    return Ok(None);
                };
                ref_bases.push(key);
            } else if is_inline_object(sub) {
                inline.push(sub);
            } else {
                return Ok(None);
            }
        }
        if ref_bases.is_empty() {
            return Ok(None);
        }

        // The outer schema's own properties join the inline set (the
        // Swagger-conversion sibling-properties pattern).
        let sibling = (!schema.properties.is_empty() || !schema.required.is_empty()).then(|| {
            let mut synthetic = Schema {
                origin: schema.origin.clone(),
                ..Schema::default()
            };
            synthetic.ty = Some(TypeHint::Object);
            synthetic.properties = schema.properties.clone();
            synthetic.required = schema.required.clone();
            synthetic
        });

        let mut fields: Vec<FieldDef> = Vec::new();
        let mut used_names: BTreeSet<String> = BTreeSet::new();

        for base_key in &ref_bases {
            let target = self.classify_named(base_key)?;
            let field_name = sanitize(base_key, Case::Snake);
            if !used_names.insert(field_name.clone()) {
                return Ok(None);
            }
            fields.push(FieldDef {
                rust_name: field_name,
                wire_name: base_key.clone(),
                ty: target,
                required: true,
                flatten: true,
                description: None,
                default: None,
                serde_options: Vec::new(),
                patch_type: None,
                origin: schema.origin.clone(),
            });
        }

        // Merge the inline subschemas (plus the sibling synthetic) into
        // one property set.
        let mut inline_all: Vec<&Schema> = inline;
        if let Some(sibling) = &sibling {
            inline_all.push(sibling);
        }
        let merged = merge_object_schemas(&inline_all, &schema.origin)?;
        let inline_fields = self.lower_struct(&merged, rust_name, &[])?;
        for field in inline_fields.fields {
            if !used_names.insert(field.rust_name.clone()) {
                // Inline property collides with a flattened base field
                // name; the merge path can collapse them, compose can't.
                return Ok(None);
            }
            fields.push(field);
        }

        // `#[serde(flatten)]` is incompatible with
        // `deny_unknown_fields`, so composed structs never deny.
        Ok(Some(Shape::Struct(StructShape {
            fields,
            deny_unknown_fields: false,
        })))
    }

    /// The pragmatic merge fallback: resolve `$ref` subschemas to their
    /// object bodies, union properties and required lists, and lower the
    /// result as one flat struct. Not typify's full JSON-Schema
    /// intersection (docs/MIGRATION.md D5): colliding same-name
    /// properties must be identical, and non-object subschemas are
    /// errors.
    fn merge_all_of(&mut self, schema: &Schema, rust_name: &str) -> Result<Shape> {
        let mut resolved: Vec<&Schema> = Vec::new();
        for sub in &schema.all_of {
            resolved.push(self.resolve_to_object(sub)?);
        }
        let sibling = (!schema.properties.is_empty() || !schema.required.is_empty()).then(|| {
            let mut synthetic = Schema {
                origin: schema.origin.clone(),
                ..Schema::default()
            };
            synthetic.ty = Some(TypeHint::Object);
            synthetic.properties = schema.properties.clone();
            synthetic.required = schema.required.clone();
            synthetic
        });
        if let Some(sibling) = &sibling {
            resolved.push(sibling);
        }

        let merged = merge_object_schemas(&resolved, &schema.origin)?;
        Ok(Shape::Struct(self.lower_struct(&merged, rust_name, &[])?))
    }

    /// Chase `$ref` / singleton-`allOf` chains until an object schema.
    fn resolve_to_object(&self, schema: &'a Schema) -> Result<&'a Schema> {
        if let Some(reference) = &schema.reference {
            let key = reference_key(reference, &schema.origin)?;
            let target = self.schema(&key)?;
            return self.resolve_to_object(target);
        }
        if let Some(single) = singleton_all_of(schema) {
            return self.resolve_to_object(single);
        }
        if is_object_like(schema) {
            Ok(schema)
        } else {
            bail!(
                "allOf merge requires object subschemas; {} is not an object \
                 (patch the spec or use compose-compatible shapes)",
                schema.origin,
            )
        }
    }

    /// Lower an object schema's properties into struct fields, sorted by
    /// wire name (`extra_fields` lets compose prepend flatten bases).
    fn lower_struct(
        &mut self,
        schema: &Schema,
        rust_name: &str,
        extra_fields: &[FieldDef],
    ) -> Result<StructShape> {
        let mut fields: Vec<FieldDef> = extra_fields.to_vec();
        let mut seen: BTreeSet<String> =
            fields.iter().map(|field| field.rust_name.clone()).collect();
        let required: BTreeSet<&String> = schema.required.iter().collect();

        for (wire_name, property) in &schema.properties {
            let field_name = sanitize(wire_name, Case::Snake);
            if !seen.insert(field_name.clone()) {
                bail!(
                    "properties {:?} at {} collide on Rust field name {field_name:?} \
                     (rename one via a patch)",
                    wire_name,
                    schema.origin,
                );
            }
            let suggested = format!("{rust_name}{}", sanitize(wire_name, Case::Pascal));
            let ty = self.lower_use_site(property, &suggested, &property.origin)?;
            fields.push(FieldDef {
                rust_name: field_name,
                wire_name: wire_name.clone(),
                ty,
                required: required.contains(wire_name),
                flatten: false,
                description: property.description.clone(),
                default: property.default.clone(),
                serde_options: Vec::new(),
                patch_type: None,
                origin: property.origin.clone(),
            });
        }

        let deny_unknown_fields = matches!(
            schema.additional_properties,
            Some(AdditionalProperties::Allowed(false)),
        );

        Ok(StructShape {
            fields,
            deny_unknown_fields,
        })
    }

    /// Lower a string enum: values must be strings; variant naming
    /// (including the ambiguity fallback) ports typify's rules.
    fn lower_string_enum(&mut self, schema: &Schema) -> Result<StringEnumShape> {
        if let Some(ty) = schema.ty
            && ty != TypeHint::String
        {
            bail!(
                "enum at {} has non-string type {:?}; only string enums are modeled",
                schema.origin,
                ty.as_str(),
            );
        }

        let mut raw_names = Vec::new();
        for value in &schema.enumeration {
            let raw = value.as_str().with_context(|| {
                format!(
                    "enum value {value} at {} is not a string (patch the spec)",
                    schema.origin,
                )
            })?;
            raw_names.push(raw.to_string());
        }

        let mut idents: Vec<String> = raw_names
            .iter()
            .map(|raw| sanitize(raw, Case::Pascal))
            .collect();
        // Typify's collision fallback: if sanitized names collide, keep
        // the elided characters visible as 'X'.
        if !all_unique(&idents) {
            idents = raw_names
                .iter()
                .map(|raw| {
                    sanitize(
                        &raw.replace(|c: char| c == '_' || !is_xid_continue(c), "X"),
                        Case::Pascal,
                    )
                })
                .collect();
        }
        if !all_unique(&idents) {
            bail!(
                "enum variants at {} do not sanitize to unique Rust names: {:?}",
                schema.origin,
                raw_names,
            );
        }

        let schema_default = match &schema.default {
            None => None,
            Some(serde_json::Value::String(value)) => {
                if !raw_names.iter().any(|raw| raw == value) {
                    bail!(
                        "enum default {value:?} at {} is not one of the enum values",
                        schema.origin,
                    );
                }
                Some(value.clone())
            }
            Some(other) => bail!(
                "enum default {other} at {} is not a string",
                schema.origin,
            ),
        };

        Ok(StringEnumShape {
            variants: raw_names
                .into_iter()
                .zip(idents)
                .map(|(raw_name, ident_name)| EnumVariant {
                    raw_name,
                    ident_name,
                    description: None,
                })
                .collect(),
            schema_default,
        })
    }

    /// Lower `oneOf`/`anyOf` into an untagged enum (the `[T, null]`
    /// Option pattern is handled by the callers' classification).
    fn lower_untagged(
        &mut self,
        schema: &Schema,
        subschemas: &[Schema],
        rust_name: &str,
    ) -> Result<Shape> {
        if let Some(inner) = option_via_null(subschemas) {
            // Named `[T, null]`: the inner shape under the outer name.
            let def = self.lower_shape(inner, rust_name)?;
            return Ok(def.shape);
        }

        let mut variants = Vec::new();
        let mut idents = BTreeSet::new();
        for (index, sub) in subschemas.iter().enumerate() {
            let ident_name = match &sub.title {
                Some(title) => sanitize(title, Case::Pascal),
                None => format!("Variant{index}"),
            };
            if !idents.insert(ident_name.clone()) {
                bail!(
                    "untagged variants at {} do not have unique names (add titles \
                     via a patch)",
                    schema.origin,
                );
            }
            let suggested = format!("{rust_name}{ident_name}");
            let ty = self.lower_use_site(sub, &suggested, &sub.origin)?;
            variants.push(UntaggedVariant {
                ident_name,
                ty,
                description: sub.description.clone(),
            });
        }
        Ok(Shape::Untagged(UntaggedShape { variants }))
    }

    /// Lower a schema in a use position (property type, array item,
    /// untagged payload) to a `TypeRef`, creating synthetic named types
    /// for inline compound schemas.
    fn lower_use_site(
        &mut self,
        schema: &Schema,
        suggested_name: &str,
        origin: &Origin,
    ) -> Result<TypeRef> {
        let base = if let Some(reference) = &schema.reference {
            let key = reference_key(reference, origin)?;
            self.classify_named(&key)?
        } else if !schema.all_of.is_empty()
            || is_object_like(schema)
            || !schema.enumeration.is_empty()
            || !schema.one_of.is_empty()
            || !schema.any_of.is_empty()
        {
            // Inline compound schema: singleton allOf and [T, null]
            // patterns resolve structurally; everything else becomes a
            // synthetic named type.
            if let Some(single) = singleton_all_of(schema) {
                self.lower_use_site(single, suggested_name, origin)?
            } else if let Some(inner) = option_via_null(&schema.one_of) {
                self.lower_use_site(inner, suggested_name, origin)?.optional()
            } else if let Some(inner) = option_via_null(&schema.any_of) {
                self.lower_use_site(inner, suggested_name, origin)?.optional()
            } else if is_object_like(schema)
                && schema.properties.is_empty()
                && schema.all_of.is_empty()
                && schema.one_of.is_empty()
                && schema.any_of.is_empty()
            {
                match &schema.additional_properties {
                    Some(AdditionalProperties::Schema(value_schema)) => {
                        let value = self.lower_use_site(
                            value_schema,
                            &format!("{suggested_name}Value"),
                            &value_schema.origin,
                        )?;
                        TypeRef::Map(Box::new(TypeRef::String), Box::new(value))
                    }
                    // `{type: object}` with no properties: a free-form
                    // map of JSON values (typify's shape for it).
                    _ => TypeRef::Map(
                        Box::new(TypeRef::String),
                        Box::new(TypeRef::JsonValue),
                    ),
                }
            } else {
                let def = self.lower_shape(schema, suggested_name)?;
                self.register(def)?;
                TypeRef::Named(suggested_name.to_string())
            }
        } else {
            self.scalar_or_array_ref(schema, suggested_name)?
        };

        Ok(if schema.nullable { base.optional() } else { base })
    }

    /// Summarize operations into the IR (data only; see D8).
    fn summarize_operations(&mut self) -> Vec<OperationIr> {
        self.spec
            .operations
            .iter()
            .map(|op| {
                let mut request_types = Vec::new();
                let mut response_types = Vec::new();
                let mut add = |schema: &Option<Schema>, bucket: &mut Vec<String>| {
                    let Some(schema) = schema else { return };
                    let Some(reference) = &schema.reference else {
                        return;
                    };
                    if let Ok(key) = reference_key(reference, &schema.origin)
                        && let Ok(target) = self.classify_named(&key)
                        && let Some(name) = target.named_target()
                    {
                        bucket.push(name.to_string());
                    }
                };
                for body in &op.request {
                    add(&body.schema, &mut request_types);
                }
                for param in &op.params {
                    add(&param.schema.clone().map(Some).unwrap_or(None), &mut request_types);
                }
                for response in &op.responses {
                    for body in &response.bodies {
                        add(&body.schema, &mut response_types);
                    }
                }
                OperationIr {
                    operation_id: op.operation_id.clone(),
                    method: op.method.clone(),
                    path: op.path.clone(),
                    request_types,
                    response_types,
                }
            })
            .collect()
    }
}

/// `#/components/schemas/<key>` → `<key>`; everything else is
/// unsupported.
fn reference_key(reference: &str, origin: &Origin) -> Result<String> {
    reference
        .strip_prefix("#/components/schemas/")
        .map(|rest| rest.replace("~1", "/").replace("~0", "~"))
        .with_context(|| {
            format!("unsupported $ref {reference:?} at {origin} (only #/components/schemas/)")
        })
}

/// `allOf: [single]` with no sibling properties or other combinators.
fn singleton_all_of(schema: &Schema) -> Option<&Schema> {
    (schema.all_of.len() == 1
        && schema.properties.is_empty()
        && schema.required.is_empty()
        && schema.one_of.is_empty()
        && schema.any_of.is_empty()
        && schema.enumeration.is_empty())
    .then(|| &schema.all_of[0])
}

/// The `[T, {type: null}]` pattern (either order) that lowers to
/// `Option<T>` — the shape 3.0 `nullable: true` normalizes into and 3.1
/// spells natively.
fn option_via_null(subschemas: &[Schema]) -> Option<&Schema> {
    if subschemas.len() != 2 {
        return None;
    }
    let is_null = |schema: &Schema| {
        schema.ty == Some(TypeHint::Null)
            && schema.reference.is_none()
            && schema.properties.is_empty()
            && schema.enumeration.is_empty()
            && schema.all_of.is_empty()
            && schema.one_of.is_empty()
            && schema.any_of.is_empty()
    };
    match (is_null(&subschemas[0]), is_null(&subschemas[1])) {
        (false, true) => Some(&subschemas[0]),
        (true, false) => Some(&subschemas[1]),
        _ => None,
    }
}

/// Object-ness: explicit `type: object`, or object keywords with no
/// scalar type.
fn is_object_like(schema: &Schema) -> bool {
    match schema.ty {
        Some(TypeHint::Object) => true,
        Some(_) => false,
        None => !schema.properties.is_empty() || schema.additional_properties.is_some(),
    }
}

/// The inline subschemas a Compose-strategy allOf can fold in: plain
/// object shapes with no `$ref`, no nested combinators.
fn is_inline_object(schema: &Schema) -> bool {
    schema.reference.is_none()
        && schema.all_of.is_empty()
        && schema.one_of.is_empty()
        && schema.any_of.is_empty()
        && schema.enumeration.is_empty()
        && matches!(schema.ty, None | Some(TypeHint::Object))
}

/// The effective scalar/array type, honoring the `format`-implies-`type`
/// inference the old lowering applied.
fn effective_type(schema: &Schema) -> Option<TypeHint> {
    if let Some(ty) = schema.ty {
        return Some(ty);
    }
    schema.format.as_deref().map(|format| match format {
        "int32" | "int64" => TypeHint::Integer,
        "float" | "double" => TypeHint::Number,
        _ => TypeHint::String,
    })
}

/// Integer format mapping under plain (unconstrained) integers.
fn integer_ref(schema: &Schema) -> TypeRef {
    match schema.format.as_deref() {
        Some("int32") => TypeRef::I32,
        _ => TypeRef::I64,
    }
}

fn all_unique(names: &[String]) -> bool {
    let set: BTreeSet<&String> = names.iter().collect();
    set.len() == names.len()
}

/// Union-merge object schemas: properties (same name must be
/// structurally identical), required lists, and additionalProperties
/// denial. Descriptions are dropped (the outer schema's description
/// belongs to the type, not the merged body).
fn merge_object_schemas(subschemas: &[&Schema], origin: &Origin) -> Result<Schema> {
    let mut merged = Schema {
        origin: origin.clone(),
        ..Schema::default()
    };
    merged.ty = Some(TypeHint::Object);

    for sub in subschemas {
        for (name, property) in &sub.properties {
            match merged.properties.get(name) {
                None => {
                    merged.properties.insert(name.clone(), property.clone());
                }
                Some(existing) if existing.to_draft07() == property.to_draft07() => {}
                Some(existing) => bail!(
                    "allOf merge: property {name:?} defined incompatibly at {} and {} \
                     (patch the spec)",
                    existing.origin,
                    property.origin,
                ),
            }
        }
        for name in &sub.required {
            if !merged.required.contains(name) {
                merged.required.push(name.clone());
            }
        }
        if matches!(
            sub.additional_properties,
            Some(AdditionalProperties::Allowed(false)),
        ) {
            merged.additional_properties = Some(AdditionalProperties::Allowed(false));
        }
    }
    Ok(merged)
}

/// Break reference cycles among named types by boxing every
/// cycle-participating field edge.
///
/// Edges: struct fields and untagged variants whose type reaches a
/// `Named` target without passing through `Vec` or `Map` (those already
/// provide indirection; `Option` does not). Every edge that stays within
/// one strongly-connected component gets `Box`ed. This differs from
/// typify's minimal-boxing choice but is always sufficient and
/// deterministic (docs/MIGRATION.md D5); the fixtures have no cycles, so
/// parity is unaffected.
fn box_cycles(ir: &mut Ir) {
    use std::collections::HashMap;

    // Adjacency over type names, direct (non-indirected) edges only.
    fn direct_target(reference: &TypeRef) -> Option<&str> {
        match reference {
            TypeRef::Named(name) => Some(name),
            TypeRef::Option(inner) | TypeRef::Boxed(inner) => direct_target(inner),
            _ => None,
        }
    }

    // Resolve alias chains to the definition-owning type name.
    fn resolve_name<'a>(names: &'a HashMap<String, usize>, ir: &'a Ir, name: &'a str) -> Option<&'a str> {
        if names.contains_key(name) {
            return Some(name);
        }
        match ir.get(name).map(|def| &def.shape) {
            Some(Shape::Alias(target)) => {
                direct_target(target).and_then(|next| resolve_name(names, ir, next))
            }
            _ => None,
        }
    }

    let names: HashMap<String, usize> = ir
        .types
        .iter()
        .enumerate()
        .filter(|(_, def)| def.emits_item())
        .map(|(index, def)| (def.name.clone(), index))
        .collect();

    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); ir.types.len()];
    for (index, def) in ir.types.iter().enumerate() {
        let mut add_edge = |reference: &TypeRef| {
            if let Some(raw) = direct_target(reference)
                && let Some(resolved) = resolve_name(&names, ir, raw)
                && let Some(&target) = names.get(resolved)
            {
                edges[index].push(target);
            }
        };
        match &def.shape {
            Shape::Struct(shape) => {
                for field in &shape.fields {
                    add_edge(&field.ty);
                }
            }
            Shape::Untagged(shape) => {
                for variant in &shape.variants {
                    add_edge(&variant.ty);
                }
            }
            _ => {}
        }
    }

    // Tarjan SCC.
    struct Tarjan<'a> {
        edges: &'a [Vec<usize>],
        index: Vec<Option<usize>>,
        lowlink: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        counter: usize,
        component: Vec<usize>,
        component_count: usize,
    }
    impl Tarjan<'_> {
        fn strongconnect(&mut self, v: usize) {
            self.index[v] = Some(self.counter);
            self.lowlink[v] = self.counter;
            self.counter += 1;
            self.stack.push(v);
            self.on_stack[v] = true;
            for &w in &self.edges[v].to_vec() {
                if self.index[w].is_none() {
                    self.strongconnect(w);
                    self.lowlink[v] = self.lowlink[v].min(self.lowlink[w]);
                } else if self.on_stack[w] {
                    self.lowlink[v] = self.lowlink[v].min(self.index[w].unwrap());
                }
            }
            if self.lowlink[v] == self.index[v].unwrap() {
                loop {
                    let w = self.stack.pop().unwrap();
                    self.on_stack[w] = false;
                    self.component[w] = self.component_count;
                    if w == v {
                        break;
                    }
                }
                self.component_count += 1;
            }
        }
    }
    let node_count = ir.types.len();
    let mut tarjan = Tarjan {
        edges: &edges,
        index: vec![None; node_count],
        lowlink: vec![0; node_count],
        on_stack: vec![false; node_count],
        stack: Vec::new(),
        counter: 0,
        component: vec![0; node_count],
        component_count: 0,
    };
    for v in 0..node_count {
        if tarjan.index[v].is_none() {
            tarjan.strongconnect(v);
        }
    }
    let component = tarjan.component;

    // Self-loops: a component of size 1 cycles only if it has an edge to
    // itself.
    let mut component_sizes = vec![0usize; node_count];
    for &c in &component {
        component_sizes[c] += 1;
    }
    let self_loop: Vec<bool> = (0..node_count)
        .map(|v| edges[v].contains(&v))
        .collect();

    fn box_reference(reference: &mut TypeRef) {
        match reference {
            TypeRef::Named(_) => {
                let inner = std::mem::replace(reference, TypeRef::Unit);
                *reference = TypeRef::Boxed(Box::new(inner));
            }
            TypeRef::Option(inner) | TypeRef::Boxed(inner) => box_reference(inner),
            _ => {}
        }
    }

    let ir_snapshot = ir.clone();
    for (index, def) in ir.types.iter_mut().enumerate() {
        let needs_box = |reference: &TypeRef| -> bool {
            let Some(raw) = direct_target(reference) else {
                return false;
            };
            let Some(resolved) = resolve_name(&names, &ir_snapshot, raw) else {
                return false;
            };
            let Some(&target) = names.get(resolved) else {
                return false;
            };
            component[target] == component[index]
                && (component_sizes[component[index]] > 1 || self_loop[index])
        };
        match &mut def.shape {
            Shape::Struct(shape) => {
                for field in &mut shape.fields {
                    if !matches!(field.ty, TypeRef::Boxed(_)) && needs_box(&field.ty) {
                        box_reference(&mut field.ty);
                    }
                }
            }
            Shape::Untagged(shape) => {
                for variant in &mut shape.variants {
                    if !matches!(variant.ty, TypeRef::Boxed(_)) && needs_box(&variant.ty) {
                        box_reference(&mut variant.ty);
                    }
                }
            }
            _ => {}
        }
    }
}

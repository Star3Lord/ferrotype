//! The purpose-built IR (migration step 2; see docs/MIGRATION.md).
//!
//! [`Ir`] is the owned middle of the new engine: named types with their
//! fields, shapes, and JSON-pointer provenance, plus operations summarized
//! as data. It is produced by [`lower`](crate::ir::lower_spec) from the
//! typed [`Spec`](crate::spec::Spec), decorated by the ordered
//! [`passes`](crate::ir::passes), and rendered by
//! [`emit`](crate::ir::emit_single_file).
//!
//! Design rule (R3 in ARCHITECTURE.md §8): the IR models what the
//! emitters need, not everything JSON Schema can express. Anything the
//! lowering can't represent is a loud, `Origin`-carrying error, and the
//! documented workaround is the spec patch mechanism.

mod emit;
mod lower;
pub mod passes;

pub use emit::{emit_single_file, emit_tokens};
pub use lower::lower_spec;

use crate::spec::Origin;

/// A reference to a Rust type from a field, variant, or alias position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRef {
    /// A named IR type (`TypeDef::name`).
    Named(String),
    /// `::std::string::String`.
    String,
    Bool,
    I32,
    I64,
    F64,
    /// `::serde_json::Value` (empty/`true` schemas).
    JsonValue,
    /// `()` — schemas that admit only `null`.
    Unit,
    /// A caller-supplied Rust path (format overrides, per-field type
    /// overrides), emitted verbatim.
    Custom(String),
    Option(Box<TypeRef>),
    Vec(Box<TypeRef>),
    /// `::std::collections::HashMap<K, V>` (from `additionalProperties`).
    Map(Box<TypeRef>, Box<TypeRef>),
    Boxed(Box<TypeRef>),
}

impl TypeRef {
    /// Wrap in `Option`, collapsing `Option<Option<T>>`.
    pub fn optional(self) -> TypeRef {
        match self {
            TypeRef::Option(_) => self,
            other => TypeRef::Option(Box::new(other)),
        }
    }

    /// The named type this reference points at, looking through
    /// `Option`/`Vec`/`Box` — `None` for primitives and maps.
    pub fn named_target(&self) -> Option<&str> {
        match self {
            TypeRef::Named(name) => Some(name),
            TypeRef::Option(inner) | TypeRef::Vec(inner) | TypeRef::Boxed(inner) => {
                inner.named_target()
            }
            _ => None,
        }
    }
}

/// One struct field.
#[derive(Debug, Clone)]
pub struct FieldDef {
    /// The Rust field identifier (snake_case, keyword-safe).
    pub rust_name: String,
    /// The authoritative wire name (the schema property key), or the
    /// base type's schema name for flatten fields.
    pub wire_name: String,
    pub ty: TypeRef,
    /// Whether the property was in the schema's `required` list.
    /// Flatten fields are always required.
    pub required: bool,
    /// `#[serde(flatten)]` composition base (allOf Compose).
    pub flatten: bool,
    pub description: Option<String>,
    /// The schema-level `default`, carried as information (the
    /// always-option style drops it from the wire surface).
    pub default: Option<serde_json::Value>,
    /// Rendered serde attribute options (e.g. `rename = "x"`,
    /// `flatten`), filled by `SerdeSurfacePass`.
    pub serde_options: Vec<String>,
    /// `Option<InnerPatch>` — the deep-patch annotation, filled by
    /// `DeepPatchPass`.
    pub patch_type: Option<String>,
    pub origin: Origin,
}

/// A struct shape.
#[derive(Debug, Clone, Default)]
pub struct StructShape {
    pub fields: Vec<FieldDef>,
    /// `additionalProperties: false` on a non-composed struct.
    pub deny_unknown_fields: bool,
}

/// One variant of a string enum.
#[derive(Debug, Clone)]
pub struct EnumVariant {
    /// The wire value.
    pub raw_name: String,
    /// The Rust variant identifier (Pascal, deduplicated).
    pub ident_name: String,
    pub description: Option<String>,
}

/// An enum of strings.
#[derive(Debug, Clone)]
pub struct StringEnumShape {
    pub variants: Vec<EnumVariant>,
    /// The schema-level `default` value (a raw wire string), when given.
    pub schema_default: Option<String>,
}

/// One variant of an untagged (oneOf/anyOf) enum.
#[derive(Debug, Clone)]
pub struct UntaggedVariant {
    pub ident_name: String,
    pub ty: TypeRef,
    pub description: Option<String>,
}

/// An untagged enum from `oneOf`/`anyOf`.
#[derive(Debug, Clone)]
pub struct UntaggedShape {
    pub variants: Vec<UntaggedVariant>,
}

/// The shape of a named IR type.
#[derive(Debug, Clone)]
pub enum Shape {
    Struct(StructShape),
    StringEnum(StringEnumShape),
    Untagged(UntaggedShape),
    /// A named schema that resolves to another type (primitives, arrays,
    /// plain strings, singleton `allOf`). Aliases emit no item —
    /// references inline the target — matching the typify engine, where
    /// only structs/enums/newtypes produce items.
    Alias(TypeRef),
}

/// A synthesized impl block, attached by `ImplSynthPass`, emitted right
/// after the type item.
#[derive(Debug, Clone)]
pub enum ImplSynth {
    /// `Display`/`FromStr`/`TryFrom<&str>`/`TryFrom<&String>`/
    /// `TryFrom<String>` for all-unit string enums.
    SimpleEnumConversions,
    /// `impl Default` returning `Self::<Variant>` (the
    /// first-unit-variant house rule).
    DefaultFirstVariant(String),
    /// `impl Default` returning `<TypeName>::<Variant>` (schema-level
    /// `default`; the different path form is a reproduced typify quirk —
    /// docs/MIGRATION.md D9).
    DefaultSchemaVariant(String),
    /// `impl Default` for untagged enums with no unit variant: first
    /// variant with `Default::default()` payloads (the old
    /// `postprocess.rs` behavior).
    DefaultUntaggedFirstVariant,
}

/// A named IR type.
#[derive(Debug, Clone)]
pub struct TypeDef {
    /// The Rust type name (sanitized Pascal, collision-checked).
    pub name: String,
    /// The originating `components.schemas` key, for named schemas.
    pub schema_key: Option<String>,
    pub origin: Origin,
    pub description: Option<String>,
    pub shape: Shape,
    /// Ordered derive list, filled by `DeriveAttrPass`.
    pub derives: Vec<String>,
    /// Unconditional attrs before the derive, filled by `DeriveAttrPass`.
    pub attrs_pre: Vec<String>,
    /// Unconditional attrs after the derive (and the type-level serde
    /// attr), filled by `DeriveAttrPass`.
    pub attrs_post: Vec<String>,
    /// `(feature, derive)` cfg-gated derives, before the main derive.
    pub cond_derives: Vec<(String, String)>,
    /// `(feature, attr)` cfg-gated attrs, before the derive.
    pub cond_attrs_pre: Vec<(String, String)>,
    /// `(feature, attr)` cfg-gated attrs, after the derive.
    pub cond_attrs_post: Vec<(String, String)>,
    /// Type-level serde options (`rename_all = "..."`,
    /// `deny_unknown_fields`), filled by `SerdeSurfacePass`.
    pub serde_options: Vec<String>,
    pub impls: Vec<ImplSynth>,
    /// Slash-separated module path, filled by `PartitionPass`; `None`
    /// means the flat (unpartitioned) top level.
    pub module: Option<String>,
}

impl TypeDef {
    pub(crate) fn new(
        name: String,
        schema_key: Option<String>,
        origin: Origin,
        description: Option<String>,
        shape: Shape,
    ) -> Self {
        TypeDef {
            name,
            schema_key,
            origin,
            description,
            shape,
            derives: Vec::new(),
            attrs_pre: Vec::new(),
            attrs_post: Vec::new(),
            cond_derives: Vec::new(),
            cond_attrs_pre: Vec::new(),
            cond_attrs_post: Vec::new(),
            serde_options: Vec::new(),
            impls: Vec::new(),
            module: None,
        }
    }

    /// Whether this type emits an item (aliases don't).
    pub fn emits_item(&self) -> bool {
        !matches!(self.shape, Shape::Alias(_))
    }

    /// Whether this is an all-unit-variant string enum (the
    /// `shared/enums` routing predicate).
    pub fn is_simple_enum(&self) -> bool {
        matches!(&self.shape, Shape::StringEnum(_))
    }
}

/// An operation summarized into the IR (data only; consumed by nothing
/// yet — steps 5/6 attach here).
#[derive(Debug, Clone)]
pub struct OperationIr {
    pub operation_id: Option<String>,
    pub method: String,
    pub path: String,
    /// Rust names of request-position types resolved from the spec.
    pub request_types: Vec<String>,
    /// Rust names of response-position types resolved from the spec.
    pub response_types: Vec<String>,
}

/// The IR: everything the passes and emitters operate on.
#[derive(Debug, Clone, Default)]
pub struct Ir {
    /// Named types, ordered by Rust name (emission order).
    pub types: Vec<TypeDef>,
    pub operations: Vec<OperationIr>,
    /// Per-module import preamble token text, filled by `ImportsPass`.
    /// Keyed by slash-separated module path; the empty string keys the
    /// flat top level.
    pub module_imports: std::collections::BTreeMap<String, String>,
    /// Modules that must exist even when empty (import-preamble
    /// referenced), filled by `PartitionPass`.
    pub materialized_modules: std::collections::BTreeSet<String>,
}

impl Ir {
    /// Look up a type by Rust name.
    pub fn get(&self, name: &str) -> Option<&TypeDef> {
        self.types.iter().find(|def| def.name == name)
    }

    /// Resolve a reference through alias chains to the effective
    /// use-site type: `Named(alias)` becomes the alias target,
    /// recursively; everything else is returned as-is.
    pub fn resolve(&self, reference: &TypeRef) -> TypeRef {
        match reference {
            TypeRef::Named(name) => match self.get(name).map(|def| &def.shape) {
                Some(Shape::Alias(target)) => self.resolve(target),
                _ => reference.clone(),
            },
            TypeRef::Option(inner) => self.resolve(inner).optional(),
            TypeRef::Vec(inner) => TypeRef::Vec(Box::new(self.resolve(inner))),
            TypeRef::Boxed(inner) => TypeRef::Boxed(Box::new(self.resolve(inner))),
            TypeRef::Map(key, value) => TypeRef::Map(
                Box::new(self.resolve(key)),
                Box::new(self.resolve(value)),
            ),
            other => other.clone(),
        }
    }
}

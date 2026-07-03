//! `Ir → syn::File`: the types emitter.
//!
//! In the default `expanded` emit style this reproduces the typify
//! engine's output shape exactly (the parity contract in
//! docs/MIGRATION.md): per module — import preamble, nested partition
//! modules (name order), an `error` submodule with `ConversionError` on
//! every leaf, then items sorted by type name with each type's impls
//! immediately following it. Attribute layout per item: doc,
//! unconditional before-derive attrs, cfg-gated before-derive attrs,
//! cfg-gated derives, `#[derive(...)]`, type-level `#[serde]`,
//! unconditional after-derive attrs, cfg-gated after-derive attrs.
//!
//! The `condensed` emit style (docs/MIGRATION.md D14) keeps the same
//! trait surface but hoists the boilerplate: one `support` module per
//! generation unit holds the single `error` module and an
//! `impl_string_enum!` macro; each string enum's five-impl conversion
//! ladder (plus its `Default`) becomes one macro invocation, and every
//! module that used to duplicate `pub mod error { ... }` re-exports the
//! shared one instead, so `<module>::error::ConversionError` paths keep
//! resolving.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::Result;
use crate::config::EmitStyle;

use super::{FieldDef, ImplSynth, Ir, Shape, TypeDef, TypeRef, UntaggedShape};

/// The name of the shared helper module emitted (once per generation
/// unit) by the condensed style.
const SUPPORT_MODULE: &str = "support";

/// Output sections within one module, in emission order (mirrors the
/// typify engine's `OutputSpaceMod`: Error < Crate). `Support` holds
/// the condensed style's bare `use`/re-export lines, which take the
/// error mod's position but are not wrapped in a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Support,
    Error,
    Crate,
}

/// One module's accumulated items.
#[derive(Default)]
struct ModuleSpace {
    items: BTreeMap<(Section, String), TokenStream>,
    has_types: bool,
    /// Whether any type here emits an `impl_string_enum!` invocation
    /// (condensed style) — decides the macro `use` line.
    uses_enum_macro: bool,
}

impl ModuleSpace {
    fn add(&mut self, section: Section, order_hint: &str, tokens: TokenStream) {
        self.items
            .entry((section, order_hint.to_string()))
            .or_default()
            .extend(tokens);
    }

    fn into_stream(self) -> TokenStream {
        let mut support_items = TokenStream::new();
        let mut error_items = TokenStream::new();
        let mut crate_items = TokenStream::new();
        for ((section, _), tokens) in self.items {
            match section {
                Section::Support => support_items.extend(tokens),
                Section::Error => error_items.extend(tokens),
                Section::Crate => crate_items.extend(tokens),
            }
        }
        let error_mod = (!error_items.is_empty()).then(|| {
            quote! {
                /// Error types.
                pub mod error {
                    #error_items
                }
            }
        });
        quote! {
            #support_items
            #error_mod
            #crate_items
        }
    }
}

/// The module tree (slash-separated paths), mirroring the typify
/// engine's `ModuleNode`.
#[derive(Default)]
struct ModuleNode {
    children: BTreeMap<String, ModuleNode>,
    space: Option<ModuleSpace>,
}

impl ModuleNode {
    fn at_path(&mut self, path: &str) -> &mut ModuleNode {
        if path.is_empty() {
            return self;
        }
        let mut node = self;
        for segment in path.split('/') {
            node = node.children.entry(segment.to_string()).or_default();
        }
        node
    }

    fn fill_error_mods(&mut self, error_items: &TokenStream) {
        let is_leaf = self.children.is_empty();
        if let Some(space) = &mut self.space
            && (is_leaf || space.has_types)
        {
            space.add(Section::Error, "", error_items.clone());
        }
        for child in self.children.values_mut() {
            child.fill_error_mods(error_items);
        }
    }

    /// Condensed-style counterpart of [`Self::fill_error_mods`]: the
    /// same modules that would have carried a duplicated `error` mod
    /// instead re-export the shared `support::error`, plus a `use` of
    /// the `impl_string_enum!` macro where an invocation follows.
    /// `depth` is the module's distance from the generation-unit root
    /// (where `support` lives).
    fn fill_support_uses(&mut self, depth: usize) {
        let is_leaf = self.children.is_empty();
        if let Some(space) = &mut self.space
            && (is_leaf || space.has_types)
        {
            // `use` paths are crate-rooted in edition 2018+; anchor the
            // in-unit path explicitly at every depth.
            let path: TokenStream = if depth == 0 {
                format!("self::{SUPPORT_MODULE}")
            } else {
                format!("{}{SUPPORT_MODULE}", "super::".repeat(depth))
            }
            .parse()
            .expect("support-module path always parses");
            let macro_use = space
                .uses_enum_macro
                .then(|| quote! { use #path::impl_string_enum; });
            space.add(
                Section::Support,
                "",
                quote! {
                    #macro_use
                    pub use #path::error;
                },
            );
        }
        for child in self.children.values_mut() {
            child.fill_support_uses(depth + 1);
        }
    }

    fn into_stream(
        self,
        path: String,
        imports: &BTreeMap<String, String>,
    ) -> Result<TokenStream> {
        let preamble: TokenStream = match imports.get(&path) {
            Some(text) => text
                .parse()
                .map_err(|error| anyhow::anyhow!("import preamble failed to parse: {error}"))?,
            None => TokenStream::new(),
        };
        let mut children = TokenStream::new();
        for (name, child) in self.children {
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let mod_ident = format_ident!("{}", name);
            let body = child.into_stream(child_path, imports)?;
            children.extend(quote! {
                pub mod #mod_ident {
                    #body
                }
            });
        }
        let body = self
            .space
            .map(ModuleSpace::into_stream)
            .unwrap_or_default();
        Ok(quote! {
            #preamble
            #children
            #body
        })
    }
}

/// Render the IR as the complete output token stream.
pub fn emit_tokens(ir: &Ir) -> Result<TokenStream> {
    let condensed = ir.emit_style == EmitStyle::Condensed;
    if condensed {
        ensure_no_support_collision(ir)?;
    }

    let mut root = ModuleNode::default();

    // Materialize referenced-but-possibly-empty modules first.
    for module in &ir.materialized_modules {
        let node = root.at_path(module);
        node.space.get_or_insert_with(ModuleSpace::default);
    }
    // Flat mode: the top level acts as one module.
    if ir.types.iter().all(|def| def.module.is_none()) {
        root.space.get_or_insert_with(ModuleSpace::default);
    }

    let mut any_enum_macro = false;
    for def in &ir.types {
        if !def.emits_item() {
            continue;
        }
        let module = def.module.clone().unwrap_or_default();
        let node = root.at_path(&module);
        let space = node.space.get_or_insert_with(ModuleSpace::default);
        space.has_types = true;
        if condensed && condenses_to_enum_macro(def) {
            space.uses_enum_macro = true;
            any_enum_macro = true;
        }
        let tokens = emit_type(ir, def).with_context(|| format!("emitting type {}", def.name))?;
        space.add(Section::Crate, &def.name, tokens);
    }

    if condensed {
        root.fill_support_uses(0);
        // The `pub mod support { ... }` item lands in the root space
        // (its empty order key sorts before every type name); the tree
        // writer splits it into `support.rs` like any partition module.
        let space = root.space.get_or_insert_with(ModuleSpace::default);
        space.add(Section::Crate, "", support_mod(any_enum_macro));
    } else {
        root.fill_error_mods(&error_mod_items());
    }
    root.into_stream(String::new(), &ir.module_imports)
}

/// Does this type's impl list render as one `impl_string_enum!`
/// invocation under the condensed style?
fn condenses_to_enum_macro(def: &TypeDef) -> bool {
    matches!(def.shape, Shape::StringEnum(_))
        && def
            .impls
            .iter()
            .any(|synth| matches!(synth, ImplSynth::SimpleEnumConversions))
}

/// The condensed style claims the `support` module name at the root of
/// the generation unit; a partition module (operation id) or module
/// override landing there would collide. Loud error over silent merge.
fn ensure_no_support_collision(ir: &Ir) -> Result<()> {
    let claims = |path: &str| {
        path.split('/').next() == Some(SUPPORT_MODULE)
    };
    for def in &ir.types {
        if def.module.as_deref().is_some_and(claims) {
            bail!(
                "type {} is assigned to module {:?}, but the condensed emit style \
                 reserves the top-level `support` module name; rename the operation \
                 or use a `[types] module` override",
                def.name,
                def.module.as_deref().unwrap_or_default(),
            );
        }
    }
    if ir.materialized_modules.iter().any(|path| claims(path)) {
        bail!(
            "a partition module is named `support`, which the condensed emit style \
             reserves for its shared helper module; rename the operation",
        );
    }
    Ok(())
}

/// Render the IR as a parsed [`syn::File`] (the artifact the writers
/// consume).
pub fn emit_single_file(ir: &Ir) -> Result<syn::File> {
    let tokens = emit_tokens(ir)?;
    syn::parse2(tokens).context("IR emitter produced tokens that do not parse as a Rust file")
}

/// The `ConversionError` items placed in every leaf `error` module —
/// token-for-token the typify engine's `fill_error_mod`.
fn error_mod_items() -> TokenStream {
    quote! {
        /// Error from a `TryFrom` or `FromStr` implementation.
        pub struct ConversionError(::std::borrow::Cow<'static, str>);

        impl ::std::error::Error for ConversionError {}
        impl ::std::fmt::Display for ConversionError {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>)
                -> Result<(), ::std::fmt::Error>
            {
                ::std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::std::fmt::Debug for ConversionError {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>)
                -> Result<(), ::std::fmt::Error>
            {
                ::std::fmt::Debug::fmt(&self.0, f)
            }
        }
        impl From<&'static str> for ConversionError {
            fn from(value: &'static str) -> Self {
                Self(value.into())
            }
        }
        impl From<String> for ConversionError {
            fn from(value: String) -> Self {
                Self(value.into())
            }
        }
    }
}

/// The condensed style's `support` module: the one shared `error`
/// module, plus — when any string enum exists — the
/// `impl_string_enum!` macro and its path-import re-export
/// (`pub(crate) use`, the standard `macro_rules!` cross-module pattern;
/// invocations reach it via `use <supers>::support::impl_string_enum;`
/// regardless of module depth or textual order).
fn support_mod(include_enum_macro: bool) -> TokenStream {
    let error_items = error_mod_items();
    let enum_macro = include_enum_macro.then(impl_string_enum_macro);
    quote! {
        /// Shared support items for the generated modules: the
        /// conversion error type and the impl-condensing macros, defined
        /// once instead of per module. Sibling modules re-export
        /// [`error`], so `<module>::error::ConversionError` paths keep
        /// resolving.
        pub mod support {
            /// Error types.
            pub mod error {
                #error_items
            }
            #enum_macro
        }
    }
}

/// The hand-formatted rendering of [`impl_string_enum_macro`]'s
/// `macro_rules!` item (at zero indent), substituted into rendered
/// output by [`polish_rendered`]. prettyplease cannot format
/// `macro_rules!` bodies (it flows them as raw tokens), and the macro
/// definition is precisely the place a reader goes to see what the
/// invocations implement — so it must read like the expanded impls it
/// replaces. `tests/emit_style.rs` pins this text token-identical to
/// the emitted macro, so the two cannot drift apart.
pub(crate) const IMPL_STRING_ENUM_MACRO_PRETTY: &str = r#"macro_rules! impl_string_enum {
    ($Type:ident { $($variant:ident => $raw:literal),* $(,)? } $(default = $default:ident)?) => {
        impl ::std::fmt::Display for $Type {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                match *self {
                    $(Self::$variant => f.write_str($raw),)*
                }
            }
        }
        impl ::std::str::FromStr for $Type {
            type Err = self::error::ConversionError;
            fn from_str(
                value: &str
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                match value {
                    $($raw => Ok(Self::$variant),)*
                    _ => Err("invalid value".into()),
                }
            }
        }
        impl ::std::convert::TryFrom<&str> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: &str
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        impl ::std::convert::TryFrom<&::std::string::String> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: &::std::string::String
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        impl ::std::convert::TryFrom<::std::string::String> for $Type {
            type Error = self::error::ConversionError;
            fn try_from(
                value: ::std::string::String
            ) -> ::std::result::Result<Self, self::error::ConversionError> {
                value.parse()
            }
        }
        $(
            impl ::std::default::Default for $Type {
                fn default() -> Self {
                    $Type::$default
                }
            }
        )?
    };
}"#;

/// Post-render fix-up applied to every formatted output document.
/// prettyplease flows macro tokens by line width, which turns both the
/// `macro_rules! impl_string_enum` definition and multi-variant
/// invocations into walls of wrapped tokens — the opposite of what the
/// condensed style is for. This pass re-renders both readably:
///
/// - the definition is replaced with the hand-formatted
///   [`IMPL_STRING_ENUM_MACRO_PRETTY`] (token-identical; pinned by
///   `tests/emit_style.rs`);
/// - every `impl_string_enum!(...)` invocation is reflowed to one
///   `Variant => "wire"` pair per line, and the result is re-parsed and
///   token-compared against the original before being accepted.
///
/// No-op for output without the macro (the expanded style and the
/// whole typify engine).
pub(crate) fn polish_rendered(source: String) -> String {
    if !source.contains("impl_string_enum") {
        return source;
    }
    polish_invocations(polish_macro_def(source))
}

/// Replace the flowed `macro_rules!` item with the pretty rendering,
/// re-indented to wherever the item sits (depth 1 in single-file
/// output, depth 0 in the tree writer's `support.rs`).
fn polish_macro_def(source: String) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let Some((start, indent)) = lines.iter().enumerate().find_map(|(index, line)| {
        line.trim_start()
            .eq("macro_rules! impl_string_enum {")
            .then(|| (index, &line[..line.len() - line.trim_start().len()]))
    }) else {
        return source;
    };
    let closer = format!("{indent}}}");
    let Some(end) = lines[start + 1..]
        .iter()
        .position(|line| *line == closer)
        .map(|offset| start + 1 + offset)
    else {
        return source;
    };

    let mut polished: Vec<String> = lines[..start].iter().map(|s| s.to_string()).collect();
    polished.extend(
        IMPL_STRING_ENUM_MACRO_PRETTY
            .lines()
            .map(|line| format!("{indent}{line}")),
    );
    polished.extend(lines[end + 1..].iter().map(|s| s.to_string()));
    finish_lines(polished, &source)
}

/// Reflow every `impl_string_enum!(...)` invocation to one variant pair
/// per line. Token-verified: the reflowed text must re-parse to the
/// exact same token stream, otherwise the original rendering is kept.
fn polish_invocations(source: String) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut polished: Vec<String> = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if !line.trim_start().starts_with("impl_string_enum!(") {
            polished.push(line.to_string());
            index += 1;
            continue;
        }
        let indent = &line[..line.len() - line.trim_start().len()];
        // Find the invocation's end: the first line completing a
        // parsable macro item (invocations are statement-terminated
        // with `);`).
        let mut end = index;
        let item = loop {
            if end >= lines.len() {
                break None;
            }
            if lines[end].trim_end().ends_with(");") {
                let candidate = lines[index..=end].join("\n");
                if let Ok(item) = syn::parse_str::<syn::ItemMacro>(&candidate) {
                    break Some(item);
                }
            }
            end += 1;
        };
        let Some(item) = item else {
            polished.push(line.to_string());
            index += 1;
            continue;
        };
        match reflow_invocation(&item, indent) {
            Some(reflowed) => polished.extend(reflowed),
            None => polished.extend(lines[index..=end].iter().map(|s| s.to_string())),
        }
        index = end + 1;
    }
    finish_lines(polished, &source)
}

/// Render one parsed invocation as `Type {` / one pair per line /
/// `} default = Variant);`, verifying token fidelity.
fn reflow_invocation(item: &syn::ItemMacro, indent: &str) -> Option<Vec<String>> {
    use proc_macro2::TokenTree;

    let tokens: Vec<TokenTree> = item.mac.tokens.clone().into_iter().collect();
    // Expected shape: Ident, Brace group, then optionally
    // `default` `=` Ident.
    let (type_ident, rest) = match tokens.split_first()? {
        (TokenTree::Ident(ident), rest) => (ident.clone(), rest),
        _ => return None,
    };
    let (group, rest) = match rest.split_first()? {
        (TokenTree::Group(group), rest)
            if group.delimiter() == proc_macro2::Delimiter::Brace =>
        {
            (group.clone(), rest)
        }
        _ => return None,
    };
    let default_variant = match rest {
        [] => None,
        [
            TokenTree::Ident(kw),
            TokenTree::Punct(eq),
            TokenTree::Ident(variant),
        ] if kw == "default" && eq.as_char() == '=' => Some(variant.clone()),
        _ => return None,
    };

    // Pairs: Ident, '=>' (joint '=' + '>'), Literal, [','].
    let mut pairs: Vec<(proc_macro2::Ident, proc_macro2::Literal)> = Vec::new();
    let mut stream = group.stream().into_iter().peekable();
    while let Some(tree) = stream.next() {
        let TokenTree::Ident(variant) = tree else {
            return None;
        };
        match (stream.next()?, stream.next()?) {
            (TokenTree::Punct(eq), TokenTree::Punct(gt))
                if eq.as_char() == '=' && gt.as_char() == '>' => {}
            _ => return None,
        }
        let TokenTree::Literal(raw) = stream.next()? else {
            return None;
        };
        pairs.push((variant, raw));
        if let Some(TokenTree::Punct(comma)) = stream.peek()
            && comma.as_char() == ','
        {
            stream.next();
        }
    }

    let mut reflowed = Vec::with_capacity(pairs.len() + 2);
    reflowed.push(format!("{indent}impl_string_enum!({type_ident} {{"));
    for (variant, raw) in &pairs {
        reflowed.push(format!("{indent}    {variant} => {raw},"));
    }
    reflowed.push(match &default_variant {
        Some(variant) => format!("{indent}}} default = {variant});"),
        None => format!("{indent}}});"),
    });

    // Fidelity gate: the reflowed text must hold the same tokens.
    let reparsed: syn::ItemMacro = syn::parse_str(&reflowed.join("\n")).ok()?;
    tokens_equal(reparsed.mac.tokens.clone(), item.mac.tokens.clone()).then_some(reflowed)
}

/// Structural token equality, ignoring `proc_macro2::Spacing` (which
/// differs between quote-built and text-lexed streams around tokens
/// like `$`) — the correct notion of "same tokens" for macro fidelity.
fn tokens_equal(a: TokenStream, b: TokenStream) -> bool {
    use proc_macro2::TokenTree;
    let (a, b): (Vec<TokenTree>, Vec<TokenTree>) =
        (a.into_iter().collect(), b.into_iter().collect());
    a.len() == b.len()
        && a.into_iter().zip(b).all(|(left, right)| match (left, right) {
            (TokenTree::Group(l), TokenTree::Group(r)) => {
                l.delimiter() == r.delimiter() && tokens_equal(l.stream(), r.stream())
            }
            (TokenTree::Ident(l), TokenTree::Ident(r)) => l == r,
            (TokenTree::Punct(l), TokenTree::Punct(r)) => l.as_char() == r.as_char(),
            (TokenTree::Literal(l), TokenTree::Literal(r)) => l.to_string() == r.to_string(),
            _ => false,
        })
}

/// Join polished lines, preserving the source's trailing newline.
fn finish_lines(lines: Vec<String>, source: &str) -> String {
    let mut result = lines.join("\n");
    if source.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// The `impl_string_enum!` definition. Each expansion is exactly the
/// impl set the expanded style writes out per string enum — same trait
/// surface, same `self::error::ConversionError` paths (resolving
/// through the invoking module's `error` re-export), same bodies — so
/// the two styles differ only in layout.
pub(crate) fn impl_string_enum_macro() -> TokenStream {
    quote! {
        /// Implements the wire-format conversions for a string enum:
        /// `Display` and `FromStr` over the `Variant => "wire value"`
        /// pairs (erring with `self::error::ConversionError`), the
        /// `TryFrom<&str>` / `TryFrom<&String>` / `TryFrom<String>`
        /// ladder via `FromStr`, and — when a `default = Variant`
        /// clause is present — `Default`.
        macro_rules! impl_string_enum {
            ($Type:ident { $($variant:ident => $raw:literal),* $(,)? } $(default = $default:ident)?) => {
                impl ::std::fmt::Display for $Type {
                    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        match *self {
                            $(Self::$variant => f.write_str($raw),)*
                        }
                    }
                }
                impl ::std::str::FromStr for $Type {
                    type Err = self::error::ConversionError;

                    fn from_str(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        match value {
                            $($raw => Ok(Self::$variant),)*
                            _ => Err("invalid value".into()),
                        }
                    }
                }
                impl ::std::convert::TryFrom<&str> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<&::std::string::String> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<::std::string::String> for $Type {
                    type Error = self::error::ConversionError;

                    fn try_from(value: ::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                $(
                    impl ::std::default::Default for $Type {
                        fn default() -> Self {
                            $Type::$default
                        }
                    }
                )?
            };
        }
        pub(crate) use impl_string_enum;
    }
}

/// Render one named type: the item plus its synthesized impls.
fn emit_type(ir: &Ir, def: &TypeDef) -> Result<TokenStream> {
    let doc_text = def
        .description
        .clone()
        .unwrap_or_else(|| format!("`{}`", def.name));
    let doc = quote! { #[doc = #doc_text] };

    let attrs_pre = parse_attr_lines(&def.attrs_pre)?;
    let attrs_post = parse_attr_lines(&def.attrs_post)?;
    let cond_attrs_pre = parse_cond_attr_lines(&def.cond_attrs_pre)?;
    let cond_attrs_post = parse_cond_attr_lines(&def.cond_attrs_post)?;
    let cond_derives = def
        .cond_derives
        .iter()
        .map(|(feature, derive)| {
            let path: syn::Path = syn::parse_str(derive)
                .with_context(|| format!("conditional derive {derive:?} is not a valid path"))?;
            Ok(quote! { #[cfg_attr(feature = #feature, derive(#path))] })
        })
        .collect::<Result<Vec<_>>>()?;
    let derives = def
        .derives
        .iter()
        .map(|derive| {
            let path: syn::Path = syn::parse_str(derive)
                .with_context(|| format!("derive {derive:?} is not a valid path"))?;
            Ok(quote! { #path })
        })
        .collect::<Result<Vec<_>>>()?;
    let serde = serde_attr(&def.serde_options)?;

    let type_ident = format_ident!("{}", def.name);
    let body = match &def.shape {
        Shape::Struct(shape) => {
            let fields = shape
                .fields
                .iter()
                .map(emit_field)
                .collect::<Result<Vec<_>>>()?;
            quote! {
                pub struct #type_ident {
                    #(#fields)*
                }
            }
        }
        Shape::StringEnum(shape) => {
            let variants = shape.variants.iter().map(|variant| {
                let ident = format_ident!("{}", variant.ident_name);
                let doc = variant
                    .description
                    .as_ref()
                    .map(|text| quote! { #[doc = #text] });
                let rename = (variant.raw_name != variant.ident_name).then(|| {
                    let raw = &variant.raw_name;
                    quote! { #[serde(rename = #raw)] }
                });
                quote! {
                    #doc
                    #rename
                    #ident,
                }
            });
            quote! {
                pub enum #type_ident {
                    #(#variants)*
                }
            }
        }
        Shape::Untagged(shape) => {
            let variants = shape
                .variants
                .iter()
                .map(|variant| {
                    let ident = format_ident!("{}", variant.ident_name);
                    let doc = variant
                        .description
                        .as_ref()
                        .map(|text| quote! { #[doc = #text] });
                    let ty = type_tokens(&variant.ty)?;
                    Ok(quote! {
                        #doc
                        #ident(#ty),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            quote! {
                pub enum #type_ident {
                    #(#variants)*
                }
            }
        }
        Shape::Alias(_) => bail!("aliases do not emit items"),
    };

    let impls = if ir.emit_style == EmitStyle::Condensed && condenses_to_enum_macro(def) {
        emit_string_enum_invocation(def)?
    } else {
        def.impls
            .iter()
            .map(|synth| emit_impl_expanded(def, synth))
            .collect::<Result<Vec<_>>>()?
    };

    Ok(quote! {
        #doc
        #(#attrs_pre)*
        #(#cond_attrs_pre)*
        #(#cond_derives)*
        #[derive(#(#derives),*)]
        #serde
        #(#attrs_post)*
        #(#cond_attrs_post)*
        #body
        #(#impls)*
    })
}

/// The condensed rendering of a string enum's synthesized impls: one
/// `impl_string_enum!` invocation carrying the `Variant => "wire"`
/// pairs, with the `Default` selection folded into a `default =`
/// clause. (`Self::Variant` vs `TypeName::Variant` — the D9 byte quirk
/// separating first-variant from schema-default `Default` impls — has
/// no distinct form here; the macro expands to `$Type::$default`, which
/// is what both spellings resolve to.) Impl kinds outside the ladder
/// fall back to their expanded form, preserving robustness if a custom
/// pass attaches one.
fn emit_string_enum_invocation(def: &TypeDef) -> Result<Vec<TokenStream>> {
    let Shape::StringEnum(shape) = &def.shape else {
        bail!("string-enum invocation on non-enum type {}", def.name);
    };
    let type_ident = format_ident!("{}", def.name);
    let pairs = shape.variants.iter().map(|variant| {
        let ident = format_ident!("{}", variant.ident_name);
        let raw = &variant.raw_name;
        quote! { #ident => #raw }
    });

    let mut default_variant = None;
    let mut extra = Vec::new();
    for synth in &def.impls {
        match synth {
            ImplSynth::SimpleEnumConversions => {}
            ImplSynth::DefaultFirstVariant(variant)
            | ImplSynth::DefaultSchemaVariant(variant) => default_variant = Some(variant),
            other => extra.push(other),
        }
    }
    let default_clause = default_variant.map(|variant| {
        let ident = format_ident!("{}", variant);
        quote! { default = #ident }
    });

    let mut impls = vec![quote! {
        impl_string_enum!(#type_ident { #(#pairs,)* } #default_clause);
    }];
    for synth in extra {
        impls.push(emit_impl_expanded(def, synth)?);
    }
    Ok(impls)
}

fn emit_field(field: &FieldDef) -> Result<TokenStream> {
    let doc = field
        .description
        .as_ref()
        .map(|text| quote! { #[doc = #text] });
    let serde = serde_attr(&field.serde_options)?;
    let patch = field
        .patch_type
        .as_ref()
        .map(|patch_type| quote! { #[patch(name = #patch_type)] });
    let name = format_ident!("{}", field.rust_name);
    let ty = type_tokens(&field.ty)?;
    Ok(quote! {
        #doc
        #serde
        #patch
        pub #name: #ty,
    })
}

/// `#[serde(a, b, c)]`, or nothing when there are no options.
fn serde_attr(options: &[String]) -> Result<Option<TokenStream>> {
    if options.is_empty() {
        return Ok(None);
    }
    let parsed = options
        .iter()
        .map(|option| {
            option
                .parse::<TokenStream>()
                .map_err(|error| anyhow::anyhow!("serde option {option:?}: {error}"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(quote! { #[serde( #(#parsed),* )] }))
}

fn parse_attr_lines(attrs: &[String]) -> Result<Vec<TokenStream>> {
    attrs
        .iter()
        .map(|attr| {
            let tokens: TokenStream = attr
                .parse()
                .map_err(|error| anyhow::anyhow!("attribute {attr:?}: {error}"))?;
            Ok(quote! { #[ #tokens ] })
        })
        .collect()
}

fn parse_cond_attr_lines(attrs: &[(String, String)]) -> Result<Vec<TokenStream>> {
    attrs
        .iter()
        .map(|(feature, attr)| {
            let tokens: TokenStream = attr
                .parse()
                .map_err(|error| anyhow::anyhow!("attribute {attr:?}: {error}"))?;
            Ok(quote! { #[cfg_attr(feature = #feature, #tokens)] })
        })
        .collect()
}

/// Render a [`TypeRef`] as a Rust type, with typify's exact path forms.
fn type_tokens(reference: &TypeRef) -> Result<TokenStream> {
    Ok(match reference {
        TypeRef::Named(name) => {
            let ident = format_ident!("{}", name);
            quote! { #ident }
        }
        TypeRef::String => quote! { ::std::string::String },
        TypeRef::Bool => quote! { bool },
        TypeRef::I32 => quote! { i32 },
        TypeRef::I64 => quote! { i64 },
        TypeRef::F64 => quote! { f64 },
        TypeRef::JsonValue => quote! { ::serde_json::Value },
        TypeRef::Unit => quote! { () },
        TypeRef::Custom(path) => {
            let ty: syn::Type = syn::parse_str(path)
                .with_context(|| format!("custom type path {path:?} does not parse"))?;
            quote! { #ty }
        }
        TypeRef::Option(inner) => {
            let inner = type_tokens(inner)?;
            quote! { ::std::option::Option<#inner> }
        }
        TypeRef::Vec(inner) => {
            let inner = type_tokens(inner)?;
            quote! { ::std::vec::Vec<#inner> }
        }
        TypeRef::Map(key, value) => {
            let key = type_tokens(key)?;
            let value = type_tokens(value)?;
            quote! { ::std::collections::HashMap<#key, #value> }
        }
        TypeRef::Boxed(inner) => {
            let inner = type_tokens(inner)?;
            quote! { ::std::boxed::Box<#inner> }
        }
    })
}

/// Render one synthesized impl block in its written-out (expanded) form.
fn emit_impl_expanded(def: &TypeDef, synth: &ImplSynth) -> Result<TokenStream> {
    let type_ident = format_ident!("{}", def.name);
    Ok(match synth {
        ImplSynth::SimpleEnumConversions => {
            let Shape::StringEnum(shape) = &def.shape else {
                bail!("SimpleEnumConversions on a non-enum type {}", def.name);
            };
            let (variant_idents, raw_names): (Vec<_>, Vec<_>) = shape
                .variants
                .iter()
                .map(|variant| {
                    (
                        format_ident!("{}", variant.ident_name),
                        variant.raw_name.clone(),
                    )
                })
                .unzip();
            quote! {
                impl ::std::fmt::Display for #type_ident {
                    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        match *self {
                            #(Self::#variant_idents => f.write_str(#raw_names),)*
                        }
                    }
                }
                impl ::std::str::FromStr for #type_ident {
                    type Err = self::error::ConversionError;

                    fn from_str(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        match value {
                            #(#raw_names => Ok(Self::#variant_idents),)*
                            _ => Err("invalid value".into()),
                        }
                    }
                }
                impl ::std::convert::TryFrom<&str> for #type_ident {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &str) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<&::std::string::String> for #type_ident {
                    type Error = self::error::ConversionError;

                    fn try_from(value: &::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
                impl ::std::convert::TryFrom<::std::string::String> for #type_ident {
                    type Error = self::error::ConversionError;

                    fn try_from(value: ::std::string::String) ->
                        ::std::result::Result<Self, self::error::ConversionError>
                    {
                        value.parse()
                    }
                }
            }
        }
        ImplSynth::DefaultFirstVariant(variant) => {
            let variant_ident = format_ident!("{}", variant);
            quote! {
                impl ::std::default::Default for #type_ident {
                    fn default() -> Self {
                        Self::#variant_ident
                    }
                }
            }
        }
        ImplSynth::DefaultSchemaVariant(variant) => {
            let variant_ident = format_ident!("{}", variant);
            quote! {
                impl ::std::default::Default for #type_ident {
                    fn default() -> Self {
                        #type_ident::#variant_ident
                    }
                }
            }
        }
        ImplSynth::DefaultUntaggedFirstVariant => {
            let Shape::Untagged(UntaggedShape { variants }) = &def.shape else {
                bail!("DefaultUntaggedFirstVariant on non-untagged type {}", def.name);
            };
            let first = variants
                .first()
                .with_context(|| format!("untagged enum {} has no variants", def.name))?;
            let variant_ident = format_ident!("{}", first.ident_name);
            quote! {
                impl ::std::default::Default for #type_ident {
                    fn default() -> Self {
                        Self::#variant_ident(::std::default::Default::default())
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-formatted macro text substituted by [`polish_rendered`]
    /// must stay token-identical to the macro the emitter actually
    /// emits — otherwise the "pretty" definition would lie about the
    /// impls the invocations expand to.
    #[test]
    fn pretty_macro_text_matches_emitted_macro_tokens() {
        let emitted: syn::File = syn::parse2(impl_string_enum_macro())
            .expect("emitted support items parse as items");
        let emitted_macro = emitted
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Macro(item) => Some(item.mac.tokens.clone()),
                _ => None,
            })
            .expect("emitted items contain the macro_rules item");

        let pretty: syn::ItemMacro = syn::parse_str(IMPL_STRING_ENUM_MACRO_PRETTY)
            .expect("pretty macro text parses as a macro item");
        assert!(
            tokens_equal(pretty.mac.tokens.clone(), emitted_macro.clone()),
            "IMPL_STRING_ENUM_MACRO_PRETTY drifted from impl_string_enum_macro():\
             \npretty:  {}\nemitted: {}",
            pretty.mac.tokens,
            emitted_macro,
        );
    }

    /// The invocation reflow keeps token fidelity on tricky wire values
    /// (quotes, braces, parens inside the strings).
    #[test]
    fn invocation_reflow_preserves_tokens() {
        let source = concat!(
            "        impl_string_enum!(\n",
            "            Weird { A => \"has \\\" quote\", B => \"has ) } ( {\", C =>\n",
            "            \"plain\", } default = A\n",
            "        );\n",
        );
        let polished = polish_invocations(source.to_string());
        assert!(
            polished.contains("        impl_string_enum!(Weird {\n"),
            "reflow applied:\n{polished}",
        );
        assert!(polished.contains("            A => \"has \\\" quote\",\n"));
        assert!(polished.contains("        } default = A);"));
        let original: syn::ItemMacro = syn::parse_str(source.trim()).unwrap();
        let reflowed: syn::ItemMacro = syn::parse_str(polished.trim()).unwrap();
        assert_eq!(
            original.mac.tokens.to_string(),
            reflowed.mac.tokens.to_string(),
        );
    }
}

//! The field-scoped override tiers (docs/MIGRATION.md D20): `[fields]`
//! `field-attrs` / table-form `type`, and the ordered `[[rules]]` tier
//! between the style-level mappings and `[fields]`.

use openapi_codegen::config::{
    Capability, FormatMapping, FormatMappingTable, Rule, RuleApply, RuleMatch, TypeReplacement,
    TypeReplacementTable,
};
use openapi_codegen::{Generator, StyleConfig, StyleProfile};

fn generator_for(name: &str, document: serde_json::Value) -> Generator {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("rules");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    Generator::new(path).profile(StyleProfile::ApiClient)
}

/// One POST operation (split mode puts its request type in
/// `create_thing/request`), plus shared schemas exercising every
/// predicate domain: kebab-case schema keys vs Rust names, wire vs
/// Rust field names, `$ref`-hop format provenance, and a struct chain
/// for transitive pruning.
fn op_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": { "/things": { "post": {
            "operationId": "createThing",
            "requestBody": { "content": { "application/json": {
                "schema": { "$ref": "#/components/schemas/CreateThingRequest" }
            } } },
            "responses": { "200": { "content": { "application/json": {
                "schema": { "$ref": "#/components/schemas/thing-result" }
            } } } }
        } } },
        "components": { "schemas": {
            "CreateThingRequest": {
                "type": "object",
                "required": ["createdAt"],
                "properties": {
                    "createdAt": { "type": "string", "format": "date-time" },
                    "updatedAt": { "type": "string", "format": "date-time" },
                    "stampedAt": { "$ref": "#/components/schemas/stamp" },
                    "note": { "$ref": "#/components/schemas/Note" }
                }
            },
            "thing-result": {
                "type": "object",
                "properties": {
                    "finishedAt": { "type": "string", "format": "date-time" }
                }
            },
            "stamp": { "type": "string", "format": "date-time" },
            "Note": {
                "type": "object",
                "properties": { "text": { "type": "string" } }
            }
        } }
    })
}

fn rule(match_: RuleMatch, apply: RuleApply) -> Rule {
    Rule { match_, apply }
}

fn attrs_apply(bodies: &[&str]) -> RuleApply {
    RuleApply {
        field_attrs: Some(bodies.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

/// The attribute line(s) directly above `pub {field}` in `out`.
fn attrs_above<'a>(out: &'a str, field: &str) -> Vec<&'a str> {
    let needle = format!("pub {field}:");
    let position = out
        .find(&needle)
        .unwrap_or_else(|| panic!("no field `{field}` in output"));
    out[..position]
        .lines()
        .rev()
        // The slice cuts through the field's own indentation; drop
        // that partial line before scanning the attr/doc stack.
        .skip(1)
        .take_while(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("#[") || trimmed.starts_with("///")
        })
        .filter(|line| line.trim_start().starts_with("#["))
        .collect()
}

// ─── Tier 1: [fields] field-attrs ────────────────────────────────────────────

fn time_mapping() -> FormatMapping {
    FormatMapping::Table(FormatMappingTable {
        type_path: "::time::OffsetDateTime".to_string(),
        field_attrs: vec!["serde(with = \"time::serde::iso8601\")".to_string()],
        optional_field_attrs: vec![
            "serde(default, with = \"time::serde::iso8601::option\")".to_string(),
        ],
        impls: vec![Capability::Serialize, Capability::Deserialize],
    })
}

#[test]
fn field_attrs_replace_clear_and_inherit() {
    let out = generator_for("fields_tier_attrs", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.formats.insert("string/date-time".into(), time_mapping());
            // Replace: createdAt gets the rfc3339 module instead of
            // the mapping's iso8601 one. Clear: updatedAt gets nothing.
            // Inherit: finishedAt keeps the mapping attr (absent key).
            style
                .fields
                .entry("CreateThingRequest.createdAt".to_string())
                .or_default()
                .field_attrs = Some(vec!["serde(with = \"time::serde::rfc3339\")".to_string()]);
            style
                .fields
                .entry("CreateThingRequest.updatedAt".to_string())
                .or_default()
                .field_attrs = Some(vec![]);
        })
        .generate_to_string()
        .unwrap();

    let created = attrs_above(&out, "created_at").join("\n");
    assert!(created.contains("rfc3339"), "replaced, not merged: {created}");
    assert!(!created.contains("iso8601"), "mapping attr replaced: {created}");

    let updated = attrs_above(&out, "updated_at").join("\n");
    assert!(!updated.contains("serde(with"), "empty list clears: {updated}");
    assert!(!updated.contains("serde(default, with"), "empty list clears: {updated}");

    let finished = attrs_above(&out, "finished_at").join("\n");
    assert!(
        finished.contains("iso8601::option"),
        "absent key inherits the mapping attr: {finished}",
    );
}

#[test]
fn fields_tier_table_type_participates_in_default_pruning() {
    // `Order.stamp` is overridden to an external type declaring no
    // capabilities: Order loses Default, and Wrapper — requiring
    // Order — loses it transitively.
    let spec = serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": {},
        "components": { "schemas": {
            "Order": {
                "type": "object",
                "required": ["stamp"],
                "properties": { "stamp": { "type": "string" } }
            },
            "Wrapper": {
                "type": "object",
                "required": ["order"],
                "properties": { "order": { "$ref": "#/components/schemas/Order" } }
            }
        } }
    });
    let out = generator_for("fields_tier_type_pruning", spec)
        .style(|style| {
            style
                .fields
                .entry("Order.stamp".to_string())
                .or_default()
                .type_path = Some(TypeReplacement::Table(TypeReplacementTable {
                type_path: "::my_crate::Stamp".to_string(),
                field_attrs: vec!["serde(with = \"my_crate::stamp_serde\")".to_string()],
                impls: vec![Capability::Serialize, Capability::Deserialize],
            }));
        })
        .generate_to_string()
        .unwrap();

    assert!(out.contains("pub stamp: ::my_crate::Stamp"), "{out}");
    assert!(
        out.contains("#[serde(with = \"my_crate::stamp_serde\")]"),
        "table attrs attach: {out}",
    );
    let order_derives = derive_line_of(&out, "Order");
    let wrapper_derives = derive_line_of(&out, "Wrapper");
    assert!(!order_derives.contains("Default"), "{order_derives}");
    assert!(!wrapper_derives.contains("Default"), "transitive: {wrapper_derives}");
}

fn derive_line_of<'a>(out: &'a str, name: &str) -> &'a str {
    let decl = format!("pub struct {name}");
    let position = out
        .find(&decl)
        .unwrap_or_else(|| panic!("no `{decl}` in output"));
    out[..position]
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("#[derive("))
        .unwrap_or_else(|| panic!("no derive line above {name}"))
}

// ─── Tier 2: [[rules]] predicates ────────────────────────────────────────────

#[test]
fn rule_predicate_matrix() {
    // module glob in split mode + format provenance (direct and
    // through the $ref hop to the named scalar `stamp`).
    let out = generator_for("rule_module_format", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.rules.push(rule(
                RuleMatch {
                    module: Some("*/request".to_string()),
                    format: Some("string/date-time".to_string()),
                    ..Default::default()
                },
                attrs_apply(&["serde(with = \"marker::request_dt\")"]),
            ));
        })
        .generate_to_string()
        .unwrap();
    for field in ["created_at", "updated_at", "stamped_at"] {
        assert!(
            attrs_above(&out, field).join("\n").contains("marker::request_dt"),
            "{field} should match module+format rule",
        );
    }
    // thing-result sits in */response: no match.
    assert!(
        !attrs_above(&out, "finished_at").join("\n").contains("marker::request_dt"),
        "response module must not match",
    );

    // struct glob on both name domains: kebab schema key and Rust name.
    for (label, pattern) in [("schema-key", "thing-*"), ("rust-name", "ThingR*")] {
        let out = generator_for(&format!("rule_struct_{label}"), op_spec())
            .split_request_response(true)
            .style(move |style| {
                style.rules.push(rule(
                    RuleMatch {
                        struct_: Some(pattern.to_string()),
                        field: Some("*".to_string()),
                        ..Default::default()
                    },
                    attrs_apply(&["serde(with = \"marker::by_struct\")"]),
                ));
            })
            .generate_to_string()
            .unwrap();
        assert!(
            attrs_above(&out, "finished_at").join("\n").contains("marker::by_struct"),
            "struct pattern {pattern:?} ({label}) should match",
        );
    }

    // field glob on both name domains: wire name and Rust name.
    for (label, pattern) in [("wire", "createdAt"), ("rust", "created_at")] {
        let out = generator_for(&format!("rule_field_{label}"), op_spec())
            .split_request_response(true)
            .style(move |style| {
                style.rules.push(rule(
                    RuleMatch {
                        field: Some(pattern.to_string()),
                        ..Default::default()
                    },
                    attrs_apply(&["serde(with = \"marker::by_field\")"]),
                ));
            })
            .generate_to_string()
            .unwrap();
        assert!(
            attrs_above(&out, "created_at").join("\n").contains("marker::by_field"),
            "field pattern {pattern:?} ({label}) should match",
        );
        assert!(
            !attrs_above(&out, "updated_at").join("\n").contains("marker::by_field"),
            "field pattern {pattern:?} must not over-match",
        );
    }

    // resolved-type glob against the mapped Rust type.
    let out = generator_for("rule_type_glob", op_spec())
        .split_request_response(true)
        .style(|style| {
            style
                .formats
                .insert("string/date-time".into(), "::time::OffsetDateTime".into());
            style.rules.push(rule(
                RuleMatch {
                    type_: Some("*OffsetDateTime".to_string()),
                    ..Default::default()
                },
                attrs_apply(&["serde(with = \"marker::by_type\")"]),
            ));
        })
        .generate_to_string()
        .unwrap();
    // Required and Option-wrapped fields both match (the predicate
    // unwraps Option); the plain-string field does not.
    for field in ["created_at", "updated_at", "finished_at"] {
        assert!(
            attrs_above(&out, field).join("\n").contains("marker::by_type"),
            "{field} should match the type glob",
        );
    }
    assert!(
        !attrs_above(&out, "text").join("\n").contains("marker::by_type"),
        "non-date-time fields must not match",
    );
}

#[test]
fn rule_ordering_later_wins_and_fields_tier_beats_rules() {
    let out = generator_for("rule_ordering", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.rules.push(rule(
                RuleMatch {
                    format: Some("string/date-time".to_string()),
                    ..Default::default()
                },
                attrs_apply(&["serde(with = \"marker::first\")"]),
            ));
            style.rules.push(rule(
                RuleMatch {
                    format: Some("string/date-time".to_string()),
                    ..Default::default()
                },
                attrs_apply(&["serde(with = \"marker::second\")"]),
            ));
            style
                .fields
                .entry("CreateThingRequest.createdAt".to_string())
                .or_default()
                .field_attrs = Some(vec!["serde(with = \"marker::fields_tier\")".to_string()]);
        })
        .generate_to_string()
        .unwrap();

    // Later rule wins key-by-key…
    let updated = attrs_above(&out, "updated_at").join("\n");
    assert!(updated.contains("marker::second"), "{updated}");
    assert!(!updated.contains("marker::first"), "later rule replaces: {updated}");
    // …and the [fields] tier beats every rule.
    let created = attrs_above(&out, "created_at").join("\n");
    assert!(created.contains("marker::fields_tier"), "{created}");
    assert!(!created.contains("marker::second"), "{created}");
}

#[test]
fn rule_type_override_prunes_and_strips_deep_patch() {
    // A rule replacing note's type: the field type changes, Note's
    // deep-patch annotation goes (no companion for the external type),
    // and the empty capability set prunes PartialEq from the owner.
    let out = generator_for("rule_type_override", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.rules.push(rule(
                RuleMatch {
                    field: Some("note".to_string()),
                    ..Default::default()
                },
                RuleApply {
                    type_: Some(TypeReplacement::Table(TypeReplacementTable {
                        type_path: "::my_crate::Note".to_string(),
                        field_attrs: vec![],
                        impls: vec![Capability::Serialize, Capability::Deserialize,
                                    Capability::Default],
                    })),
                    ..Default::default()
                },
            ));
        })
        .generate_to_string()
        .unwrap();

    assert!(
        out.contains("pub note: ::std::option::Option<::my_crate::Note>"),
        "{out}",
    );
    assert!(!out.contains("#[patch(name = \"Option<NotePatch>\")]"), "{out}");
    // Note itself is still generated (other uses may exist), but the
    // owner loses PartialEq: Option<my_crate::Note> without partial-eq.
    let derives = derive_line_of(&out, "CreateThingRequest");
    assert!(!derives.contains("PartialEq"), "{derives}");
    assert!(derives.contains("Default"), "declared default keeps Default: {derives}");
}

#[test]
fn deep_patch_rule_reaches_generation_time_filter() {
    // Without the rule, api-client deep-patches Option<Note>; the rule
    // switches it off for */request modules.
    let control = generator_for("deep_patch_control", op_spec())
        .split_request_response(true)
        .generate_to_string()
        .unwrap();
    assert!(control.contains("#[patch(name = \"Option<NotePatch>\")]"), "{control}");

    let out = generator_for("deep_patch_rule", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.rules.push(rule(
                RuleMatch {
                    module: Some("*/request".to_string()),
                    ..Default::default()
                },
                RuleApply {
                    deep_patch: Some(false),
                    ..Default::default()
                },
            ));
        })
        .generate_to_string()
        .unwrap();
    assert!(!out.contains("#[patch(name = \"Option<NotePatch>\")]"), "{out}");
}

// ─── Type-level patch rules ──────────────────────────────────────────────────

#[test]
fn module_scoped_patch_rule_strips_response_types() {
    let out = generator_for("patch_rule_module", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.rules.push(rule(
                RuleMatch {
                    module: Some("*/response".to_string()),
                    ..Default::default()
                },
                RuleApply {
                    patch: Some(false),
                    ..Default::default()
                },
            ));
        })
        .generate_to_string()
        .unwrap();

    // ThingResult (create_thing/response) loses the whole surface…
    let result_derives = derive_line_of(&out, "ThingResult");
    assert!(!result_derives.contains("Patch"), "{result_derives}");
    let result_position = out.find("pub struct ThingResult").unwrap();
    let above = &out[result_position.saturating_sub(700)..result_position];
    assert!(!above.contains("#[patch("), "companion attrs stripped:\n{above}");

    // …while request-side types keep theirs, deep-patch annotation
    // included.
    assert!(derive_line_of(&out, "CreateThingRequest").contains("Patch"), "{out}");
    assert!(out.contains("#[patch(name = \"Option<NotePatch>\")]"), "{out}");
}

#[test]
fn patch_rule_prunes_annotations_into_depatched_types() {
    // De-patching Note by rule: Note loses its machinery AND the
    // annotation on CreateThingRequest.note (whose companion no longer
    // exists) is pruned — the cross-type consistency the exact
    // `[types] patch = false` entry provides.
    let out = generator_for("patch_rule_crosstype", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.rules.push(rule(
                RuleMatch {
                    struct_: Some("Note".to_string()),
                    ..Default::default()
                },
                RuleApply {
                    patch: Some(false),
                    ..Default::default()
                },
            ));
        })
        .generate_to_string()
        .unwrap();

    assert!(!derive_line_of(&out, "Note").contains("Patch"), "{out}");
    assert!(!out.contains("NotePatch"), "annotation into Note pruned: {out}");
    assert!(derive_line_of(&out, "CreateThingRequest").contains("Patch"), "{out}");
}

#[test]
fn types_entry_beats_patch_rule_both_directions() {
    // Rule says off, exact entry re-enables.
    let out = generator_for("patch_rule_vs_types_on", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.rules.push(rule(
                RuleMatch {
                    struct_: Some("*".to_string()),
                    ..Default::default()
                },
                RuleApply {
                    patch: Some(false),
                    ..Default::default()
                },
            ));
            style.types.entry("Note".to_string()).or_default().patch = Some(true);
        })
        .generate_to_string()
        .unwrap();
    assert!(derive_line_of(&out, "Note").contains("Patch"), "{out}");
    assert!(!derive_line_of(&out, "CreateThingRequest").contains("Patch"), "{out}");

    // Rule says on (over a false baseline), exact entry disables.
    let out = generator_for("patch_rule_vs_types_off", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.patch = false;
            style.rules.push(rule(
                RuleMatch {
                    struct_: Some("*".to_string()),
                    ..Default::default()
                },
                RuleApply {
                    patch: Some(true),
                    ..Default::default()
                },
            ));
            style.types.entry("Note".to_string()).or_default().patch = Some(false);
        })
        .generate_to_string()
        .unwrap();
    assert!(!derive_line_of(&out, "Note").contains("Patch"), "{out}");
    assert!(derive_line_of(&out, "CreateThingRequest").contains("Patch"), "{out}");
}

#[test]
fn later_patch_rule_wins_and_baseline_reenable_works() {
    // Baseline false; rule 1 re-enables everything; rule 2 switches
    // Note back off — later wins.
    let out = generator_for("patch_rule_ordering", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.patch = false;
            style.rules.push(rule(
                RuleMatch {
                    struct_: Some("*".to_string()),
                    ..Default::default()
                },
                RuleApply {
                    patch: Some(true),
                    ..Default::default()
                },
            ));
            style.rules.push(rule(
                RuleMatch {
                    struct_: Some("Note".to_string()),
                    ..Default::default()
                },
                RuleApply {
                    patch: Some(false),
                    ..Default::default()
                },
            ));
        })
        .generate_to_string()
        .unwrap();
    assert!(derive_line_of(&out, "CreateThingRequest").contains("Patch"), "{out}");
    assert!(!derive_line_of(&out, "Note").contains("Patch"), "{out}");
}

#[test]
fn patch_payload_with_field_predicates_or_mixed_payload_errors() {
    // Field-scoped predicates cannot select types.
    let error = format!(
        "{:#}",
        generator_for("patch_rule_field_pred", op_spec())
            .split_request_response(true)
            .style(|style| {
                style.rules.push(rule(
                    RuleMatch {
                        field: Some("*".to_string()),
                        ..Default::default()
                    },
                    RuleApply {
                        patch: Some(false),
                        ..Default::default()
                    },
                ));
            })
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(
        error.contains("matches types") && error.contains("`module` and `struct`"),
        "{error}",
    );

    // A rule is single-scope: type-level and field-level payloads
    // cannot mix.
    let error = format!(
        "{:#}",
        generator_for("patch_rule_mixed", op_spec())
            .split_request_response(true)
            .style(|style| {
                style.rules.push(rule(
                    RuleMatch {
                        struct_: Some("*".to_string()),
                        ..Default::default()
                    },
                    RuleApply {
                        patch: Some(false),
                        deep_patch: Some(false),
                        ..Default::default()
                    },
                ));
            })
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(error.contains("single-scope") && error.contains("split"), "{error}");
}

// ─── Config errors and warnings ──────────────────────────────────────────────

#[test]
fn deep_patch_with_type_predicate_errors() {
    let error = format!(
        "{:#}",
        generator_for("deep_patch_type_pred", op_spec())
            .split_request_response(true)
            .style(|style| {
                style.rules.push(rule(
                    RuleMatch {
                        type_: Some("*OffsetDateTime".to_string()),
                        ..Default::default()
                    },
                    RuleApply {
                        deep_patch: Some(false),
                        ..Default::default()
                    },
                ));
            })
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(
        error.contains("generation time") && error.contains("[[rules]] #0"),
        "{error}",
    );
}

#[test]
fn module_predicate_without_partitioning_errors() {
    let error = format!(
        "{:#}",
        generator_for("module_no_partition", op_spec())
            .style(|style| {
                style.rules.push(rule(
                    RuleMatch {
                        module: Some("*/request".to_string()),
                        ..Default::default()
                    },
                    attrs_apply(&["serde(with = \"marker::x\")"]),
                ));
            })
            .generate_to_string()
            .unwrap_err(),
    );
    assert!(error.contains("partitioning is off"), "{error}");
}

#[test]
fn empty_match_and_empty_apply_error() {
    for (label, match_, apply) in [
        ("no-predicates", RuleMatch::default(), attrs_apply(&["serde(default)"])),
        (
            "no-payload",
            RuleMatch {
                field: Some("*".to_string()),
                ..Default::default()
            },
            RuleApply::default(),
        ),
    ] {
        let error = format!(
            "{:#}",
            generator_for(&format!("rule_empty_{label}"), op_spec())
                .split_request_response(true)
                .style(move |style| style.rules.push(rule(match_.clone(), apply.clone())))
                .generate_to_string()
                .unwrap_err(),
        );
        assert!(error.contains("[[rules]] #0"), "{label}: {error}");
    }
}

#[test]
fn zero_match_rule_warns_but_generation_succeeds() {
    generator_for("rule_zero_match", op_spec())
        .split_request_response(true)
        .style(|style| {
            style.rules.push(rule(
                RuleMatch {
                    struct_: Some("Nonexistent*".to_string()),
                    ..Default::default()
                },
                attrs_apply(&["serde(default)"]),
            ));
        })
        .generate_to_string()
        .expect("a zero-match rule warns, it does not fail generation");
}

#[test]
fn rules_parse_from_codegen_toml_and_reject_unknown_keys() {
    let config = StyleConfig::from_toml_str(
        "profile = \"api-client\"\n\
         [[rules]]\n\
         match = { module = \"*/request\", format = \"string/date-time\" }\n\
         apply = { field-attrs = [\"serde(with = \\\"time::serde::iso8601\\\")\"], deep-patch = false }\n",
        StyleConfig::api_client(),
    )
    .unwrap();
    assert_eq!(config.rules.len(), 1);
    assert_eq!(config.rules[0].match_.module.as_deref(), Some("*/request"));
    assert_eq!(config.rules[0].apply.deep_patch, Some(false));

    for (label, raw) in [
        (
            "match",
            "[[rules]]\nmatch = { modul = \"x\" }\napply = { deep-patch = false }\n",
        ),
        (
            "apply",
            "[[rules]]\nmatch = { field = \"*\" }\napply = { deep-patchh = false }\n",
        ),
    ] {
        let error = StyleConfig::from_toml_str(raw, StyleConfig::api_client()).unwrap_err();
        assert!(
            format!("{error:#}").contains("codegen.toml"),
            "{label} unknown key must fail: {error:#}",
        );
    }
}

#[test]
fn fields_tier_type_shorthand_still_parses() {
    // The pre-existing bare-string form of [fields] `type` keeps
    // working through the untagged enum.
    let config = StyleConfig::from_toml_str(
        "profile = \"api-client\"\n\
         [fields.\"Pet.id\"]\n\
         type = \"::my_crate::PetId\"\n",
        StyleConfig::api_client(),
    )
    .unwrap();
    let replacement = config.fields["Pet.id"].type_path.as_ref().unwrap();
    assert_eq!(replacement.type_path(), "::my_crate::PetId");
}

//! End-to-end pipeline regression tests against the checked-in specs.
//!
//! These assert on text patterns of the generated output — the stronger
//! guarantee (the output actually compiles and round-trips JSON) is
//! covered by the `petstore` / `sabre_booking` examples, which build the
//! checked-in generated code.

use openapi_codegen::{Generator, StyleProfile, render_file};

const PETSTORE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/specs/petstore.yaml");

fn generate(profile: StyleProfile, partition: bool) -> String {
    Generator::new(concat!(env!("CARGO_MANIFEST_DIR"), "/specs/petstore.yaml"))
        .profile(profile)
        .partition_by_operation(partition)
        .generate_to_string()
        .unwrap()
}

#[test]
fn api_client_profile_shape() {
    let out = generate(StyleProfile::ApiClient, true);

    // Partitioned into per-operation modules + shared.
    assert!(out.contains("pub mod create_pet {"));
    assert!(out.contains("pub mod get_pet {"));
    assert!(out.contains("pub mod shared {"));

    // The full attribute stack in the exact expected order.
    let stack = [
        "#[serde_with::skip_serializing_none]",
        "#[cfg_attr(feature = \"schemars\", derive(schemars::JsonSchema))]",
        "#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Patch)]",
        "#[serde(rename_all = \"camelCase\")]",
        "#[patch(attribute(serde_with::skip_serializing_none))]",
        "#[patch(attribute(derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)))]",
        "#[patch(attribute(serde(default, rename_all = \"camelCase\")))]",
        "#[cfg_attr(feature = \"schemars\", patch(attribute(derive(schemars::JsonSchema))))]",
    ];
    let mut position = 0;
    for line in stack {
        let found = out[position..]
            .find(line)
            .unwrap_or_else(|| panic!("missing or out-of-order attribute: {line}"));
        position += found;
    }

    // Wire-shape knobs: bare Option fields without per-field serde noise,
    // no chrono/uuid/NonZero, no constrained-string newtype.
    assert!(out.contains("pub wants_newsletter: ::std::option::Option<bool>"));
    assert!(out.contains("pub photo_urls: ::std::option::Option<::std::vec::Vec<"));
    assert!(out.contains("pub tag_count: ::std::option::Option<i32>"));
    assert!(out.contains("pub id: ::std::string::String"));
    assert!(!out.contains("chrono"));
    assert!(!out.contains("uuid::Uuid"));
    assert!(!out.contains("NonZero"));
    assert!(!out.contains("IdempotencyKey"));
    assert!(!out.contains("skip_serializing_if"));

    // allOf composition and deep patches.
    assert!(out.contains("#[serde(flatten)]\n        pub pet: Pet"));
    assert!(out.contains("#[patch(name = \"Option<CategoryPatch>\")]"));

    // Enum defaults to the first unit variant.
    assert!(out.contains("impl ::std::default::Default for PetStatus"));
}

#[test]
fn typify_profile_keeps_upstream_shape() {
    let out = generate(StyleProfile::Typify, false);

    // Upstream defaults: constrained-string newtype, NonZero integers,
    // bare Vec arrays with skip_serializing_if, chrono/uuid types.
    assert!(out.contains("IdempotencyKey"));
    assert!(out.contains("NonZeroU32"));
    assert!(out.contains("skip_serializing_if = \"::std::vec::Vec::is_empty\""));
    assert!(out.contains("chrono"));
    assert!(out.contains("uuid::Uuid"));
    // No fork-profile attributes leak in.
    assert!(!out.contains("skip_serializing_none"));
    assert!(!out.contains("struct_patch"));
}

#[test]
fn patches_are_applied() {
    let out = Generator::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/specs/sabre-booking/spec.openapi.yaml"
    ))
    .patches_dir(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/specs/sabre-booking/patches"
    ))
    .profile(StyleProfile::ApiClient)
    .partition_by_operation(true)
    .generate_to_string()
    .unwrap();

    // The checked-in patch removes `flightCoupons` from FlightTicket's
    // required list, so it must come out as Option<Vec<...>>. Compare
    // whitespace-insensitively — prettyplease wraps the long type.
    let condensed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        condensed.contains(
            "pub flight_coupons: ::std::option::Option< \
             ::std::vec::Vec<FlightReferenceCoupon>, >"
        ),
        "patch 001-flighttickets-optional-coupons was not applied",
    );
}

/// The body of the top-level `pub mod <name> { ... }` block in `source`.
/// prettyplease emits top-level modules at column 0, so the next
/// unindented `pub mod ` (or end of input) closes the slice; nested
/// modules like `error` / `defaults` are indented and don't match.
fn module_body<'a>(source: &'a str, name: &str) -> &'a str {
    let open = format!("pub mod {name} {{");
    let start = source
        .find(&open)
        .unwrap_or_else(|| panic!("no `pub mod {name}` in output"));
    let rest = &source[start + open.len()..];
    let end = rest.find("\npub mod ").unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn staged_pipeline_matches_one_shot_byte_for_byte() {
    for (profile, partition) in [
        (StyleProfile::Typify, false),
        (StyleProfile::Typify, true),
        (StyleProfile::ApiClient, false),
        (StyleProfile::ApiClient, true),
    ] {
        let generator = Generator::new(PETSTORE)
            .profile(profile)
            .partition_by_operation(partition);
        let one_shot = generator.generate_to_string().unwrap();

        // The long route: every stage spelled out, ending in the
        // standalone render helper.
        let file = generator
            .load()
            .unwrap()
            .lower()
            .unwrap()
            .build_types()
            .unwrap()
            .into_file()
            .unwrap();
        let staged = render_file(&file, PETSTORE);
        assert_eq!(
            one_shot, staged,
            "staged output diverged ({profile:?}, partition: {partition})",
        );

        // The short route: the render convenience on the final stage.
        let rendered = generator
            .load()
            .unwrap()
            .lower()
            .unwrap()
            .build_types()
            .unwrap()
            .render()
            .unwrap();
        assert_eq!(
            one_shot, rendered,
            "render() output diverged ({profile:?}, partition: {partition})",
        );
    }
}

#[test]
fn partition_override_moves_type_between_modules() {
    let generator = Generator::new(PETSTORE)
        .profile(StyleProfile::ApiClient)
        .partition_by_operation(true);

    // Untouched, the orphan `Dog` schema lands in `shared`.
    let default_out = generator.generate_to_string().unwrap();
    assert!(module_body(&default_out, "shared").contains("pub struct Dog"));

    let mut stage = generator.load().unwrap().lower().unwrap();
    let partition = stage.partition_mut().unwrap();
    assert_eq!(partition.by_schema["Dog"], "shared");
    partition
        .by_schema
        .insert("Dog".to_string(), "get_pet".to_string());
    let out = stage.build_types().unwrap().render().unwrap();

    assert!(module_body(&out, "get_pet").contains("pub struct Dog"));
    assert!(!module_body(&out, "shared").contains("pub struct Dog"));
}

#[test]
fn split_role_classification_on_petstore() {
    let stage = Generator::new(PETSTORE)
        .profile(StyleProfile::ApiClient)
        .split_request_response(true)
        .load()
        .unwrap()
        .lower()
        .unwrap()
        .build_types()
        .unwrap();
    let partition = stage.partition().unwrap();

    // Reachable from exactly one (operation, role): the op's role leaf.
    assert_eq!(partition.by_schema["CreatePetRequest"], "create_pet/request");
    // Response of both operations, never a request: shared/response.
    assert_eq!(partition.by_schema["Pet"], "shared/response");
    // Category sits in CreatePetRequest (request role) and in Pet
    // (response role): both roles → the shared/common catch-all. Its
    // transitive reference CategoryRef inherits the same usages.
    assert_eq!(partition.by_schema["Category"], "shared/common");
    assert_eq!(partition.by_schema["CategoryRef"], "shared/common");
    // Orphans (unreachable from any operation) follow the shared
    // classification with no roles: shared/common.
    assert_eq!(partition.by_schema["Dog"], "shared/common");
    assert!(partition.unreachable.contains(&"Dog".to_string()));

    // PetStatus is role-classified shared/response at compute time…
    assert_eq!(partition.by_schema["PetStatus"], "shared/response");
    // …but it generates as a simple (all-unit-variant) enum, so the
    // final Rust partition — resolved with typify's view in hand —
    // routes it to shared/enums.
    let rust_partition = partition.to_rust_partition(stage.type_space());
    assert_eq!(rust_partition["PetStatus"], "shared/enums");
    assert_eq!(rust_partition["Pet"], "shared/response");
    assert_eq!(rust_partition["CreatePetRequest"], "create_pet/request");
    assert_eq!(rust_partition["Dog"], "shared/common");
}

#[test]
fn split_mode_emits_nested_modules_in_single_file() {
    // --split-request-response with a single-file output: the nested
    // module tree renders inline.
    let out = Generator::new(PETSTORE)
        .profile(StyleProfile::ApiClient)
        .split_request_response(true)
        .generate_to_string()
        .unwrap();

    let condensed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(condensed.contains("pub mod create_pet { pub mod request {"));
    assert!(condensed.contains("pub mod shared { pub mod common {"));
    assert!(condensed.contains("pub mod enums {"));
    // Leaf modules glob the shared leaves with super-chains.
    assert!(condensed.contains("use super::super::shared::request::*;"));
    assert!(condensed.contains("use super::super::shared::enums::*;"));
}

/// A scratch directory under the target tmp dir, unique per test.
fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    dir
}

fn tree_generator() -> Generator {
    Generator::new(PETSTORE)
        .profile(StyleProfile::ApiClient)
        .split_request_response(true)
}

/// Every `.rs` file under `dir`, as sorted `dir`-relative path strings.
fn rs_files(dir: &std::path::Path) -> Vec<String> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, acc: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, acc);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                acc.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let mut acc = Vec::new();
    walk(dir, dir, &mut acc);
    acc.sort();
    acc
}

#[test]
fn dir_output_writes_expected_tree() {
    let dir = scratch_dir("petstore_tree_layout");
    tree_generator().generate_to_dir(&dir).unwrap();

    assert_eq!(
        rs_files(&dir),
        [
            "create_pet/mod.rs",
            "create_pet/request.rs",
            "create_pet/response.rs",
            "get_pet/mod.rs",
            "get_pet/request.rs",
            "get_pet/response.rs",
            "mod.rs",
            "shared/common.rs",
            "shared/enums.rs",
            "shared/mod.rs",
            "shared/request.rs",
            "shared/response.rs",
        ]
    );

    // The root mod.rs carries the header and declares the top-level
    // modules; per-operation mod.rs files declare the role leaves.
    let root = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
    assert!(root.starts_with("// @generated by openapi-codegen from"));
    assert!(root.contains("pub mod create_pet;"));
    assert!(root.contains("pub mod get_pet;"));
    assert!(root.contains("pub mod shared;"));
    let op = std::fs::read_to_string(dir.join("create_pet/mod.rs")).unwrap();
    assert!(op.contains("pub mod request;"));
    assert!(op.contains("pub mod response;"));

    // Helper modules stay inline in the leaf files instead of becoming
    // files of their own.
    let leaf = std::fs::read_to_string(dir.join("create_pet/request.rs")).unwrap();
    assert!(leaf.starts_with("// @generated by openapi-codegen from"));
    assert!(leaf.contains("pub mod error {"));
    assert!(leaf.contains("pub struct CreatePetRequest"));

    // Writes are idempotent: a second run leaves mtimes untouched.
    let stamp = |path: &std::path::Path| std::fs::metadata(path).unwrap().modified().unwrap();
    let before = stamp(&dir.join("shared/response.rs"));
    tree_generator().generate_to_dir(&dir).unwrap();
    assert_eq!(before, stamp(&dir.join("shared/response.rs")));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dir_output_removes_stale_generated_files() {
    let dir = scratch_dir("petstore_tree_stale");

    // Simulate a previous run that generated modules which no longer
    // exist, plus a user-owned file that must survive.
    std::fs::create_dir_all(dir.join("old_op")).unwrap();
    std::fs::write(
        dir.join("old_op/request.rs"),
        "// @generated by openapi-codegen from old.yaml\npub struct Gone;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("removed.rs"),
        "// @generated by openapi-codegen from old.yaml\npub struct Gone;\n",
    )
    .unwrap();
    std::fs::write(dir.join("user_owned.rs"), "pub struct Mine;\n").unwrap();

    tree_generator().generate_to_dir(&dir).unwrap();

    // Stale generated files (and their emptied directory) are gone; the
    // marker-less user file survives.
    assert!(!dir.join("old_op").exists());
    assert!(!dir.join("removed.rs").exists());
    assert!(dir.join("user_owned.rs").exists());
    assert!(dir.join("mod.rs").exists());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn syn_file_edits_appear_in_render() {
    let mut file = Generator::new(PETSTORE)
        .profile(StyleProfile::ApiClient)
        .partition_by_operation(true)
        .load()
        .unwrap()
        .lower()
        .unwrap()
        .build_types()
        .unwrap()
        .into_file()
        .unwrap();
    file.items.push(syn::parse_quote! {
        pub const PIPELINE_EDITED: bool = true;
    });

    let out = render_file(&file, PETSTORE);
    assert!(out.contains("pub const PIPELINE_EDITED: bool = true;"));
    assert!(out.starts_with("// @generated by openapi-codegen from"));
}

//! Exercises the petstore folder tree generated with
//! `emit-style = "condensed"` (IR engine): same module layout and trait
//! surface as `petstore_tree`, but string-enum conversion ladders are
//! one `impl_string_enum!` invocation each and the per-module `error`
//! mods are re-exports of the shared `support::error`.
//!
//! The assertions here are the capability-equivalence check for the
//! condensed style: everything `petstore_tree` asserts, **plus** the
//! full conversion ladder the macro now generates (`Display`,
//! `FromStr`, all three `TryFrom` forms, `Default`) and the historical
//! `<module>::error::ConversionError` path.
//!
//! Regenerate with:
//!
//! ```text
//! cargo run -- generate --spec specs/petstore.yaml --profile api-client \
//!     --engine ir --config examples/codegen-condensed.toml \
//!     --split-request-response \
//!     --output-dir examples/generated_tree/petstore_condensed
//! ```

// Generated modules glob-import each other for cross-module type
// references; not every import ends up used. Clippy style lints don't
// apply to generated code.
#[allow(unused_imports, dead_code, clippy::all)]
#[path = "generated_tree/petstore_condensed/mod.rs"]
mod petstore;

use petstore::create_pet::request::CreatePetRequest;
use petstore::shared::common::Category;
use petstore::shared::enums::PetStatus;
use petstore::shared::response::Pet;

fn main() {
    // The wire surface is identical to the expanded style.
    let request = CreatePetRequest {
        name: "Rex".to_string(),
        category: Some(Category {
            name: Some("dogs".to_string()),
            parent: None,
        }),
        wants_newsletter: Some(true),
        ..Default::default()
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({
            "name": "Rex",
            "category": { "name": "dogs" },
            "wantsNewsletter": true,
        }),
    );
    let round_tripped: CreatePetRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(round_tripped, request);

    // The conversion ladder the `impl_string_enum!` invocation expands
    // to — every trait the expanded style wrote out per enum.
    assert_eq!(PetStatus::Available.to_string(), "available"); // Display
    let parsed: PetStatus = "pending".parse().unwrap(); // FromStr
    assert_eq!(parsed, PetStatus::Pending);
    assert_eq!(PetStatus::try_from("sold").unwrap(), PetStatus::Sold);
    assert_eq!(
        PetStatus::try_from(&String::from("available")).unwrap(),
        PetStatus::Available,
    );
    assert_eq!(
        PetStatus::try_from(String::from("pending")).unwrap(),
        PetStatus::Pending,
    );
    assert_eq!(PetStatus::default(), PetStatus::Available); // Default

    // The error type keeps its historical per-module path (now a
    // re-export of the one shared type in `support::error`).
    let error: petstore::shared::enums::error::ConversionError =
        "not-a-status".parse::<PetStatus>().unwrap_err();
    assert_eq!(error.to_string(), "invalid value");
    let _same_type: petstore::support::error::ConversionError = error;

    // Struct machinery (Default synthesis through enum Default) is
    // untouched.
    let pet = Pet {
        id: "3fa85f64-5717-4562-b3fc-2c963f66afa6".to_string(),
        name: "Rex".to_string(),
        status: Some(PetStatus::default()),
        ..Default::default()
    };
    assert_eq!(pet.status, Some(PetStatus::Available));

    println!("petstore_tree_condensed example: all assertions passed");
}

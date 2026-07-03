//! Exercises the generated petstore types: construction, JSON round-trip,
//! and a deep `struct_patch` merge.
//!
//! Regenerate the types with:
//!
//! ```text
//! cargo run -- generate --spec specs/petstore.yaml --profile api-client \
//!     --partition-by-operation --output examples/generated/petstore.rs
//! ```

// Generated modules glob-import each other for cross-module type
// references; not every import ends up used. Clippy style lints don't
// apply to generated code.
#[allow(unused_imports, dead_code, clippy::all)]
#[path = "generated/petstore.rs"]
mod petstore;

use petstore::create_pet::{CreatePetRequest, CreatePetRequestPatch};
use petstore::shared::{Category, Pet, PetStatus};
use struct_patch::Patch as _;

fn main() {
    // Non-required fields are bare `Option<T>` — a struct literal with
    // `..Default::default()` reads like the hand-written style.
    let request = CreatePetRequest {
        name: "Rex".to_string(),
        category: Some(Category {
            name: Some("dogs".to_string()),
            parent: None,
        }),
        wants_newsletter: Some(true),
        ..Default::default()
    };

    // `skip_serializing_none` keeps absent fields off the wire; the
    // struct-level `rename_all = "camelCase"` restores the spec's names.
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({
            "name": "Rex",
            "category": { "name": "dogs" },
            "wantsNewsletter": true,
        }),
    );

    // Missing JSON keys deserialize as `None` without per-field serde
    // attributes.
    let round_tripped: CreatePetRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(round_tripped, request);

    // The `#[patch(name = "Option<CategoryPatch>")]` rewrite makes patches
    // recurse: this patch updates `category.name` without clobbering the
    // other `Category` fields.
    let patch: CreatePetRequestPatch = serde_json::from_value(serde_json::json!({
        "category": { "name": "good dogs" },
    }))
    .unwrap();
    let mut patched = request.clone();
    patched.apply(patch);
    assert_eq!(
        patched.category.as_ref().unwrap().name.as_deref(),
        Some("good dogs"),
    );
    assert_eq!(patched.name, "Rex");
    assert_eq!(patched.wants_newsletter, Some(true));

    // Enums default to their first unit variant so `Pet::default()` (via
    // the derived struct `Default`) is constructible even though `status`
    // is an enum.
    let pet = Pet {
        id: "3fa85f64-5717-4562-b3fc-2c963f66afa6".to_string(),
        name: "Rex".to_string(),
        status: Some(PetStatus::default()),
        ..Default::default()
    };
    assert_eq!(pet.status, Some(PetStatus::Available));

    println!("petstore example: all assertions passed");
}

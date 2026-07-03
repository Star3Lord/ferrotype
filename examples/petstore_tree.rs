//! Exercises the petstore types generated in folder-tree form: one
//! directory per operation with `request.rs` / `response.rs` leaves plus
//! `shared/{request,response,enums,common}`, mounted from a root
//! `mod.rs`. Small enough to eyeball the whole layout:
//!
//! ```text
//! generated_tree/petstore/
//!   mod.rs
//!   create_pet/{mod.rs, request.rs, response.rs}
//!   get_pet/{mod.rs, request.rs, response.rs}
//!   shared/{mod.rs, common.rs, enums.rs, request.rs, response.rs}
//! ```
//!
//! Regenerate with:
//!
//! ```text
//! cargo run -- generate --spec specs/petstore.yaml --profile api-client \
//!     --split-request-response --output-dir examples/generated_tree/petstore
//! ```

// Generated modules glob-import each other for cross-module type
// references; not every import ends up used. Clippy style lints don't
// apply to generated code.
#[allow(unused_imports, dead_code, clippy::all)]
#[path = "generated_tree/petstore/mod.rs"]
mod petstore;

use petstore::create_pet::request::CreatePetRequest;
use petstore::shared::common::Category;
use petstore::shared::enums::PetStatus;
use petstore::shared::response::Pet;

fn main() {
    // Role classification: CreatePetRequest is reachable only from
    // createPet's request body → create_pet::request. Category appears
    // in both the request and the response trees → shared::common.
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

    // Pet is a response of both operations → shared::response; its
    // status enum is a shared simple enum → shared::enums.
    let pet = Pet {
        id: "3fa85f64-5717-4562-b3fc-2c963f66afa6".to_string(),
        name: "Rex".to_string(),
        status: Some(PetStatus::default()),
        ..Default::default()
    };
    assert_eq!(pet.status, Some(PetStatus::Available));

    println!("petstore_tree example: all assertions passed");
}

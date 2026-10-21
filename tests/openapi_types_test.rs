#![cfg(feature = "openapi-schemars")]
#![allow(dead_code)] // the types only exist to be described
//! OpenAPI schemas derived from Rust types

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ullun::openapi::{OpenApiInfo, Operation};
use ullun::prelude::*;
use ullun::testing::TestClient;

#[derive(Deserialize, JsonSchema)]
struct NewUser {
    /// Display name
    name: String,
    age: Option<u8>,
    role: Role,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Role {
    Admin,
    Member,
}

#[derive(Serialize, JsonSchema)]
struct User {
    id: u64,
    name: String,
    role: Role,
    tags: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
struct Paging {
    /// Page size
    limit: Option<u32>,
    after: u64,
}

async fn doc(op: Operation) -> Value {
    let app = App::new()
        .post("/users", |_req: Request| async { Ok(Response::text("ok")) })
        .doc("POST", "/users", op)
        .openapi(OpenApiInfo::new("Users", "1.0.0"));
    TestClient::new(app)
        .get("/openapi.json")
        .send()
        .await
        .json::<Value>()
}

#[tokio::test]
async fn request_and_response_types_become_component_schemas() {
    let doc = doc(Operation::new("Create a user")
        .request_type::<NewUser>()
        .response_type::<User>(201, "Created"))
    .await;

    let body =
        &doc["paths"]["/users"]["post"]["requestBody"]["content"]["application/json"]["schema"];
    assert_eq!(body["$ref"], "#/components/schemas/NewUser");
    let reply = &doc["paths"]["/users"]["post"]["responses"]["201"]["content"]["application/json"]
        ["schema"];
    assert_eq!(reply["$ref"], "#/components/schemas/User");

    let schemas = &doc["components"]["schemas"];
    let new_user = &schemas["NewUser"];
    assert_eq!(new_user["type"], "object");
    assert_eq!(
        new_user["properties"]["name"]["description"],
        "Display name"
    );
    // Option<T> is optional, the others are required
    let required: Vec<&str> = new_user["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(required.contains(&"name") && required.contains(&"role"));
    assert!(!required.contains(&"age"));
    // nested types are components too, with serde renames applied
    assert_eq!(
        new_user["properties"]["role"]["$ref"],
        "#/components/schemas/Role"
    );
    assert_eq!(
        schemas["Role"]["enum"],
        serde_json::json!(["admin", "member"])
    );
    assert_eq!(schemas["User"]["properties"]["tags"]["type"], "array");
    // the built-in error schema is still there
    assert_eq!(schemas["Error"]["type"], "object");
}

#[tokio::test]
async fn no_json_schema_dollar_defs_leak_into_the_document() {
    let doc = doc(Operation::new("x").request_type::<NewUser>()).await;
    let text = doc.to_string();
    assert!(!text.contains("$defs"), "{text}");
    assert!(!text.contains("\"$schema\""), "{text}");
}

#[tokio::test]
async fn query_type_lists_one_parameter_per_field() {
    let doc = doc(Operation::new("List").query_type::<Paging>()).await;
    let params = doc["paths"]["/users"]["post"]["parameters"]
        .as_array()
        .unwrap();
    let find = |name: &str| params.iter().find(|p| p["name"] == name).unwrap();
    assert_eq!(find("limit")["in"], "query");
    assert_eq!(find("limit")["required"], false);
    assert_eq!(find("limit")["description"], "Page size");
    assert_eq!(find("after")["required"], true);
    assert_eq!(params.len(), 2);
}

#[tokio::test]
async fn form_bodies_and_shared_types_work_together() {
    let doc = doc(Operation::new("Both")
        .request_form_type::<Paging>()
        .response_type::<User>(200, "ok")
        .response_type::<Vec<User>>(207, "many"))
    .await;
    let form = &doc["paths"]["/users"]["post"]["requestBody"]["content"]
        ["application/x-www-form-urlencoded"]["schema"];
    assert_eq!(form["$ref"], "#/components/schemas/Paging");
    let many = &doc["paths"]["/users"]["post"]["responses"]["207"]["content"]["application/json"]
        ["schema"];
    assert_eq!(many["type"], "array");
    assert_eq!(many["items"]["$ref"], "#/components/schemas/User");
    assert!(doc["components"]["schemas"]["Paging"].is_object());
    assert!(doc["components"]["schemas"]["Role"].is_object());
}

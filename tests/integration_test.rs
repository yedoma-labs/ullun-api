use ullun::prelude::*;

// Simple integration tests to verify core functionality

#[test]
fn test_error_types() {
    let err = Error::bad_request("test");
    assert_eq!(err.status_code(), 400);
    assert_eq!(err.message(), "test");

    let err = Error::not_found("missing");
    assert_eq!(err.status_code(), 404);
    
    let err = Error::custom(418, "teapot");
    assert_eq!(err.status_code(), 418);
}

#[test]
fn test_params() {
    let mut params = Params::new();
    params.insert("id".to_string(), "123".to_string());
    
    assert_eq!(params.get("id").unwrap(), "123");
    assert!(params.get("missing").is_err());
    assert_eq!(params.get_opt("missing"), None);
}

#[test]
fn test_query() {
    let query = Query::parse_query_string("name=Alice&age=30");
    
    assert_eq!(query.get("name"), Some("Alice"));
    assert_eq!(query.get("age"), Some("30"));
    assert_eq!(query.get("missing"), None);
}

#[test]
fn test_response_builders() {
    let resp = Response::text("hello");
    assert_eq!(resp.status, http::StatusCode::OK);
    
    let resp = Response::json(serde_json::json!({"key": "value"}));
    assert_eq!(resp.status, http::StatusCode::OK);
    assert!(resp.body.len() > 0);
}

#[tokio::test]
async fn test_app_builder() {
    // Just test that the builder pattern works
    let _app = App::new()
        .get("/", |_req| async { Ok(Response::text("ok")) })
        .post("/data", |_req| async { Ok(Response::text("created")) })
        .middleware(ullun::middleware::logger);
    
    // If we got here without panicking, builder works
    assert!(true);
}

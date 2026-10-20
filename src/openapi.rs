//! OpenAPI 3.1 document generated from the registered routes
//!
//! Every route you register shows up in the document with its method, path and
//! path parameters. Attach details with [`Operation`] through `App::doc` (or
//! `RouteGroup::doc`), then publish the document with `App::openapi`:
//!
//! ```no_run
//! use ullun::openapi::{schema, OpenApiInfo, Operation};
//! use ullun::prelude::*;
//!
//! # async fn run() {
//! App::new()
//!     .get_with_params("/users/{id}", |p: Params| async move {
//!         Ok(Response::json(serde_json::json!({ "id": p.get("id")? })))
//!     })
//!     .doc(
//!         "GET",
//!         "/users/{id}",
//!         Operation::new("Fetch a user")
//!             .tag("users")
//!             .path_param("id", schema::integer(), "User id")
//!             .response_json(200, "The user", schema::object([("id", schema::integer())], ["id"]))
//!             .error_response(404, "No such user"),
//!     )
//!     .openapi(OpenApiInfo::new("Users API", "1.0.0").swagger_ui("/docs"))
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! The document is built when the server starts, so routes added after the
//! `openapi` call are included too. Static file routes are left out.
//! `swagger_ui` serves a page that loads Swagger UI from unpkg (pinned version
//! with subresource integrity hashes); leave it off if you do not want a
//! third-party script on your server.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Document metadata and where to publish it
#[derive(Debug, Clone)]
pub struct OpenApiInfo {
    pub(crate) title: String,
    pub(crate) version: String,
    pub(crate) description: Option<String>,
    pub(crate) servers: Vec<String>,
    pub(crate) spec_path: String,
    pub(crate) docs_path: Option<String>,
}

impl OpenApiInfo {
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
            servers: Vec::new(),
            spec_path: "/openapi.json".to_string(),
            docs_path: None,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add a server URL, e.g. `https://api.example.com`
    pub fn server(mut self, url: impl Into<String>) -> Self {
        self.servers.push(url.into());
        self
    }

    /// Where the JSON document is served (default `/openapi.json`)
    pub fn spec_path(mut self, path: impl Into<String>) -> Self {
        self.spec_path = path.into();
        self
    }

    /// Also serve a Swagger UI page at `path`
    pub fn swagger_ui(mut self, path: impl Into<String>) -> Self {
        self.docs_path = Some(path.into());
        self
    }
}

/// Description of one operation (method + path)
#[derive(Debug, Clone, Default)]
pub struct Operation {
    summary: String,
    description: Option<String>,
    tags: Vec<String>,
    operation_id: Option<String>,
    deprecated: bool,
    secured: bool,
    parameters: Vec<Value>,
    request_body: Option<Value>,
    responses: BTreeMap<u16, Value>,
}

impl Operation {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            ..Default::default()
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    pub fn operation_id(mut self, id: impl Into<String>) -> Self {
        self.operation_id = Some(id.into());
        self
    }

    pub fn deprecated(mut self) -> Self {
        self.deprecated = true;
        self
    }

    /// Requires a bearer token (`Authorization: Bearer ...`)
    pub fn secured(mut self) -> Self {
        self.secured = true;
        self
    }

    /// Describe a path parameter (undeclared ones default to `string`)
    pub fn path_param(self, name: &str, schema: Value, description: &str) -> Self {
        self.param("path", name, schema, description, true)
    }

    pub fn query_param(self, name: &str, schema: Value, description: &str, required: bool) -> Self {
        self.param("query", name, schema, description, required)
    }

    pub fn header_param(
        self,
        name: &str,
        schema: Value,
        description: &str,
        required: bool,
    ) -> Self {
        self.param("header", name, schema, description, required)
    }

    fn param(
        mut self,
        location: &str,
        name: &str,
        schema: Value,
        description: &str,
        required: bool,
    ) -> Self {
        self.parameters
            .retain(|p| !(p["in"] == location && p["name"] == name));
        let mut p = json!({ "name": name, "in": location, "required": required, "schema": schema });
        if !description.is_empty() {
            p["description"] = json!(description);
        }
        self.parameters.push(p);
        self
    }

    /// JSON request body with the given schema
    pub fn request_json(mut self, schema: Value) -> Self {
        self.request_body = Some(json!({
            "required": true,
            "content": { "application/json": { "schema": schema } }
        }));
        self
    }

    /// `application/x-www-form-urlencoded` request body
    pub fn request_form(mut self, schema: Value) -> Self {
        self.request_body = Some(json!({
            "required": true,
            "content": { "application/x-www-form-urlencoded": { "schema": schema } }
        }));
        self
    }

    /// Response without a body description
    pub fn response(mut self, status: u16, description: &str) -> Self {
        self.responses
            .insert(status, json!({ "description": description }));
        self
    }

    pub fn response_json(mut self, status: u16, description: &str, schema: Value) -> Self {
        self.responses.insert(
            status,
            json!({
                "description": description,
                "content": { "application/json": { "schema": schema } }
            }),
        );
        self
    }

    /// Response using the framework's error body `{ "error": "...", "status": 404 }`
    pub fn error_response(self, status: u16, description: &str) -> Self {
        self.response_json(
            status,
            description,
            json!({ "$ref": "#/components/schemas/Error" }),
        )
    }
}

/// Small helpers for JSON Schema values
pub mod schema {
    use serde_json::{json, Map, Value};

    pub fn string() -> Value {
        json!({ "type": "string" })
    }
    pub fn integer() -> Value {
        json!({ "type": "integer" })
    }
    pub fn number() -> Value {
        json!({ "type": "number" })
    }
    pub fn boolean() -> Value {
        json!({ "type": "boolean" })
    }
    pub fn array(items: Value) -> Value {
        json!({ "type": "array", "items": items })
    }
    /// Object with named properties; `required` lists the mandatory ones
    pub fn object<'a>(
        properties: impl IntoIterator<Item = (&'a str, Value)>,
        required: impl IntoIterator<Item = &'a str>,
    ) -> Value {
        let props: Map<String, Value> = properties
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let required: Vec<&str> = required.into_iter().collect();
        let mut o = json!({ "type": "object", "properties": props });
        if !required.is_empty() {
            o["required"] = json!(required);
        }
        o
    }
}

/// How a route was registered
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteKind {
    Http,
    #[cfg_attr(not(feature = "websocket"), allow(dead_code))]
    WebSocket,
    /// Registered, but not part of the documentation (static files)
    Hidden,
}

pub(crate) struct RouteInfo {
    pub method: String,
    pub path: String,
    pub kind: RouteKind,
}

/// `{*rest}` catch-alls become plain `{rest}`; returns the parameter names
fn openapi_path(path: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        let name = rest[start + 1..start + end].trim_start_matches('*');
        out.push('{');
        out.push_str(name);
        out.push('}');
        names.push(name.to_string());
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    (out, names)
}

pub(crate) fn build(
    info: &OpenApiInfo,
    routes: &[RouteInfo],
    docs: &std::collections::HashMap<(String, String), Operation>,
) -> Value {
    let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let mut uses_bearer = false;

    for route in routes.iter().filter(|r| r.kind != RouteKind::Hidden) {
        let (doc_path, names) = openapi_path(&route.path);
        let op = docs.get(&(route.method.clone(), route.path.clone()));

        let mut operation = Map::new();
        if let Some(op) = op {
            if !op.summary.is_empty() {
                operation.insert("summary".into(), json!(op.summary));
            }
            if let Some(d) = &op.description {
                operation.insert("description".into(), json!(d));
            }
            if !op.tags.is_empty() {
                operation.insert("tags".into(), json!(op.tags));
            }
            if let Some(id) = &op.operation_id {
                operation.insert("operationId".into(), json!(id));
            }
            if op.deprecated {
                operation.insert("deprecated".into(), json!(true));
            }
            if op.secured {
                uses_bearer = true;
                operation.insert("security".into(), json!([{ "bearerAuth": [] }]));
            }
        }

        // every {param} in the path is declared; documented ones win
        let mut parameters: Vec<Value> = names
            .iter()
            .filter(|n| {
                !op.is_some_and(|o| o.parameters.iter().any(|p| p["in"] == "path" && p["name"] == n.as_str()))
            })
            .map(|n| json!({ "name": n, "in": "path", "required": true, "schema": { "type": "string" } }))
            .collect();
        if let Some(op) = op {
            parameters.extend(op.parameters.iter().cloned());
        }
        if !parameters.is_empty() {
            operation.insert("parameters".into(), json!(parameters));
        }
        if let Some(body) = op.and_then(|o| o.request_body.clone()) {
            operation.insert("requestBody".into(), body);
        }

        let mut responses = Map::new();
        match (op, route.kind) {
            (Some(op), _) if !op.responses.is_empty() => {
                for (status, r) in &op.responses {
                    responses.insert(status.to_string(), r.clone());
                }
            }
            (_, RouteKind::WebSocket) => {
                responses.insert(
                    "101".into(),
                    json!({ "description": "Switching Protocols (WebSocket)" }),
                );
            }
            _ => {
                responses.insert("200".into(), json!({ "description": "OK" }));
            }
        }
        operation.insert("responses".into(), Value::Object(responses));

        paths
            .entry(doc_path)
            .or_default()
            .insert(route.method.to_ascii_lowercase(), Value::Object(operation));
    }

    let mut info_obj = json!({ "title": info.title, "version": info.version });
    if let Some(d) = &info.description {
        info_obj["description"] = json!(d);
    }

    let mut components = json!({
        "schemas": {
            "Error": {
                "type": "object",
                "properties": { "error": { "type": "string" }, "status": { "type": "integer" } },
                "required": ["error", "status"]
            }
        }
    });
    if uses_bearer {
        components["securitySchemes"] =
            json!({ "bearerAuth": { "type": "http", "scheme": "bearer", "bearerFormat": "JWT" } });
    }

    let mut doc = json!({
        "openapi": "3.1.0",
        "info": info_obj,
        "paths": paths,
        "components": components,
    });
    if !info.servers.is_empty() {
        doc["servers"] = json!(info
            .servers
            .iter()
            .map(|u| json!({ "url": u }))
            .collect::<Vec<_>>());
    }
    doc
}

/// Swagger UI page for the document at `spec_url`
pub(crate) fn swagger_ui_html(title: &str, spec_url: &str) -> String {
    let title = title
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    // JSON string literal is safe inside the script block once '<' is escaped
    let spec = serde_json::to_string(spec_url)
        .unwrap_or_else(|_| "\"\"".into())
        .replace('<', "\\u003c");
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5.34.0/swagger-ui.css"
  integrity="sha384-4c3vsE5f+trhFyHrFrLM1fFGUn531NIaC7dcqEWIJdoRQJ5CkJbSuSCwXLpaS1KF" crossorigin="anonymous">
</head>
<body>
<div id="ui"></div>
<script src="https://unpkg.com/swagger-ui-dist@5.34.0/swagger-ui-bundle.js"
  integrity="sha384-9Td7MKiIr2S83E1kZyyVi8KdJw/S8CpAvu0llLyMeg0xq/hDGiZJfVn0fV4llCmK" crossorigin="anonymous"></script>
<script>window.ui = SwaggerUIBundle({{ url: {spec}, dom_id: "#ui" }});</script>
</body>
</html>
"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn route(method: &str, path: &str, kind: RouteKind) -> RouteInfo {
        RouteInfo {
            method: method.into(),
            path: path.into(),
            kind,
        }
    }

    #[test]
    fn converts_paths_and_catch_alls() {
        assert_eq!(
            openapi_path("/users/{id}/posts/{post}"),
            (
                "/users/{id}/posts/{post}".into(),
                vec!["id".into(), "post".into()]
            )
        );
        assert_eq!(
            openapi_path("/files/{*path}"),
            ("/files/{path}".into(), vec!["path".into()])
        );
        assert_eq!(openapi_path("/plain"), ("/plain".into(), vec![]));
    }

    #[test]
    fn undocumented_routes_get_defaults_and_hidden_ones_are_skipped() {
        let routes = vec![
            route("GET", "/users/{id}", RouteKind::Http),
            route("POST", "/users", RouteKind::Http),
            route("GET", "/chat", RouteKind::WebSocket),
            route("GET", "/static/{*filepath}", RouteKind::Hidden),
        ];
        let doc = build(&OpenApiInfo::new("T", "1"), &routes, &HashMap::new());
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"], json!({ "title": "T", "version": "1" }));
        assert!(doc["paths"].get("/static/{filepath}").is_none());

        let get = &doc["paths"]["/users/{id}"]["get"];
        assert_eq!(
            get["parameters"][0],
            json!({ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } })
        );
        assert_eq!(get["responses"]["200"]["description"], "OK");
        assert!(doc["paths"]["/users"]["post"].is_object());
        assert_eq!(
            doc["paths"]["/chat"]["get"]["responses"]["101"]["description"],
            "Switching Protocols (WebSocket)"
        );
        assert!(doc["components"].get("securitySchemes").is_none());
    }

    #[test]
    fn documented_operation_is_rendered() {
        let routes = vec![route("GET", "/users/{id}", RouteKind::Http)];
        let mut docs = HashMap::new();
        docs.insert(
            ("GET".to_string(), "/users/{id}".to_string()),
            Operation::new("Fetch")
                .description("Longer text")
                .tag("users")
                .operation_id("getUser")
                .deprecated()
                .secured()
                .path_param("id", schema::integer(), "User id")
                .query_param("verbose", schema::boolean(), "", false)
                .response_json(
                    200,
                    "User",
                    schema::object([("id", schema::integer())], ["id"]),
                )
                .error_response(404, "Missing"),
        );
        let info = OpenApiInfo::new("T", "1")
            .description("d")
            .server("https://api.example.com");
        let doc = build(&info, &routes, &docs);

        let op = &doc["paths"]["/users/{id}"]["get"];
        assert_eq!(op["summary"], "Fetch");
        assert_eq!(op["tags"], json!(["users"]));
        assert_eq!(op["operationId"], "getUser");
        assert_eq!(op["deprecated"], true);
        assert_eq!(op["security"], json!([{ "bearerAuth": [] }]));
        // documented path param replaces the default one instead of duplicating it
        let params = op["parameters"].as_array().unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0]["schema"]["type"], "integer");
        assert_eq!(params[1]["name"], "verbose");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["required"],
            json!(["id"])
        );
        assert_eq!(
            op["responses"]["404"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Error"
        );
        assert!(op["responses"].get("default").is_none());
        assert_eq!(
            doc["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        assert_eq!(doc["servers"][0]["url"], "https://api.example.com");
        assert_eq!(doc["info"]["description"], "d");
    }

    #[test]
    fn request_bodies() {
        let routes = vec![
            route("POST", "/a", RouteKind::Http),
            route("POST", "/b", RouteKind::Http),
        ];
        let mut docs = HashMap::new();
        docs.insert(
            ("POST".into(), "/a".into()),
            Operation::new("a").request_json(schema::object([("n", schema::string())], [])),
        );
        docs.insert(
            ("POST".into(), "/b".into()),
            Operation::new("b").request_form(schema::object([("n", schema::string())], ["n"])),
        );
        let doc = build(&OpenApiInfo::new("T", "1"), &routes, &docs);
        assert!(
            doc["paths"]["/a"]["post"]["requestBody"]["content"]["application/json"].is_object()
        );
        assert!(doc["paths"]["/b"]["post"]["requestBody"]["content"]
            ["application/x-www-form-urlencoded"]
            .is_object());
    }

    #[test]
    fn swagger_page_escapes_inputs() {
        let html = swagger_ui_html("<T&C>", "/x</script><script>alert(1)");
        assert!(html.contains("&lt;T&amp;C&gt;"));
        assert!(!html.contains("</script><script>alert"));
        assert!(html.contains(r#"integrity="sha384-"#));
    }
}

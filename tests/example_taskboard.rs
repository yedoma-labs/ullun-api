#![cfg(feature = "jwt")]
//! The task board example, tested over real HTTP

mod common;

#[allow(dead_code)]
#[path = "../examples/taskboard/app.rs"]
mod taskboard;

use common::{spawn, Server};
use serde_json::{json, Value};
use std::time::Duration;

const SECRET: &[u8] = b"test-secret-test-secret-test-secret!";
const PASSWORD: &str = "correct horse battery";

fn config(auth_limit: u32) -> taskboard::Config {
    taskboard::Config {
        jwt_secret: SECRET.to_vec(),
        token_ttl_secs: 600,
        fast_hashing: true,
        auth_rate_limit: (auth_limit, Duration::from_secs(60)),
    }
}

async fn server() -> Server {
    spawn(taskboard::build(config(1000))).await
}

async fn register(s: &Server, email: &str) -> common::Reply {
    s.post("/auth/register")
        .json(&json!({ "email": email, "password": PASSWORD }))
        .send()
        .await
}

async fn login(s: &Server, email: &str) -> String {
    let res = s
        .post("/auth/login")
        .json(&json!({ "email": email, "password": PASSWORD }))
        .send()
        .await;
    assert_eq!(res.status, 200, "{}", res.text());
    res.json()["token"].as_str().unwrap().to_string()
}

/// A registered, logged-in user
async fn user(s: &Server, email: &str) -> String {
    assert_eq!(register(s, email).await.status, 201);
    login(s, email).await
}

async fn project(s: &Server, token: &str, name: &str) -> u64 {
    let res = s
        .post("/api/projects")
        .bearer(token)
        .json(&json!({ "name": name }))
        .send()
        .await;
    assert_eq!(res.status, 201, "{}", res.text());
    res.json()["id"].as_u64().unwrap()
}

async fn task(s: &Server, token: &str, project: u64, body: Value) -> Value {
    let res = s
        .post(&format!("/api/projects/{project}/tasks"))
        .bearer(token)
        .json(&body)
        .send()
        .await;
    assert_eq!(res.status, 201, "{}", res.text());
    res.json()
}

#[tokio::test]
async fn registration_validation_conflicts_and_normalisation() {
    let s = server().await;

    let bad = s
        .post("/auth/register")
        .json(&json!({ "email": "not-an-email", "password": "short" }))
        .send()
        .await;
    assert_eq!(bad.status, 422);
    let body = bad.json();
    assert_eq!(body["error"], "Validation failed");
    assert_eq!(body["fields"]["email"][0], "must be a valid e-mail address");
    assert_eq!(
        body["fields"]["password"][0],
        "must be 10 to 128 characters long"
    );

    let ok = register(&s, "  Ann@Example.COM ").await;
    assert_eq!(ok.status, 201);
    assert_eq!(ok.json()["email"], "ann@example.com");
    // the password hash never leaves the server
    assert!(!ok.text().contains("argon2"));

    assert_eq!(register(&s, "ann@example.com").await.status, 409);
    assert_eq!(register(&s, "ANN@example.com").await.status, 409);
    // e-mail casing does not matter when logging in either
    assert_eq!(login(&s, "Ann@Example.com").await.split('.').count(), 3);
}

#[tokio::test]
async fn login_failures_look_the_same() {
    let s = server().await;
    register(&s, "ann@example.com").await;

    let wrong_pw = s
        .post("/auth/login")
        .json(&json!({ "email": "ann@example.com", "password": "wrong password!" }))
        .send()
        .await;
    let unknown = s
        .post("/auth/login")
        .json(&json!({ "email": "nobody@example.com", "password": PASSWORD }))
        .send()
        .await;
    assert_eq!((wrong_pw.status, unknown.status), (401, 401));
    assert_eq!(wrong_pw.text(), unknown.text());

    let malformed = s
        .post("/auth/login")
        .json(&json!({ "email": "ann@example.com" }))
        .send()
        .await;
    assert_eq!(malformed.status, 400);
}

#[tokio::test]
async fn auth_endpoints_are_throttled_per_ip() {
    let s = spawn(taskboard::build(config(4))).await;
    for i in 0..4 {
        let res = s
            .post("/auth/login")
            .json(&json!({ "email": "x@example.com", "password": PASSWORD }))
            .send()
            .await;
        assert_eq!(res.status, 401, "attempt {i}");
    }
    let limited = s
        .post("/auth/login")
        .json(&json!({ "email": "x@example.com", "password": PASSWORD }))
        .send()
        .await;
    assert_eq!(limited.status, 429);
    assert!(limited.header("retry-after").parse::<u64>().unwrap() >= 1);
    // register shares the budget; the API itself is not throttled by it
    assert_eq!(register(&s, "y@example.com").await.status, 429);
    assert_eq!(s.get("/healthz").send().await.status, 200);
}

#[tokio::test]
async fn api_requires_a_valid_unexpired_token() {
    let s = server().await;
    let token = user(&s, "ann@example.com").await;

    assert_eq!(
        s.get("/api/me").bearer(&token).send().await.json()["email"],
        "ann@example.com"
    );
    assert_eq!(s.get("/api/me").send().await.status, 401);
    assert_eq!(s.get("/api/me").bearer("garbage").send().await.status, 401);

    let mut tampered = token.clone();
    tampered.push('x');
    assert_eq!(s.get("/api/me").bearer(&tampered).send().await.status, 401);

    let expired =
        ullun::jwt::encode(&json!({ "sub": 1, "exp": ullun::jwt::now() - 10 }), SECRET).unwrap();
    assert_eq!(s.get("/api/me").bearer(&expired).send().await.status, 401);
    let wrong_key = ullun::jwt::encode(
        &json!({ "sub": 1 }),
        b"another-secret-another-secret-another",
    )
    .unwrap();
    assert_eq!(s.get("/api/me").bearer(&wrong_key).send().await.status, 401);
    // valid signature but a subject that does not exist
    let ghost = ullun::jwt::encode(
        &json!({ "sub": 9999, "exp": ullun::jwt::expires_in(60) }),
        SECRET,
    )
    .unwrap();
    assert_eq!(s.get("/api/me").bearer(&ghost).send().await.status, 401);
    // a token without a numeric subject is no login
    let odd = ullun::jwt::encode(
        &json!({ "sub": "ann", "exp": ullun::jwt::expires_in(60) }),
        SECRET,
    )
    .unwrap();
    assert_eq!(s.get("/api/me").bearer(&odd).send().await.status, 401);
}

#[tokio::test]
async fn projects_are_private_to_their_owner() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    let bob = user(&s, "bob@example.com").await;

    let p = project(&s, &ann, "  Launch  ").await;
    assert_eq!(
        s.get(&format!("/api/projects/{p}"))
            .bearer(&ann)
            .send()
            .await
            .json()["name"],
        "Launch"
    );
    task(&s, &ann, p, json!({ "title": "secret plan" })).await;

    // Bob sees nothing of it, and cannot touch it
    assert_eq!(
        s.get("/api/projects").bearer(&bob).send().await.json()["items"],
        json!([])
    );
    for (method, path) in [
        ("GET", format!("/api/projects/{p}")),
        ("DELETE", format!("/api/projects/{p}")),
        ("GET", format!("/api/projects/{p}/tasks")),
    ] {
        assert_eq!(
            s.request(method, &path).bearer(&bob).send().await.status,
            404,
            "{method} {path}"
        );
    }
    let intruder = s
        .post(&format!("/api/projects/{p}/tasks"))
        .bearer(&bob)
        .json(&json!({ "title": "x" }))
        .send()
        .await;
    assert_eq!(intruder.status, 404);

    // Ann's list shows her open task count
    let list = s.get("/api/projects").bearer(&ann).send().await.json();
    assert_eq!(list["items"][0]["open_tasks"], 1);

    for bad in [
        json!({ "name": "" }),
        json!({ "name": "   " }),
        json!({ "name": "x".repeat(81) }),
    ] {
        let res = s.post("/api/projects").bearer(&ann).json(&bad).send().await;
        assert_eq!(res.status, 422, "{bad}");
        assert!(res.json()["fields"]["name"].is_array());
    }
}

#[tokio::test]
async fn task_ownership_is_enforced_by_task_id_too() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    let bob = user(&s, "bob@example.com").await;
    let p = project(&s, &ann, "P").await;
    let t = task(&s, &ann, p, json!({ "title": "mine" })).await;
    let (id, etag) = (t["id"].as_u64().unwrap(), "\"v1\"");

    assert_eq!(
        s.get(&format!("/api/tasks/{id}"))
            .bearer(&bob)
            .send()
            .await
            .status,
        404
    );
    let patch = s
        .patch(&format!("/api/tasks/{id}"))
        .bearer(&bob)
        .header("if-match", etag)
        .json(&json!({ "title": "pwned" }))
        .send()
        .await;
    assert_eq!(patch.status, 404);
    assert_eq!(
        s.delete(&format!("/api/tasks/{id}"))
            .bearer(&bob)
            .send()
            .await
            .status,
        404
    );
    assert_eq!(
        s.get(&format!("/api/tasks/{id}"))
            .bearer(&ann)
            .send()
            .await
            .json()["title"],
        "mine"
    );
}

#[tokio::test]
async fn task_creation_reports_every_invalid_field() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    let p = project(&s, &ann, "P").await;
    let url = format!("/api/projects/{p}/tasks");

    let res = s
        .post(&url)
        .bearer(&ann)
        .json(&json!({ "title": "", "description": "d".repeat(2001), "priority": 9, "due": "2026-02-30" }))
        .send()
        .await;
    assert_eq!(res.status, 422);
    let fields = &res.json()["fields"];
    for f in ["title", "description", "priority", "due"] {
        assert!(fields[f].is_array(), "{f}: {fields}");
    }

    for due in [
        "2026-13-01",
        "2026-1-1",
        "26-01-01",
        "2026/01/01",
        "tomorrow",
        "2025-02-29",
        "2026-04-31",
        "",
    ] {
        let res = s
            .post(&url)
            .bearer(&ann)
            .json(&json!({ "title": "t", "due": due }))
            .send()
            .await;
        assert_eq!(res.status, 422, "due {due:?}");
    }
    for due in ["2028-02-29", "2026-12-31", "2000-02-29"] {
        let res = s
            .post(&url)
            .bearer(&ann)
            .json(&json!({ "title": "t", "due": due }))
            .send()
            .await;
        assert_eq!(res.status, 201, "due {due:?}");
    }
    // wrong JSON types are 400 (malformed), not 422
    let res = s
        .post(&url)
        .bearer(&ann)
        .json(&json!({ "title": 5 }))
        .send()
        .await;
    assert_eq!(res.status, 400);

    let created = s
        .post(&url)
        .bearer(&ann)
        .json(&json!({ "title": "  Ship it  " }))
        .send()
        .await;
    assert_eq!(created.status, 201);
    assert_eq!(created.header("etag"), "\"v1\"");
    let body = created.json();
    assert_eq!(
        (
            body["title"].as_str(),
            body["status"].as_str(),
            body["priority"].as_u64()
        ),
        (Some("Ship it"), Some("open"), Some(1))
    );
    assert_eq!(
        created.header("location"),
        format!("/api/tasks/{}", body["id"])
    );
}

#[tokio::test]
async fn listing_filters_sorts_and_paginates() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    let p = project(&s, &ann, "P").await;
    let url = format!("/api/projects/{p}/tasks");
    // title, priority, due
    for (title, prio, due) in [
        ("write docs", 1, Some("2026-03-01")),
        ("fix bug", 3, Some("2026-01-15")),
        ("plan release", 2, None),
        ("Fix typo", 0, Some("2026-02-01")),
        ("review", 3, None),
    ] {
        let mut body = json!({ "title": title, "priority": prio });
        if let Some(d) = due {
            body["due"] = json!(d);
        }
        task(&s, &ann, p, body).await;
    }
    // close one
    let first = s
        .get(&format!("{url}?sort=created"))
        .bearer(&ann)
        .send()
        .await
        .json();
    let first_id = first["items"][0]["id"].as_u64().unwrap();
    let patched = s
        .patch(&format!("/api/tasks/{first_id}"))
        .bearer(&ann)
        .header("if-match", "\"v1\"")
        .json(&json!({ "status": "done" }))
        .send()
        .await;
    assert_eq!(patched.status, 200);

    let titles = |v: &Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap().to_string())
            .collect()
    };
    let list = |q: &str| {
        let (s, ann, url) = (s, ann.clone(), url.clone());
        let q = q.to_string();
        async move { s.get(&format!("{url}{q}")).bearer(&ann).send().await }
    };

    assert_eq!(
        titles(&list("").await.json()),
        [
            "write docs",
            "fix bug",
            "plan release",
            "Fix typo",
            "review"
        ]
    );
    assert_eq!(titles(&list("?order=desc").await.json())[0], "review");
    assert_eq!(
        titles(&list("?sort=due").await.json()),
        [
            "fix bug",
            "Fix typo",
            "write docs",
            "plan release",
            "review"
        ]
    );
    assert_eq!(
        titles(&list("?sort=priority&order=desc").await.json())[..2],
        ["fix bug", "review"]
    );
    assert_eq!(titles(&list("?status=done").await.json()), ["write docs"]);
    assert_eq!(list("?status=open").await.json()["total"], 4);
    assert_eq!(
        titles(&list("?priority=3").await.json()),
        ["fix bug", "review"]
    );
    assert_eq!(
        titles(&list("?q=FIX").await.json()),
        ["fix bug", "Fix typo"]
    );
    assert_eq!(
        titles(&list("?q=fix&priority=0").await.json()),
        ["Fix typo"]
    );

    let page2 = list("?per_page=2&page=2").await.json();
    assert_eq!(
        (
            page2["page"].as_u64(),
            page2["per_page"].as_u64(),
            page2["total"].as_u64()
        ),
        (Some(2), Some(2), Some(5))
    );
    assert_eq!(titles(&page2), ["plan release", "Fix typo"]);
    assert_eq!(
        titles(&list("?per_page=2&page=9").await.json()),
        Vec::<String>::new()
    );
    assert_eq!(list("?per_page=1000").await.json()["per_page"], 100);

    for bad in ["?status=bogus", "?sort=name", "?order=up"] {
        let res = list(bad).await;
        assert_eq!(res.status, 422, "{bad}");
    }
    assert_eq!(list("?priority=high").await.status, 400);
}

#[tokio::test]
async fn patch_needs_if_match_and_detects_conflicts() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    let p = project(&s, &ann, "P").await;
    let t = task(
        &s,
        &ann,
        p,
        json!({ "title": "draft", "description": "text", "due": "2026-05-05" }),
    )
    .await;
    let path = format!("/api/tasks/{}", t["id"]);

    let patch = |tag: Option<&str>, body: Value| {
        let (s, ann, path) = (s, ann.clone(), path.clone());
        let tag = tag.map(str::to_string);
        async move {
            let mut call = s.patch(&path).bearer(&ann);
            if let Some(tag) = tag {
                call = call.header("if-match", &tag);
            }
            call.json(&body).send().await
        }
    };

    assert_eq!(patch(None, json!({ "title": "x" })).await.status, 428);
    assert_eq!(
        patch(Some("\"v7\""), json!({ "title": "x" })).await.status,
        412
    );
    assert_eq!(
        patch(Some("W/\"v1\""), json!({ "title": "x" }))
            .await
            .status,
        412
    );

    let ok = patch(
        Some("\"v1\""),
        json!({ "title": "final", "status": "in_progress", "priority": 2 }),
    )
    .await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.header("etag"), "\"v2\"");
    let body = ok.json();
    assert_eq!(
        (
            body["title"].as_str(),
            body["status"].as_str(),
            body["priority"].as_u64(),
            body["version"].as_u64()
        ),
        (Some("final"), Some("in_progress"), Some(2), Some(2))
    );
    // untouched fields stay
    assert_eq!(
        (body["description"].as_str(), body["due"].as_str()),
        (Some("text"), Some("2026-05-05"))
    );

    // a concurrent writer holding v1 loses instead of overwriting
    assert_eq!(
        patch(Some("\"v1\""), json!({ "title": "lost update" }))
            .await
            .status,
        412
    );
    assert_eq!(
        s.get(&path).bearer(&ann).send().await.json()["title"],
        "final"
    );

    // null clears optional fields
    let cleared = patch(Some("\"v2\""), json!({ "description": null, "due": null })).await;
    let body = cleared.json();
    assert_eq!(
        (
            body["description"].is_null(),
            body["due"].is_null(),
            body["version"].as_u64()
        ),
        (true, true, Some(3))
    );
    // * matches any version
    assert_eq!(
        patch(Some("*"), json!({ "title": "forced" })).await.status,
        200
    );
}

#[tokio::test]
async fn invalid_patches_change_nothing() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    let p = project(&s, &ann, "P").await;
    let t = task(&s, &ann, p, json!({ "title": "keep me" })).await;
    let path = format!("/api/tasks/{}", t["id"]);

    let res = s
        .patch(&path)
        .bearer(&ann)
        .header("if-match", "\"v1\"")
        .json(&json!({ "title": "ok but", "status": "archived", "priority": 7, "due": "soon", "colour": "red", "description": 5 }))
        .send()
        .await;
    assert_eq!(res.status, 422);
    let fields = &res.json()["fields"];
    for f in ["status", "priority", "due", "colour", "description"] {
        assert!(fields[f].is_array(), "{f}: {fields}");
    }
    assert!(fields.get("title").is_none());

    let after = s.get(&path).bearer(&ann).send().await;
    assert_eq!(after.header("etag"), "\"v1\"");
    assert_eq!(after.json()["title"], "keep me");

    // a PATCH body that is not an object
    let arr = s
        .patch(&path)
        .bearer(&ann)
        .header("if-match", "\"v1\"")
        .json(&json!([1]))
        .send()
        .await;
    assert_eq!(arr.status, 400);
}

#[tokio::test]
async fn conditional_get_returns_304() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    let p = project(&s, &ann, "P").await;
    let t = task(&s, &ann, p, json!({ "title": "t" })).await;
    let path = format!("/api/tasks/{}", t["id"]);

    let first = s.get(&path).bearer(&ann).send().await;
    let tag = first.header("etag").to_string();
    assert_eq!(tag, "\"v1\"");
    let cached = s
        .get(&path)
        .bearer(&ann)
        .header("if-none-match", &tag)
        .send()
        .await;
    assert_eq!(cached.status, 304);
    assert!(cached.body.is_empty());

    s.patch(&path)
        .bearer(&ann)
        .header("if-match", &tag)
        .json(&json!({ "title": "t2" }))
        .send()
        .await;
    let changed = s
        .get(&path)
        .bearer(&ann)
        .header("if-none-match", &tag)
        .send()
        .await;
    assert_eq!((changed.status, changed.header("etag")), (200, "\"v2\""));

    // lists get a content-derived tag
    let list = s.get("/api/projects").bearer(&ann).send().await;
    let list_tag = list.header("etag").to_string();
    assert!(list_tag.starts_with('"'));
    assert_eq!(
        s.get("/api/projects")
            .bearer(&ann)
            .header("if-none-match", &list_tag)
            .send()
            .await
            .status,
        304
    );
}

#[tokio::test]
async fn deleting_tasks_and_projects() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    let p = project(&s, &ann, "P").await;
    let t1 = task(&s, &ann, p, json!({ "title": "one" })).await["id"]
        .as_u64()
        .unwrap();
    let t2 = task(&s, &ann, p, json!({ "title": "two" })).await["id"]
        .as_u64()
        .unwrap();

    // a stale If-Match protects the task, a missing one does not
    let stale = s
        .delete(&format!("/api/tasks/{t1}"))
        .bearer(&ann)
        .header("if-match", "\"v9\"")
        .send()
        .await;
    assert_eq!(stale.status, 412);
    assert_eq!(
        s.delete(&format!("/api/tasks/{t1}"))
            .bearer(&ann)
            .header("if-match", "\"v1\"")
            .send()
            .await
            .status,
        204
    );
    assert_eq!(
        s.get(&format!("/api/tasks/{t1}"))
            .bearer(&ann)
            .send()
            .await
            .status,
        404
    );
    assert_eq!(
        s.delete(&format!("/api/tasks/{t1}"))
            .bearer(&ann)
            .send()
            .await
            .status,
        404
    );

    // deleting the project removes the remaining tasks
    assert_eq!(
        s.delete(&format!("/api/projects/{p}"))
            .bearer(&ann)
            .send()
            .await
            .status,
        204
    );
    assert_eq!(
        s.get(&format!("/api/tasks/{t2}"))
            .bearer(&ann)
            .send()
            .await
            .status,
        404
    );
    assert_eq!(
        s.get(&format!("/api/projects/{p}"))
            .bearer(&ann)
            .send()
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn path_ids_are_validated() {
    let s = server().await;
    let ann = user(&s, "ann@example.com").await;
    for path in [
        "/api/projects/abc",
        "/api/projects/-1",
        "/api/tasks/1.5",
        "/api/projects/99999999999999999999",
    ] {
        let res = s.get(path).bearer(&ann).send().await;
        assert_eq!(res.status, 400, "{path}");
        assert!(res.text().contains("Invalid path parameter 'id'"));
    }
    assert_eq!(
        s.get("/api/projects/424242")
            .bearer(&ann)
            .send()
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn hardening_headers_and_body_limit() {
    let s = server().await;
    let res = s.get("/healthz").send().await;
    assert_eq!(res.header("x-content-type-options"), "nosniff");
    assert_eq!(res.header("cache-control"), "no-store");
    // headers are present on errors as well
    let denied = s.get("/api/me").send().await;
    assert_eq!(
        (denied.status, denied.header("cache-control")),
        (401, "no-store")
    );

    let big = s
        .post("/auth/register")
        .header("content-type", "application/json")
        .body(vec![b'x'; 70 * 1024])
        .send()
        .await;
    assert_eq!(big.status, 413);
}

#[tokio::test]
async fn openapi_documents_security_and_conditional_requests() {
    let s = server().await;
    let doc = s.get("/openapi.json").send().await.json();
    assert_eq!(doc["info"]["title"], "Task board");
    for path in [
        "/auth/login",
        "/auth/register",
        "/api/me",
        "/api/projects",
        "/api/projects/{id}/tasks",
        "/api/tasks/{id}",
    ] {
        assert!(doc["paths"][path].is_object(), "{path}");
    }
    assert_eq!(
        doc["paths"]["/api/tasks/{id}"]["patch"]["security"],
        json!([{ "bearerAuth": [] }])
    );
    for code in ["412", "428", "422"] {
        assert!(
            doc["paths"]["/api/tasks/{id}"]["patch"]["responses"][code].is_object(),
            "{code}"
        );
    }
    assert!(doc["paths"]["/auth/login"]["post"]
        .get("security")
        .is_none());
    assert_eq!(s.get("/docs").send().await.status, 200);
}

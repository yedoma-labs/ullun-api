use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use std::sync::Arc;
use ullun::prelude::*;

async fn dummy_handler(_req: Request) -> Result<Response> {
    Ok(Response::text("OK"))
}

fn benchmark_router_insert(c: &mut Criterion) {
    c.bench_function("router_insert_10_routes", |b| {
        b.iter(|| {
            let mut router = ullun::router::Router::new();
            for i in 0..10 {
                let path = format!("/route{}", i);
                router
                    .insert(
                        "GET",
                        &path,
                        Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>,
                    )
                    .unwrap();
            }
            black_box(router);
        });
    });

    c.bench_function("router_insert_100_routes", |b| {
        b.iter(|| {
            let mut router = ullun::router::Router::new();
            for i in 0..100 {
                let path = format!("/route{}", i);
                router
                    .insert(
                        "GET",
                        &path,
                        Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>,
                    )
                    .unwrap();
            }
            black_box(router);
        });
    });
}

fn benchmark_router_match(c: &mut Criterion) {
    let mut router = ullun::router::Router::new();
    for i in 0..100 {
        let path = format!("/route{}", i);
        router
            .insert(
                "GET",
                &path,
                Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>,
            )
            .unwrap();
    }
    router
        .insert(
            "GET",
            "/users/{id}",
            Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>,
        )
        .unwrap();
    router
        .insert(
            "GET",
            "/posts/{id}/comments/{comment_id}",
            Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>,
        )
        .unwrap();

    c.bench_function("router_match_static", |b| {
        b.iter(|| {
            let result = router.match_route("GET", "/route50");
            let _ = black_box(result);
        });
    });

    c.bench_function("router_match_one_param", |b| {
        b.iter(|| {
            let result = router.match_route("GET", "/users/123");
            let _ = black_box(result);
        });
    });

    c.bench_function("router_match_multi_param", |b| {
        b.iter(|| {
            let result = router.match_route("GET", "/posts/456/comments/789");
            let _ = black_box(result);
        });
    });
}

fn benchmark_json_serialization(c: &mut Criterion) {
    use serde_json::json;

    c.bench_function("json_serialize_small", |b| {
        b.iter(|| {
            let response = Response::json(json!({
                "status": "ok",
                "code": 200
            }));
            black_box(response);
        });
    });

    c.bench_function("json_serialize_medium", |b| {
        b.iter(|| {
            let response = Response::json(json!({
                "users": vec![
                    json!({"id": 1, "name": "Alice", "email": "alice@example.com"}),
                    json!({"id": 2, "name": "Bob", "email": "bob@example.com"}),
                    json!({"id": 3, "name": "Charlie", "email": "charlie@example.com"}),
                ]
            }));
            black_box(response);
        });
    });
}

fn request_with(uri: &str, headers: &[(&str, &str)], body: &'static str) -> Request {
    let mut map = http::HeaderMap::new();
    for (k, v) in headers {
        map.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            http::HeaderValue::from_str(v).unwrap(),
        );
    }
    Request::new(
        http::Method::POST,
        uri.parse().unwrap(),
        map,
        bytes::Bytes::from_static(body.as_bytes()),
    )
}

fn benchmark_request_parsing(c: &mut Criterion) {
    #[derive(Deserialize)]
    struct NewUser {
        #[allow(dead_code)]
        name: String,
        #[allow(dead_code)]
        email: String,
    }

    c.bench_function("request_new_with_query", |b| {
        b.iter(|| {
            black_box(request_with(
                "/search?q=rust+web&page=2&per_page=20&sort=desc",
                &[],
                "",
            ))
        });
    });

    let req = request_with(
        "/users",
        &[("content-type", "application/json")],
        r#"{"name":"Alice","email":"alice@example.com"}"#,
    );
    c.bench_function("request_json_body", |b| {
        b.iter(|| black_box(req.json::<NewUser>().unwrap()));
    });

    c.bench_function("request_cookies_parse", |b| {
        b.iter(|| {
            let req = request_with(
                "/",
                &[(
                    "cookie",
                    "session=abc123; theme=dark; lang=de; tracking=off",
                )],
                "",
            );
            black_box(req.cookies().get("theme").map(str::len))
        });
    });
}

fn benchmark_response(c: &mut Criterion) {
    c.bench_function("response_text", |b| {
        b.iter(|| black_box(Response::text("Hello, World!")));
    });

    c.bench_function("response_json_with_cookie", |b| {
        b.iter(|| {
            black_box(
                Response::json(serde_json::json!({ "ok": true }))
                    .cookie(Cookie::new("session", "abc123").http_only().max_age(3600)),
            )
        });
    });

    c.bench_function("response_from_error", |b| {
        b.iter(|| black_box(Response::from_error(Error::not_found("missing"))));
    });
}

fn benchmark_middleware_chain(c: &mut Criterion) {
    use ullun::middleware::{Middleware, Next};

    async fn pass(req: Request, next: Next) -> Result<Response> {
        next.run(req).await
    }
    async fn end(_req: Request, _next: Next) -> Result<Response> {
        Ok(Response::text("OK"))
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    for depth in [0usize, 1, 3, 10] {
        let mut chain: Vec<Arc<dyn Middleware>> = Vec::new();
        for _ in 0..depth {
            chain.push(Arc::new(pass));
        }
        chain.push(Arc::new(end));
        let chain = Arc::new(chain);

        c.bench_function(&format!("middleware_chain_{depth}_layers"), |b| {
            b.to_async(&rt).iter(|| async {
                let req = request_with("/", &[], "");
                black_box(Next::new(chain.clone()).run(req).await)
            });
        });
    }
}

criterion_group!(
    benches,
    benchmark_router_insert,
    benchmark_router_match,
    benchmark_json_serialization,
    benchmark_request_parsing,
    benchmark_response,
    benchmark_middleware_chain
);
criterion_main!(benches);

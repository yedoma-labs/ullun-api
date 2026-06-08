use criterion::{black_box, criterion_group, criterion_main, Criterion};
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
                    .insert("GET", &path, Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>)
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
                    .insert("GET", &path, Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>)
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
            .insert("GET", &path, Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>)
            .unwrap();
    }
    router.insert("GET", "/users/{id}", Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>).unwrap();
    router.insert("GET", "/posts/{id}/comments/{comment_id}", Arc::new(dummy_handler) as Arc<dyn ullun::handler::Handler>).unwrap();

    c.bench_function("router_match_static", |b| {
        b.iter(|| {
            let result = router.match_route("GET", "/route50");
            black_box(result);
        });
    });

    c.bench_function("router_match_one_param", |b| {
        b.iter(|| {
            let result = router.match_route("GET", "/users/123");
            black_box(result);
        });
    });

    c.bench_function("router_match_multi_param", |b| {
        b.iter(|| {
            let result = router.match_route("GET", "/posts/456/comments/789");
            black_box(result);
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

criterion_group!(
    benches,
    benchmark_router_insert,
    benchmark_router_match,
    benchmark_json_serialization
);
criterion_main!(benches);

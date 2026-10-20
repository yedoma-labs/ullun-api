//! One server, one scenario, one framework: `bench-server <ullun|axum|actix> <scenario> <port> [threads]`
//!
//! Every framework serves exactly the same responses with default settings, a
//! release build, the same worker thread count, and no logging.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

const MESSAGE: &str = "Hello, World!";

#[derive(Deserialize)]
struct Person {
    name: String,
    age: u32,
    tags: Vec<String>,
}

#[derive(Serialize)]
struct Echo<'a> {
    ok: bool,
    name: &'a str,
    age: u32,
    tags: usize,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [framework, scenario, port] = &args[..3.min(args.len())] else {
        eprintln!("usage: bench-server <ullun|axum|actix> <hello|json|param|post|middleware> <port> [threads]");
        std::process::exit(2);
    };
    let port: u16 = port.parse().expect("port");
    let threads: usize = args
        .get(3)
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();

    match framework.as_str() {
        "ullun" => ullun_server(scenario, addr, threads),
        "axum" => axum_server(scenario, addr, threads),
        "actix" => actix_server(scenario, addr, threads),
        other => {
            eprintln!("unknown framework {other}");
            std::process::exit(2);
        }
    }
}

fn runtime(threads: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .expect("runtime")
}

// ---- ullun ----

fn ullun_server(scenario: &str, addr: SocketAddr, threads: usize) {
    use ullun::prelude::*;

    let mut app = App::new();
    app = match scenario {
        "hello" | "middleware" => app.get("/", |_req: Request| async { Ok(Response::text(MESSAGE)) }),
        "json" => app.get("/json", |_req: Request| async {
            Ok(Response::json(serde_json::json!({ "message": MESSAGE })))
        }),
        "param" => app.get_with_params("/users/{id}", |p: Params| async move {
            let id: u64 = p.parse("id")?;
            Ok(Response::json(serde_json::json!({ "id": id, "name": format!("user-{id}") })))
        }),
        "post" => app.post("/echo", |req: Request| async move {
            let p: Person = req.json()?;
            Ok(Response::json(Echo { ok: true, name: &p.name, age: p.age, tags: p.tags.len() }))
        }),
        other => panic!("unknown scenario {other}"),
    };
    if scenario == "middleware" {
        for name in ["x-a", "x-b", "x-c"] {
            app = app.middleware(move |req: Request, next: Next| async move {
                Ok(next.run(req).await?.with_header(name, "1"))
            });
        }
    }
    runtime(threads).block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        app.serve(listener).await.expect("serve");
    });
}

// ---- axum ----

fn axum_server(scenario: &str, addr: SocketAddr, threads: usize) {
    use axum::extract::Path;
    use axum::http::HeaderValue;
    use axum::middleware::{self, Next};
    use axum::response::{IntoResponse, Json, Response};
    use axum::routing::{get, post};
    use axum::Router;

    async fn add_header(name: &'static str, req: axum::extract::Request, next: Next) -> Response {
        let mut res = next.run(req).await;
        res.headers_mut().insert(name, HeaderValue::from_static("1"));
        res
    }

    let mut app = match scenario {
        "hello" | "middleware" => Router::new().route("/", get(|| async { MESSAGE })),
        "json" => Router::new().route("/json", get(|| async { Json(serde_json::json!({ "message": MESSAGE })) })),
        "param" => Router::new().route(
            "/users/{id}",
            get(|Path(id): Path<u64>| async move { Json(serde_json::json!({ "id": id, "name": format!("user-{id}") })) }),
        ),
        "post" => Router::new().route(
            "/echo",
            post(|Json(p): Json<Person>| async move {
                Json(Echo { ok: true, name: &p.name, age: p.age, tags: p.tags.len() }).into_response()
            }),
        ),
        other => panic!("unknown scenario {other}"),
    };
    if scenario == "middleware" {
        for name in ["x-a", "x-b", "x-c"] {
            app = app.layer(middleware::from_fn(move |req, next| add_header(name, req, next)));
        }
    }
    runtime(threads).block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        axum::serve(listener, app).await.expect("serve");
    });
}

// ---- actix-web ----

fn actix_server(scenario: &str, addr: SocketAddr, threads: usize) {
    use actix_web::dev::Service;
    use actix_web::http::header::{HeaderName, HeaderValue};
    use actix_web::{web, App, HttpResponse, HttpServer};

    macro_rules! run {
        ($factory:expr) => {
            actix_web::rt::System::new().block_on(async move {
                HttpServer::new($factory)
                    .workers(threads)
                    .bind(addr)
                    .expect("bind")
                    .run()
                    .await
                    .expect("serve");
            })
        };
    }
    // a middleware layer that adds one response header
    macro_rules! layer {
        ($name:literal) => {
            |req, srv| {
                let fut = srv.call(req);
                async move {
                    let mut res = fut.await?;
                    res.headers_mut().insert(HeaderName::from_static($name), HeaderValue::from_static("1"));
                    Ok(res)
                }
            }
        };
    }
    let hello = || web::get().to(|| async { HttpResponse::Ok().content_type("text/plain; charset=utf-8").body(MESSAGE) });

    match scenario {
        "hello" => run!(move || App::new().route("/", hello())),
        "middleware" => run!(move || App::new()
            .route("/", hello())
            .wrap_fn(layer!("x-a"))
            .wrap_fn(layer!("x-b"))
            .wrap_fn(layer!("x-c"))),
        "json" => run!(|| App::new().route(
            "/json",
            web::get().to(|| async { HttpResponse::Ok().json(serde_json::json!({ "message": MESSAGE })) })
        )),
        "param" => run!(|| App::new().route(
            "/users/{id}",
            web::get().to(|id: web::Path<u64>| async move {
                let id = id.into_inner();
                HttpResponse::Ok().json(serde_json::json!({ "id": id, "name": format!("user-{id}") }))
            })
        )),
        "post" => run!(|| App::new().route(
            "/echo",
            web::post().to(|p: web::Json<Person>| async move {
                HttpResponse::Ok().json(Echo { ok: true, name: &p.name, age: p.age, tags: p.tags.len() })
            })
        )),
        other => panic!("unknown scenario {other}"),
    }
}

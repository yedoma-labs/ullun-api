//! End-to-end HTTP benchmarks: real TCP, real hyper client, keep-alive.
//!
//! Each benchmark starts an in-process server on an ephemeral port
//! (`App::serve`) and measures full request/response round trips over
//! loopback, so numbers include parsing, routing, middleware, handler and
//! serialization, but also kernel/loopback overhead and the client itself.
//!
//! Run: cargo bench --bench http_benchmark

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use std::hint::black_box;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use ullun::prelude::*;

const CONCURRENCY: usize = 32;
const REQUESTS_PER_CONN: usize = 50;

async fn pass_through(req: Request, next: Next) -> Result<Response> {
    next.run(req).await
}

async fn add_header(req: Request, next: Next) -> Result<Response> {
    Ok(next.run(req).await?.with_header("x-bench", "1"))
}

#[derive(Deserialize, Serialize)]
struct NewUser {
    name: String,
    email: String,
}

fn app() -> App {
    App::new()
        .get("/hello", |_req: Request| async {
            Ok(Response::text("Hello, World!"))
        })
        .get_with_params("/users/{id}", |params: Params| async move {
            let id: u64 = params
                .get("id")?
                .parse()
                .map_err(|_| Error::bad_request("bad id"))?;
            Ok(Response::json(
                serde_json::json!({ "id": id, "name": "Alice" }),
            ))
        })
        .post("/users", |req: Request| async move {
            let user: NewUser = req.json()?;
            Ok(Response::json(user))
        })
}

fn app_with_middleware() -> App {
    app()
        .middleware(pass_through)
        .middleware(add_header)
        .middleware(pass_through)
}

async fn start(app: App) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

async fn connect(addr: SocketAddr) -> SendRequest<Full<Bytes>> {
    let stream = TcpStream::connect(addr).await.unwrap();
    stream.set_nodelay(true).unwrap();
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    sender
}

async fn roundtrip(
    sender: &mut SendRequest<Full<Bytes>>,
    method: &str,
    uri: &str,
    body: &'static str,
) -> usize {
    let req = hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "bench")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .unwrap();
    sender.ready().await.unwrap();
    let res = sender.send_request(req).await.unwrap();
    res.into_body().collect().await.unwrap().to_bytes().len()
}

/// Sequential round trips on one keep-alive connection = per-request latency.
fn bench_latency(c: &mut Criterion, rt: &Runtime) {
    let plain = rt.block_on(start(app()));
    let layered = rt.block_on(start(app_with_middleware()));

    let mut group = c.benchmark_group("http_latency");
    group.sample_size(50);

    let cases: [(&str, SocketAddr, &str, &str, &'static str); 6] = [
        ("hello_text", plain, "GET", "/hello", ""),
        ("path_param_json", plain, "GET", "/users/42", ""),
        (
            "post_json_echo",
            plain,
            "POST",
            "/users",
            r#"{"name":"Alice","email":"alice@example.com"}"#,
        ),
        ("not_found", plain, "GET", "/nope", ""),
        ("hello_3_middlewares", layered, "GET", "/hello", ""),
        (
            "post_json_3_middlewares",
            layered,
            "POST",
            "/users",
            r#"{"name":"Alice","email":"alice@example.com"}"#,
        ),
    ];

    for (name, addr, method, uri, body) in cases {
        // one connection per case; the (uncontended) lock lets the iter closure borrow it
        let sender = tokio::sync::Mutex::new(rt.block_on(connect(addr)));
        group.bench_function(name, |b| {
            b.to_async(rt).iter(|| async {
                let mut sender = sender.lock().await;
                black_box(roundtrip(&mut sender, method, uri, body).await)
            });
        });
    }
    group.finish();
}

/// Many keep-alive connections in parallel = throughput under concurrency.
fn bench_throughput(c: &mut Criterion, rt: &Runtime) {
    let addr = rt.block_on(start(app()));
    let senders = rt.block_on(async {
        let mut v = Vec::new();
        for _ in 0..CONCURRENCY {
            v.push(connect(addr).await);
        }
        v
    });
    let senders = tokio::sync::Mutex::new(senders);

    let mut group = c.benchmark_group("http_throughput");
    group.sample_size(20);
    group.throughput(Throughput::Elements(
        (CONCURRENCY * REQUESTS_PER_CONN) as u64,
    ));
    group.bench_function("hello_32_connections", |b| {
        b.to_async(rt).iter(|| async {
            // take the connections out for this iteration, put them back after
            let conns = std::mem::take(&mut *senders.lock().await);
            let tasks: Vec<_> = conns
                .into_iter()
                .map(|mut sender| {
                    tokio::spawn(async move {
                        for _ in 0..REQUESTS_PER_CONN {
                            black_box(roundtrip(&mut sender, "GET", "/hello", "").await);
                        }
                        sender
                    })
                })
                .collect();
            let mut back = Vec::with_capacity(tasks.len());
            for t in tasks {
                back.push(t.await.unwrap());
            }
            *senders.lock().await = back;
        });
    });
    group.finish();
}

fn benches(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    bench_latency(c, &rt);
    bench_throughput(c, &rt);
}

criterion_group!(http, benches);
criterion_main!(http);

#![cfg(feature = "redis")]
//! Redis client and stores against an in-process fake that speaks RESP.
//! Set `ULLUN_TEST_REDIS=redis://host:port` to run the same tests against a real server.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use ullun::idempotency::{Begin, Idempotency, IdempotencyStore, StoredResponse};
use ullun::prelude::*;
use ullun::rate_limit::{RateLimitStore, RateLimiter};
use ullun::redis::{
    Cmd, Redis, RedisIdempotencyStore, RedisRateLimitStore, RedisSessionStore, Value,
};
use ullun::session::{SessionLayer, SessionRecord, SessionStore};

#[derive(Default)]
struct Db {
    data: HashMap<Vec<u8>, (Vec<u8>, Option<Instant>)>,
    commands: usize,
}

impl Db {
    fn live(&mut self, key: &[u8]) -> Option<&mut (Vec<u8>, Option<Instant>)> {
        if let Some((_, Some(at))) = self.data.get(key) {
            if *at <= Instant::now() {
                self.data.remove(key);
            }
        }
        self.data.get_mut(key)
    }
}

struct Fake {
    url: String,
    db: Arc<Mutex<Db>>,
    task: tokio::task::JoinHandle<()>,
}

impl Fake {
    fn commands(&self) -> usize {
        self.db.lock().unwrap().commands
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_command(r: &mut BufReader<TcpStream>) -> Option<Vec<Vec<u8>>> {
    let mut line = String::new();
    if r.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let n: usize = line.trim().strip_prefix('*')?.parse().ok()?;
    let mut args = Vec::new();
    for _ in 0..n {
        line.clear();
        r.read_line(&mut line).await.ok()?;
        let len: usize = line.trim().strip_prefix('$')?.parse().ok()?;
        let mut buf = vec![0u8; len + 2];
        r.read_exact(&mut buf).await.ok()?;
        buf.truncate(len);
        args.push(buf);
    }
    Some(args)
}

fn bulk(v: &[u8]) -> Vec<u8> {
    [format!("${}\r\n", v.len()).as_bytes(), v, b"\r\n"].concat()
}

fn execute(db: &mut Db, args: &[Vec<u8>], password: &Option<String>, authed: &mut bool) -> Vec<u8> {
    db.commands += 1;
    let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
    if name == "AUTH" {
        let given = String::from_utf8_lossy(args.last().unwrap()).into_owned();
        return if password.as_deref() == Some(given.as_str()) {
            *authed = true;
            b"+OK\r\n".to_vec()
        } else {
            b"-WRONGPASS invalid password\r\n".to_vec()
        };
    }
    if password.is_some() && !*authed {
        return b"-NOAUTH Authentication required.\r\n".to_vec();
    }
    let key = args.get(1).cloned().unwrap_or_default();
    match name.as_str() {
        "PING" => b"+PONG\r\n".to_vec(),
        "SELECT" => b"+OK\r\n".to_vec(),
        "GET" => match db.live(&key) {
            Some((v, _)) => bulk(v),
            None => b"$-1\r\n".to_vec(),
        },
        "SET" => {
            let opts: Vec<String> = args[3..]
                .iter()
                .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
                .collect();
            let exists = db.live(&key).is_some();
            if (opts.contains(&"NX".into()) && exists) || (opts.contains(&"XX".into()) && !exists) {
                return b"$-1\r\n".to_vec();
            }
            let expiry = opts.iter().position(|o| o == "PX" || o == "EX").map(|i| {
                let n: u64 = opts[i + 1].parse().unwrap();
                Instant::now()
                    + if opts[i] == "PX" {
                        Duration::from_millis(n)
                    } else {
                        Duration::from_secs(n)
                    }
            });
            db.data.insert(key, (args[2].clone(), expiry));
            b"+OK\r\n".to_vec()
        }
        "DEL" => {
            let n = db.live(&key).is_some() as i32;
            db.data.remove(&key);
            format!(":{n}\r\n").into_bytes()
        }
        "INCR" | "DECR" => {
            let delta = if name == "INCR" { 1 } else { -1 };
            let entry = db.live(&key);
            let current: i64 = entry
                .as_ref()
                .map(|(v, _)| String::from_utf8_lossy(v).parse().unwrap_or(0))
                .unwrap_or(0);
            let next = current + delta;
            let expiry = entry.and_then(|(_, e)| *e);
            db.data.insert(key, (next.to_string().into_bytes(), expiry));
            format!(":{next}\r\n").into_bytes()
        }
        "PEXPIRE" => match db.live(&key) {
            Some(e) => {
                let ms: u64 = String::from_utf8_lossy(&args[2]).parse().unwrap();
                e.1 = Some(Instant::now() + Duration::from_millis(ms));
                b":1\r\n".to_vec()
            }
            None => b":0\r\n".to_vec(),
        },
        "PTTL" => match db.live(&key) {
            None => b":-2\r\n".to_vec(),
            Some((_, None)) => b":-1\r\n".to_vec(),
            Some((_, Some(at))) => format!(
                ":{}\r\n",
                at.saturating_duration_since(Instant::now()).as_millis()
            )
            .into_bytes(),
        },
        "BOOM" => b"-ERR boom\r\n".to_vec(),
        _ => format!("-ERR unknown command '{name}'\r\n").into_bytes(),
    }
}

async fn fake(password: Option<&str>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let db = Arc::new(Mutex::new(Db::default()));
    let password_for_url = password.map(String::from);
    let password = password_for_url.clone();
    let shared = db.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (db, password) = (shared.clone(), password.clone());
            tokio::spawn(async move {
                let mut r = BufReader::new(stream);
                let mut authed = false;
                // replies to everything that arrived together are written together
                while let Some(args) = read_command(&mut r).await {
                    let reply = execute(&mut db.lock().unwrap(), &args, &password, &mut authed);
                    if r.get_mut().write_all(&reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let url = match (std::env::var("ULLUN_TEST_REDIS"), password_for_url) {
        (Ok(real), _) => real,
        (_, Some(p)) => format!("redis://:{p}@{addr}/2"),
        _ => format!("redis://{addr}"),
    };
    Fake { url, db, task }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}{}:", ullun::security::random_token(8).unwrap())
}

#[tokio::test]
async fn client_runs_commands_and_pipelines() {
    let f = fake(None).await;
    let r = Redis::from_url(&f.url).unwrap();
    assert_eq!(
        r.run(Cmd::new("PING")).await.unwrap(),
        Value::Status("PONG".into())
    );
    let k = unique("k");
    assert!(r
        .run(Cmd::new("SET").arg(&k).arg(b"a\r\nb\0"))
        .await
        .unwrap()
        .is_ok());
    assert_eq!(
        r.run(Cmd::new("GET").arg(&k))
            .await
            .unwrap()
            .into_bytes()
            .unwrap(),
        b"a\r\nb\0"
    );
    assert_eq!(
        r.run(Cmd::new("GET").arg("missing-key")).await.unwrap(),
        Value::Nil
    );

    let replies = r
        .pipeline(&[
            Cmd::new("INCR").arg("ctr"),
            Cmd::new("BOOM"),
            Cmd::new("INCR").arg("ctr"),
        ])
        .await
        .unwrap();
    assert_eq!(replies[0], Value::Int(1));
    assert!(matches!(&replies[1], Value::Error(e) if e.contains("boom")));
    assert_eq!(replies[2], Value::Int(2));
    // an error reply of a single command is an Err
    assert!(r.run(Cmd::new("BOOM")).await.is_err());
}

#[tokio::test]
async fn client_authenticates_and_reuses_connections() {
    let f = fake(Some("s3cret")).await;
    let r = Redis::from_url(&f.url).unwrap();
    assert!(r.run(Cmd::new("PING")).await.is_ok());
    for _ in 0..5 {
        r.run(Cmd::new("PING")).await.unwrap();
    }
    let wrong = Redis::from_url(&f.url.replace("s3cret", "nope")).unwrap();
    assert!(wrong.run(Cmd::new("PING")).await.is_err());
}

#[tokio::test]
async fn client_reconnects_after_the_server_dropped_idle_connections() {
    let f = fake(None).await;
    let r = Redis::from_url(&f.url).unwrap();
    r.run(Cmd::new("PING")).await.unwrap();
    // restart the server on the same port: the pooled connection is dead
    let port = f.url.rsplit(':').next().unwrap().to_string();
    drop(f);
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await;
    if let Ok(listener) = listener {
        let task = tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut r = BufReader::new(s);
                    while read_command(&mut r).await.is_some() {
                        if r.get_mut().write_all(b"+PONG\r\n").await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        assert!(r.run(Cmd::new("PING")).await.is_ok());
        task.abort();
    }
}

#[tokio::test]
async fn unreachable_server_is_an_error_not_a_hang() {
    let r = Redis::from_url("redis://127.0.0.1:1")
        .unwrap()
        .timeout(Duration::from_millis(500));
    assert!(r.run(Cmd::new("PING")).await.is_err());
}

#[tokio::test]
async fn sessions_roundtrip_and_expire() {
    let f = fake(None).await;
    let store = RedisSessionStore::new(Redis::from_url(&f.url).unwrap())
        .prefix(unique("s"))
        .ttl(Duration::from_millis(150));
    assert!(store.load("abc").await.is_none());

    let mut rec = SessionRecord::default();
    rec.data.insert(
        "user".into(),
        serde_json::json!({ "id": 7, "name": "Ann \"A\"" }),
    );
    rec.created_at = 100;
    rec.last_seen = 200;
    store.save("abc", &rec).await;
    let got = store.load("abc").await.unwrap();
    assert_eq!(got.data, rec.data);
    assert_eq!((got.created_at, got.last_seen), (100, 200));

    store.delete("abc").await;
    assert!(store.load("abc").await.is_none());

    store.save("gone", &rec).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(store.load("gone").await.is_none());
}

#[tokio::test]
async fn session_middleware_shares_state_between_instances() {
    let f = fake(None).await;
    let prefix = unique("mw");
    let instance = |url: String, prefix: String| {
        let store = RedisSessionStore::new(Redis::from_url(&url).unwrap()).prefix(prefix);
        App::new()
            .middleware(SessionLayer::with_store(store).insecure_cookies())
            .get("/login", |req: Request| async move {
                req.session()?.set("user", "ann")?;
                Ok(Response::text("ok"))
            })
            .get("/me", |req: Request| async move {
                Ok(Response::text(
                    req.session()?
                        .get::<String>("user")
                        .unwrap_or_else(|| "nobody".into()),
                ))
            })
    };
    let a = common::spawn(instance(f.url.clone(), prefix.clone())).await;
    let b = common::spawn(instance(f.url.clone(), prefix)).await;

    let login = a.get("/login").send().await;
    let sid = login.cookie("sid").expect("session cookie");
    // instance B has never seen this session in memory
    let me = b.get("/me").cookie("sid", &sid).send().await;
    assert_eq!(me.text(), "ann");
    assert_eq!(b.get("/me").send().await.text(), "nobody");
}

#[tokio::test]
async fn rate_limits_are_shared_and_fixed_windows_reset() {
    let f = fake(None).await;
    let new = || RedisRateLimitStore::new(Redis::from_url(&f.url).unwrap()).prefix(unique("rl"));
    let a = new();
    let key = "client";
    let w = Duration::from_millis(400);

    for expected in [2, 1, 0] {
        let d = a.hit(key, 3, w, false).await;
        assert!(d.allowed);
        assert_eq!(d.remaining, expected);
    }
    let denied = a.hit(key, 3, w, false).await;
    assert!(!denied.allowed);
    assert_eq!((denied.remaining, denied.limit), (0, 3));
    assert!(denied.reset_secs >= 1);

    // a second store on the same Redis and prefix shares the counter
    let shared = RedisRateLimitStore::new(Redis::from_url(&f.url).unwrap());
    let other = RedisRateLimitStore::new(Redis::from_url(&f.url).unwrap());
    let key = unique("shared");
    assert!(
        shared
            .hit(&key, 1, Duration::from_secs(5), false)
            .await
            .allowed
    );
    assert!(
        !other
            .hit(&key, 1, Duration::from_secs(5), false)
            .await
            .allowed
    );

    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(a.hit("client", 3, w, false).await.allowed);
}

#[tokio::test]
async fn sliding_window_denials_do_not_count() {
    let f = fake(None).await;
    let store = RedisRateLimitStore::new(Redis::from_url(&f.url).unwrap()).prefix(unique("sl"));
    let w = Duration::from_secs(60);
    assert!(store.hit("c", 2, w, true).await.allowed);
    assert!(store.hit("c", 2, w, true).await.allowed);
    for _ in 0..5 {
        assert!(!store.hit("c", 2, w, true).await.allowed);
    }
    // five denials were handed back: raising the limit by one admits exactly one request
    assert!(store.hit("c", 3, w, true).await.allowed);
    assert!(!store.hit("c", 3, w, true).await.allowed);
}

#[tokio::test]
async fn rate_limiter_middleware_uses_the_store_and_fails_open() {
    let f = fake(None).await;
    let store = RedisRateLimitStore::new(Redis::from_url(&f.url).unwrap()).prefix(unique("mw"));
    let app = App::new()
        .middleware(RateLimiter::new(2, Duration::from_secs(60)).store(store))
        .get("/", |_req: Request| async { Ok(Response::text("hi")) });
    let s = common::spawn(app).await;
    assert_eq!(s.get("/").send().await.status, 200);
    let second = s.get("/").send().await;
    assert_eq!(second.status, 200);
    assert_eq!(second.headers["x-ratelimit-remaining"], "0");
    assert_eq!(s.get("/").send().await.status, 429);
    assert!(f.commands() > 0);

    // Redis gone: requests are let through
    let dead = RedisRateLimitStore::new(
        Redis::from_url("redis://127.0.0.1:1")
            .unwrap()
            .timeout(Duration::from_millis(300)),
    );
    let app = App::new()
        .middleware(RateLimiter::new(1, Duration::from_secs(60)).store(dead))
        .get("/", |_req: Request| async { Ok(Response::text("hi")) });
    let s = common::spawn(app).await;
    for _ in 0..3 {
        assert_eq!(s.get("/").send().await.status, 200);
    }
}

fn stored() -> StoredResponse {
    StoredResponse {
        status: 201,
        headers: vec![("content-type".into(), b"text/plain".to_vec())],
        body: bytes::Bytes::from_static(b"created\0\r\n"),
    }
}

#[tokio::test]
async fn idempotency_lifecycle() {
    let f = fake(None).await;
    let store = RedisIdempotencyStore::new(Redis::from_url(&f.url).unwrap()).prefix(unique("id"));
    let (lock, ttl) = (Duration::from_secs(30), Duration::from_secs(60));

    assert!(matches!(store.begin("k", 1, lock).await, Begin::Started));
    assert!(matches!(store.begin("k", 1, lock).await, Begin::InFlight));
    assert!(matches!(store.begin("k", 2, lock).await, Begin::Mismatch));

    store.complete("k", stored(), ttl).await;
    match store.begin("k", 1, lock).await {
        Begin::Replay(res) => {
            assert_eq!(res.status, 201);
            assert_eq!(res.body.as_ref(), b"created\0\r\n");
            assert_eq!(res.headers, stored().headers);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(store.begin("k", 2, lock).await, Begin::Mismatch));

    // abandoning a finished key keeps its response
    store.abandon("k").await;
    assert!(matches!(store.begin("k", 1, lock).await, Begin::Replay(_)));

    // abandoning a running claim frees it
    assert!(matches!(store.begin("j", 1, lock).await, Begin::Started));
    store.abandon("j").await;
    assert!(matches!(store.begin("j", 1, lock).await, Begin::Started));
}

#[tokio::test]
async fn idempotency_claims_lapse_and_only_one_caller_wins() {
    let f = fake(None).await;
    let store = Arc::new(
        RedisIdempotencyStore::new(Redis::from_url(&f.url).unwrap()).prefix(unique("race")),
    );
    let lock = Duration::from_millis(200);
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { store.begin("k", 1, lock).await })
        })
        .collect();
    let mut started = 0;
    for t in tasks {
        if matches!(t.await.unwrap(), Begin::Started) {
            started += 1;
        }
    }
    assert_eq!(started, 1);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(store.begin("k", 1, lock).await, Begin::Started));
}

#[tokio::test]
async fn idempotency_fails_closed_when_redis_is_down() {
    let store = RedisIdempotencyStore::new(
        Redis::from_url("redis://127.0.0.1:1")
            .unwrap()
            .timeout(Duration::from_millis(300)),
    );
    assert!(matches!(
        store.begin("k", 1, Duration::from_secs(1)).await,
        Begin::Unavailable
    ));
    // complete and abandon never panic either
    store.complete("k", stored(), Duration::from_secs(1)).await;
    store.abandon("k").await;
}

#[tokio::test]
async fn idempotency_middleware_replays_across_instances() {
    let f = fake(None).await;
    let prefix = unique("mw");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let instance =
        |calls: Arc<std::sync::atomic::AtomicUsize>| {
            let store =
                RedisIdempotencyStore::new(Redis::from_url(&f.url).unwrap()).prefix(prefix.clone());
            App::new()
                .middleware(Idempotency::with_store(store).shared_seed(b"0123456789abcdef-seed"))
                .post("/pay", move |_req: Request| {
                    let calls = calls.clone();
                    async move {
                        let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        Ok(Response::text(format!("charge {n}"))
                            .with_status(http::StatusCode::CREATED))
                    }
                })
        };
    let a = common::spawn(instance(calls.clone())).await;
    let b = common::spawn(instance(calls.clone())).await;

    let first = a
        .post("/pay")
        .header("idempotency-key", "abc")
        .text("x")
        .send()
        .await;
    assert_eq!((first.status, first.text().as_str()), (201, "charge 1"));
    let retry = b
        .post("/pay")
        .header("idempotency-key", "abc")
        .text("x")
        .send()
        .await;
    assert_eq!(retry.text(), "charge 1");
    assert_eq!(retry.headers["idempotency-replayed"], "true");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    let other_payload = b
        .post("/pay")
        .header("idempotency-key", "abc")
        .text("y")
        .send()
        .await;
    assert_eq!(other_payload.status, 422);
}

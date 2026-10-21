//! API-key and Basic authentication

use ullun::auth::{ApiKeyAuth, BasicAuth};
use ullun::prelude::*;
use ullun::testing::TestClient;

fn who(req: Request) -> std::future::Ready<Result<Response>> {
    std::future::ready(Ok(Response::text(
        req.principal().unwrap_or("nobody").to_string(),
    )))
}

fn basic(user: &str, pass: &str) -> String {
    // base64 of "user:pass" without a dependency
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{pass}");
    let mut out = String::new();
    for chunk in raw.as_bytes().chunks(3) {
        let n =
            chunk.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

#[tokio::test]
async fn api_key_accepts_known_keys_and_names_the_caller() {
    let c = TestClient::new(
        App::new()
            .middleware(
                ApiKeyAuth::new()
                    .key("billing", "k-billing-1")
                    .key("search", "k-search-1")
                    .except_paths(["/healthz"]),
            )
            .get("/who", who)
            .get("/healthz", who),
    );
    let ok = c.get("/who").header("x-api-key", "k-search-1").send().await;
    assert_eq!((ok.status(), ok.text().as_str()), (200, "search"));
    assert_eq!(
        c.get("/who")
            .header("x-api-key", "k-billing-1")
            .send()
            .await
            .text(),
        "billing"
    );

    let missing = c.get("/who").send().await;
    assert_eq!(missing.status(), 401);
    assert_eq!(missing.header("www-authenticate"), "ApiKey");
    assert_eq!(missing.header("cache-control"), "no-store");
    for wrong in ["k-search-", "k-search-1 ", "K-SEARCH-1", ""] {
        assert_eq!(
            c.get("/who")
                .header("x-api-key", wrong)
                .send()
                .await
                .status(),
            401,
            "{wrong:?}"
        );
    }
    // probes need no key and carry no principal
    assert_eq!(c.get("/healthz").send().await.text(), "nobody");
}

#[tokio::test]
async fn api_key_header_and_bearer_options() {
    let c = TestClient::new(
        App::new()
            .middleware(
                ApiKeyAuth::new()
                    .header("X-Token")
                    .allow_bearer()
                    .key("svc", "tok"),
            )
            .get("/who", who),
    );
    assert_eq!(
        c.get("/who").header("x-token", "tok").send().await.text(),
        "svc"
    );
    assert_eq!(c.get("/who").bearer("tok").send().await.text(), "svc");
    assert_eq!(c.get("/who").bearer("nope").send().await.status(), 401);
    assert_eq!(
        c.get("/who")
            .header("x-api-key", "tok")
            .send()
            .await
            .status(),
        401
    );
    assert_eq!(
        c.get("/who").send().await.header("www-authenticate"),
        "Bearer"
    );

    // bearer is off unless asked for
    let strict = TestClient::new(
        App::new()
            .middleware(ApiKeyAuth::new().key("svc", "tok"))
            .get("/who", who),
    );
    assert_eq!(strict.get("/who").bearer("tok").send().await.status(), 401);
}

#[tokio::test]
async fn basic_auth_challenges_and_accepts_valid_credentials() {
    let c = TestClient::new(
        App::new()
            .middleware(
                BasicAuth::new("admin area")
                    .user("ops", "p:ass word")
                    .user("dev", "x"),
            )
            .get("/", who),
    );
    let anon = c.get("/").send().await;
    assert_eq!(anon.status(), 401);
    assert_eq!(
        anon.header("www-authenticate"),
        "Basic realm=\"admin area\", charset=\"UTF-8\""
    );

    let ok = c
        .get("/")
        .header("authorization", &basic("ops", "p:ass word"))
        .send()
        .await;
    assert_eq!((ok.status(), ok.text().as_str()), (200, "ops"));
    assert_eq!(
        c.get("/")
            .header("authorization", &basic("dev", "x"))
            .send()
            .await
            .text(),
        "dev"
    );

    for (u, p) in [
        ("ops", "wrong"),
        ("nobody", "p:ass word"),
        ("ops", ""),
        ("dev", "p:ass word"),
    ] {
        let res = c
            .get("/")
            .header("authorization", &basic(u, p))
            .send()
            .await;
        assert_eq!(res.status(), 401, "{u}:{p}");
    }
    // malformed headers are just "no credentials"
    for bad in ["Basic", "Basic !!!", "Basic Zm9v", "Bearer abc", "basic"] {
        assert_eq!(
            c.get("/")
                .header("authorization", bad)
                .send()
                .await
                .status(),
            401,
            "{bad}"
        );
    }
    // the scheme name is case-insensitive
    let lower = basic("ops", "p:ass word").replacen("Basic", "basic", 1);
    assert_eq!(
        c.get("/")
            .header("authorization", &lower)
            .send()
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn basic_auth_with_a_custom_verifier() {
    let c = TestClient::new(
        App::new()
            .middleware(
                BasicAuth::new("db")
                    .verify_with(|user, pass| async move { user == "ann" && pass.len() > 3 })
                    .except_paths(["/public"]),
            )
            .get("/", who)
            .get("/public", who),
    );
    assert_eq!(
        c.get("/")
            .header("authorization", &basic("ann", "longpass"))
            .send()
            .await
            .text(),
        "ann"
    );
    assert_eq!(
        c.get("/")
            .header("authorization", &basic("ann", "no"))
            .send()
            .await
            .status(),
        401
    );
    assert_eq!(c.get("/public").send().await.status(), 200);
}

#[tokio::test]
async fn auth_failures_do_not_reach_the_handler_and_rate_limits_can_sit_in_front() {
    use std::time::Duration;
    use ullun::rate_limit::RateLimiter;
    let c = TestClient::new(
        App::new()
            .middleware(RateLimiter::new(2, Duration::from_secs(60)))
            .middleware(ApiKeyAuth::new().key("a", "right"))
            .get("/", who),
    );
    assert_eq!(
        c.get("/").header("x-api-key", "bad").send().await.status(),
        401
    );
    assert_eq!(
        c.get("/").header("x-api-key", "bad").send().await.status(),
        401
    );
    // guessing is throttled
    assert_eq!(
        c.get("/")
            .header("x-api-key", "right")
            .send()
            .await
            .status(),
        429
    );
}

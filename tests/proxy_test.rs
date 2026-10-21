//! Trusted proxies: client address and scheme

use ullun::prelude::*;
use ullun::proxy::TrustedProxies;
use ullun::rate_limit::RateLimiter;
use ullun::testing::TestClient;

fn app(proxies: Option<TrustedProxies>) -> App {
    let mut app = App::new().get("/ip", |req: Request| async move {
        Ok(Response::text(format!(
            "{} {}",
            req.client_ip().map(|i| i.to_string()).unwrap_or_default(),
            req.is_secure()
        )))
    });
    if let Some(p) = proxies {
        app = app.trusted_proxies(p);
    }
    app
}

fn lb() -> TrustedProxies {
    TrustedProxies::new().trust("10.0.0.0/8").unwrap()
}

async fn ip(app: App, peer: [u8; 4], headers: &[(&str, &str)]) -> String {
    let c = TestClient::new(app);
    let mut call = c.get("/ip").remote_addr((peer, 4000));
    for (k, v) in headers {
        call = call.header(k, v);
    }
    call.send().await.text()
}

#[tokio::test]
async fn without_trusted_proxies_headers_are_ignored() {
    let out = ip(
        app(None),
        [10, 0, 0, 1],
        &[
            ("x-forwarded-for", "1.2.3.4"),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    assert_eq!(out, "10.0.0.1 false");
}

#[tokio::test]
async fn only_trusted_peers_may_set_the_client() {
    // a stranger cannot claim to be someone else
    let out = ip(
        app(Some(lb())),
        [203, 0, 113, 5],
        &[
            ("x-forwarded-for", "1.2.3.4"),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    assert_eq!(out, "203.0.113.5 false");
    // the load balancer can
    let out = ip(
        app(Some(lb())),
        [10, 0, 0, 1],
        &[
            ("x-forwarded-for", "198.51.100.7"),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    assert_eq!(out, "198.51.100.7 true");
}

#[tokio::test]
async fn spoofed_leftmost_entries_are_skipped() {
    // the client sent "6.6.6.6"; our own proxy appended the real address
    let out = ip(
        app(Some(lb())),
        [10, 0, 0, 1],
        &[("x-forwarded-for", "6.6.6.6, 198.51.100.7")],
    )
    .await;
    assert_eq!(out, "198.51.100.7 false");
    // a chain of our own proxies is walked through
    let out = ip(
        app(Some(lb())),
        [10, 0, 0, 1],
        &[(
            "x-forwarded-for",
            "6.6.6.6, 198.51.100.7, 10.0.0.9, 10.0.0.8",
        )],
    )
    .await;
    assert_eq!(out, "198.51.100.7 false");
}

#[tokio::test]
async fn all_trusted_chain_and_garbage_fall_back_safely() {
    let out = ip(
        app(Some(lb())),
        [10, 0, 0, 1],
        &[("x-forwarded-for", "10.0.0.5, 10.0.0.6")],
    )
    .await;
    assert_eq!(out, "10.0.0.5 false");
    // an unparsable hop stops the walk at the last address we know
    let out = ip(
        app(Some(lb())),
        [10, 0, 0, 1],
        &[("x-forwarded-for", "9.9.9.9, unknown, 10.0.0.6")],
    )
    .await;
    assert_eq!(out, "10.0.0.6 false");
    let out = ip(app(Some(lb())), [10, 0, 0, 1], &[]).await;
    assert_eq!(out, "10.0.0.1 false");
}

#[tokio::test]
async fn forwarded_header_is_understood_and_wins() {
    let out = ip(
        app(Some(lb())),
        [10, 0, 0, 1],
        &[
            (
                "forwarded",
                "for=6.6.6.6;proto=http, for=\"[2001:db8::7]:4711\";proto=https",
            ),
            ("x-forwarded-for", "1.1.1.1"),
        ],
    )
    .await;
    assert_eq!(out, "2001:db8::7 true");
    let out = ip(
        app(Some(lb())),
        [10, 0, 0, 1],
        &[("forwarded", "for=198.51.100.7;proto=http")],
    )
    .await;
    assert_eq!(out, "198.51.100.7 false");
}

#[tokio::test]
async fn only_the_proxy_nearest_to_us_decides_the_scheme() {
    // the client claims https, our proxy says http
    let out = ip(
        app(Some(lb())),
        [10, 0, 0, 1],
        &[
            ("x-forwarded-for", "198.51.100.7"),
            ("x-forwarded-proto", "https, http"),
        ],
    )
    .await;
    assert_eq!(out, "198.51.100.7 false");
}

#[tokio::test]
async fn rate_limit_buckets_follow_the_real_client() {
    let app = App::new()
        .trusted_proxies(lb())
        .middleware(RateLimiter::new(1, std::time::Duration::from_secs(60)))
        .get("/", |_req: Request| async { Ok(Response::text("ok")) });
    let c = TestClient::new(app);
    let call = |client: &str| {
        c.get("/")
            .remote_addr(([10, 0, 0, 1], 4000))
            .header("x-forwarded-for", &format!("6.6.6.6, {client}"))
            .send()
    };
    assert_eq!(call("198.51.100.1").await.status(), 200);
    assert_eq!(call("198.51.100.1").await.status(), 429);
    // a different client behind the same proxy has its own budget
    assert_eq!(call("198.51.100.2").await.status(), 200);
}

#[tokio::test]
async fn invalid_networks_are_reported() {
    assert!(TrustedProxies::new().trust("10.0.0.0/40").is_err());
    assert!(TrustedProxies::new().trust("not an ip").is_err());
}

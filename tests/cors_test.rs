//! CORS and security headers over real HTTP

mod common;

use common::spawn;
use std::time::Duration;
use ullun::cors::Cors;
use ullun::prelude::*;
use ullun::security_headers::{Csp, FrameOptions, SecurityHeaders};

const APP: &str = "https://app.example.com";

fn api(cors: Cors) -> App {
    App::new()
        .middleware(cors)
        .get("/data", |_req: Request| async { Ok(Response::text("x")) })
        .get("/boom", |_req: Request| async {
            Err::<Response, _>(Error::internal("boom"))
        })
}

#[tokio::test]
async fn echoes_the_matching_origin_and_varies() {
    let s = spawn(api(Cors::new()
        .allow_origins([APP, "https://admin.example.com"])
        .allow_credentials()))
    .await;

    let a = s.get("/data").header("origin", APP).send().await;
    assert_eq!(a.header("access-control-allow-origin"), APP);
    assert_eq!(a.header("access-control-allow-credentials"), "true");
    assert!(a.header("vary").contains("origin"));

    // the second configured origin is echoed too, not just the first
    let b = s
        .get("/data")
        .header("origin", "https://admin.example.com")
        .send()
        .await;
    assert_eq!(
        b.header("access-control-allow-origin"),
        "https://admin.example.com"
    );
}

#[tokio::test]
async fn unknown_origins_get_no_cors_headers_but_still_vary() {
    let s = spawn(api(Cors::new().allow_origin(APP))).await;

    for origin in [
        "https://evil.example",
        "https://app.example.com.evil.io",
        "null",
    ] {
        let r = s.get("/data").header("origin", origin).send().await;
        assert_eq!(r.status, 200);
        assert!(
            !r.headers.contains_key("access-control-allow-origin"),
            "{origin}"
        );
        assert!(r.header("vary").contains("origin"));
    }
    // requests without Origin (same-origin, curl) still carry Vary for caches
    let plain = s.get("/data").send().await;
    assert!(plain.header("vary").contains("origin"));
}

#[tokio::test]
async fn any_origin_never_sends_credentials() {
    let s = spawn(api(Cors::new().allow_any_origin().allow_credentials())).await;
    let r = s
        .get("/data")
        .header("origin", "https://whatever.example")
        .send()
        .await;
    assert_eq!(r.header("access-control-allow-origin"), "*");
    assert!(!r.headers.contains_key("access-control-allow-credentials"));
    assert!(!r.headers.contains_key("vary"));
}

#[tokio::test]
async fn origin_predicate_allows_subdomains() {
    let s = spawn(api(Cors::new().allow_origin_fn(|o| {
        o.starts_with("https://") && o.ends_with(".example.com")
    })))
    .await;
    let ok = s
        .get("/data")
        .header("origin", "https://x.example.com")
        .send()
        .await;
    assert_eq!(
        ok.header("access-control-allow-origin"),
        "https://x.example.com"
    );
    let no = s
        .get("/data")
        .header("origin", "https://example.org")
        .send()
        .await;
    assert!(!no.headers.contains_key("access-control-allow-origin"));
}

#[tokio::test]
async fn preflight_is_answered_with_the_configured_policy() {
    let s = spawn(api(Cors::new()
        .allow_origin(APP)
        .allow_methods([http::Method::GET, http::Method::POST])
        .allow_headers(["content-type", "x-api-key"])
        .max_age(Duration::from_secs(3600))))
    .await;

    let r = s
        .request("OPTIONS", "/anything-at-all")
        .header("origin", APP)
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "x-evil")
        .send()
        .await;
    assert_eq!(r.status, 204);
    assert_eq!(r.header("access-control-allow-origin"), APP);
    assert_eq!(r.header("access-control-allow-methods"), "GET, POST");
    assert_eq!(
        r.header("access-control-allow-headers"),
        "content-type, x-api-key"
    );
    assert_eq!(r.header("access-control-max-age"), "3600");
}

#[tokio::test]
async fn preflight_mirrors_requested_headers_by_default() {
    let s = spawn(api(Cors::new().allow_origin(APP))).await;
    let r = s
        .request("OPTIONS", "/data")
        .header("origin", APP)
        .header("access-control-request-method", "PUT")
        .header("access-control-request-headers", "authorization, x-custom")
        .send()
        .await;
    assert_eq!(r.status, 204);
    assert_eq!(
        r.header("access-control-allow-headers"),
        "authorization, x-custom"
    );
    assert!(r.header("vary").contains("access-control-request-headers"));
}

#[tokio::test]
async fn preflight_from_unknown_origin_or_for_unlisted_method_is_refused() {
    let s = spawn(api(Cors::new()
        .allow_origin(APP)
        .allow_methods([http::Method::GET])))
    .await;

    let evil = s
        .request("OPTIONS", "/data")
        .header("origin", "https://evil.example")
        .header("access-control-request-method", "GET")
        .send()
        .await;
    assert_eq!(evil.status, 403);
    assert!(!evil.headers.contains_key("access-control-allow-origin"));

    let method = s
        .request("OPTIONS", "/data")
        .header("origin", APP)
        .header("access-control-request-method", "DELETE")
        .send()
        .await;
    assert_eq!(method.status, 403);
}

#[tokio::test]
async fn plain_options_without_cors_headers_is_not_a_preflight() {
    let s = spawn(api(Cors::new().allow_origin(APP))).await;
    let r = s.request("OPTIONS", "/data").send().await;
    // the automatic OPTIONS answer lists the allowed methods
    assert_eq!(r.status, 204);
    assert!(r.headers.contains_key("allow"));
    assert!(!r.headers.contains_key("access-control-allow-origin"));
}

#[tokio::test]
async fn error_responses_carry_cors_headers() {
    let s = spawn(api(Cors::new()
        .allow_origin(APP)
        .expose_headers(["x-request-id"])))
    .await;
    let r = s.get("/boom").header("origin", APP).send().await;
    assert_eq!(r.status, 500);
    assert_eq!(r.header("access-control-allow-origin"), APP);
    assert_eq!(r.header("access-control-expose-headers"), "x-request-id");

    let missing = s.get("/nope").header("origin", APP).send().await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.header("access-control-allow-origin"), APP);
}

#[tokio::test]
async fn private_network_access_is_opt_in() {
    let on = spawn(api(Cors::new().allow_origin(APP).allow_private_network())).await;
    let off = spawn(api(Cors::new().allow_origin(APP))).await;
    let ask = |s: common::Server| async move {
        s.request("OPTIONS", "/data")
            .header("origin", APP)
            .header("access-control-request-method", "GET")
            .header("access-control-request-private-network", "true")
            .send()
            .await
    };
    assert_eq!(
        ask(on).await.header("access-control-allow-private-network"),
        "true"
    );
    assert!(!ask(off)
        .await
        .headers
        .contains_key("access-control-allow-private-network"));
}

#[tokio::test]
async fn existing_vary_is_preserved() {
    let app = App::new()
        .middleware(Cors::new().allow_origin(APP))
        .get("/v", |_req: Request| async {
            Ok(Response::text("x").with_header("vary", "accept-encoding"))
        });
    let s = spawn(app).await;
    let r = s.get("/v").header("origin", APP).send().await;
    assert_eq!(r.header("vary"), "accept-encoding, origin");
}

// ---- security headers ------------------------------------------------------

#[tokio::test]
async fn security_headers_defaults_cover_errors_and_misses() {
    let app = App::new()
        .middleware(SecurityHeaders::new())
        .get("/", |_req: Request| async { Ok(Response::text("x")) })
        .get("/boom", |_req: Request| async {
            Err::<Response, _>(Error::internal("boom"))
        });
    let s = spawn(app).await;
    for (path, status) in [("/", 200), ("/boom", 500), ("/missing", 404)] {
        let r = s.get(path).send().await;
        assert_eq!(r.status, status);
        assert_eq!(r.header("x-content-type-options"), "nosniff");
        assert_eq!(r.header("x-frame-options"), "DENY");
        assert_eq!(r.header("referrer-policy"), "no-referrer");
        assert_eq!(r.header("cross-origin-opener-policy"), "same-origin");
        assert!(r.header("permissions-policy").contains("camera=()"));
        assert!(!r.headers.contains_key("content-security-policy"));
    }
}

#[tokio::test]
async fn handler_headers_win_and_defaults_can_be_changed() {
    let app = App::new()
        .middleware(
            SecurityHeaders::new()
                .frame_options(FrameOptions::SameOrigin)
                .header("Referrer-Policy", "same-origin")
                .without("cross-origin-resource-policy"),
        )
        .get("/embed", |_req: Request| async {
            Ok(Response::text("x").with_header("x-frame-options", "ALLOWALL"))
        })
        .get("/", |_req: Request| async { Ok(Response::text("x")) });
    let s = spawn(app).await;

    let embed = s.get("/embed").send().await;
    assert_eq!(embed.header("x-frame-options"), "ALLOWALL");
    let home = s.get("/").send().await;
    assert_eq!(home.header("x-frame-options"), "SAMEORIGIN");
    assert_eq!(home.header("referrer-policy"), "same-origin");
    assert!(!home.headers.contains_key("cross-origin-resource-policy"));
}

#[tokio::test]
async fn csp_nonce_is_fresh_per_request_and_matches_the_header() {
    let app = App::new()
        .middleware(
            SecurityHeaders::new().csp(Csp::web().script_src(["'self'", "'nonce-{nonce}'"])),
        )
        .get("/", |req: Request| async move {
            Ok(Response::text(
                req.csp_nonce().unwrap_or("none").to_string(),
            ))
        });
    let s = spawn(app).await;

    let first = s.get("/").send().await;
    let second = s.get("/").send().await;
    let n1 = String::from_utf8(first.body.clone()).unwrap();
    let n2 = String::from_utf8(second.body.clone()).unwrap();
    assert_eq!(n1.len(), 32);
    assert_ne!(n1, n2);
    let csp = first.header("content-security-policy");
    assert!(
        csp.contains(&format!("script-src 'self' 'nonce-{n1}'")),
        "{csp}"
    );
    assert!(csp.contains("frame-ancestors 'none'"));
}

#[tokio::test]
async fn csp_report_only_uses_its_own_header() {
    let app = App::new()
        .middleware(SecurityHeaders::new().csp(Csp::api().report_uri("/csp").report_only()))
        .get("/", |_req: Request| async { Ok(Response::text("x")) });
    let s = spawn(app).await;
    let r = s.get("/").send().await;
    assert!(!r.headers.contains_key("content-security-policy"));
    assert_eq!(
        r.header("content-security-policy-report-only"),
        "default-src 'none'; frame-ancestors 'none'; report-uri /csp"
    );
}

#[tokio::test]
async fn request_without_nonce_policy_has_no_nonce() {
    let app = App::new()
        .middleware(SecurityHeaders::new().csp(Csp::web()))
        .get("/", |req: Request| async move {
            Ok(Response::text(format!("{:?}", req.csp_nonce())))
        });
    let s = spawn(app).await;
    assert_eq!(s.get("/").send().await.text(), "None");
}

#[tokio::test]
#[allow(deprecated)]
async fn deprecated_cors_function_delegates_to_the_new_middleware() {
    let list = App::new()
        .middleware(ullun::middleware::cors(vec![
            APP.to_string(),
            "https://b.example.com".to_string(),
        ]))
        .get("/", |_req: Request| async { Ok(Response::text("x")) });
    let s = spawn(list).await;
    let r = s
        .get("/")
        .header("origin", "https://b.example.com")
        .send()
        .await;
    assert_eq!(
        r.header("access-control-allow-origin"),
        "https://b.example.com"
    );
    assert_eq!(r.header("access-control-allow-credentials"), "true");

    let any = App::new()
        .middleware(ullun::middleware::cors(vec!["*".to_string()]))
        .get("/", |_req: Request| async { Ok(Response::text("x")) });
    let s = spawn(any).await;
    let r = s.get("/").header("origin", APP).send().await;
    assert_eq!(r.header("access-control-allow-origin"), "*");
}

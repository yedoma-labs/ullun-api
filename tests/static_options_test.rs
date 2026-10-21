//! Static files: precompressed variants, immutable caching, SPA fallback

use std::path::{Path, PathBuf};
use ullun::prelude::*;
use ullun::static_files::StaticOptions;
use ullun::testing::TestClient;

fn site(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ullun-static-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("assets")).unwrap();
    std::fs::write(d.join("index.html"), "<h1>app</h1>").unwrap();
    std::fs::write(d.join("app.js"), "console.log('plain')").unwrap();
    std::fs::write(d.join("app.js.br"), "BROTLI-BYTES").unwrap();
    std::fs::write(d.join("app.js.gz"), "GZIP-BYTES").unwrap();
    std::fs::write(d.join("only-gz.css"), "body{}").unwrap();
    std::fs::write(d.join("only-gz.css.gz"), "GZ-CSS").unwrap();
    std::fs::write(d.join("assets/logo.svg"), "<svg/>").unwrap();
    std::fs::write(d.join("main.3f9a2c1b.js"), "hashed").unwrap();
    std::fs::write(d.join("release-notes-2024.txt"), "notes").unwrap();
    d
}

fn client(dir: &Path, options: StaticOptions) -> TestClient {
    TestClient::new(App::new().serve_static_with("", dir.to_str().unwrap(), options))
}

#[tokio::test]
async fn precompressed_variants_follow_accept_encoding() {
    let c = client(&site("pre"), StaticOptions::new().precompressed());

    let br = c
        .get("/app.js")
        .header("accept-encoding", "gzip, br")
        .send()
        .await;
    assert_eq!(br.text(), "BROTLI-BYTES");
    assert_eq!(br.header("content-encoding"), "br");
    assert!(
        br.header("content-type").starts_with("text/javascript")
            || br
                .header("content-type")
                .starts_with("application/javascript")
    );
    assert_eq!(br.header("vary"), "accept-encoding");

    let gz = c
        .get("/app.js")
        .header("accept-encoding", "gzip")
        .send()
        .await;
    assert_eq!(
        (gz.text().as_str(), gz.header("content-encoding")),
        ("GZIP-BYTES", "gzip")
    );

    // explicit refusal, no header, and unsupported encodings get the plain file
    for accept in [Some("br;q=0, gzip;q=0"), Some("deflate"), None] {
        let mut call = c.get("/app.js");
        if let Some(a) = accept {
            call = call.header("accept-encoding", a);
        }
        let plain = call.send().await;
        assert_eq!(plain.text(), "console.log('plain')", "{accept:?}");
        assert!(!plain.headers().contains_key("content-encoding"));
        assert_eq!(plain.header("vary"), "accept-encoding");
    }

    // only the variant that exists is used
    let css = c
        .get("/only-gz.css")
        .header("accept-encoding", "br, gzip")
        .send()
        .await;
    assert_eq!(
        (css.text().as_str(), css.header("content-encoding")),
        ("GZ-CSS", "gzip")
    );
    assert!(css.header("content-type").starts_with("text/css"));
}

#[tokio::test]
async fn without_the_option_variants_are_ignored() {
    let c = client(&site("nopre"), StaticOptions::new());
    let res = c
        .get("/app.js")
        .header("accept-encoding", "br")
        .send()
        .await;
    assert_eq!(res.text(), "console.log('plain')");
    assert!(!res.headers().contains_key("vary"));
}

#[tokio::test]
async fn variants_keep_conditional_requests_and_ranges() {
    let c = client(&site("cond"), StaticOptions::new().precompressed());
    let first = c
        .get("/app.js")
        .header("accept-encoding", "br")
        .send()
        .await;
    let etag = first.header("etag").to_string();
    let again = c
        .get("/app.js")
        .header("accept-encoding", "br")
        .header("if-none-match", &etag)
        .send()
        .await;
    assert_eq!(again.status(), 304);
    let part = c
        .get("/app.js")
        .header("accept-encoding", "br")
        .header("range", "bytes=0-4")
        .send()
        .await;
    assert_eq!((part.status(), part.text().as_str()), (206, "BROTL"));
}

#[tokio::test]
async fn immutable_caching_for_directories_and_hashed_names() {
    let c = client(
        &site("immutable"),
        StaticOptions::new().immutable_dir("assets").hashed_names(),
    );
    let immutable = "public, max-age=31536000, immutable";
    assert_eq!(
        c.get("/assets/logo.svg")
            .send()
            .await
            .header("cache-control"),
        immutable
    );
    assert_eq!(
        c.get("/main.3f9a2c1b.js")
            .send()
            .await
            .header("cache-control"),
        immutable
    );
    // a number in a name is not a content hash
    assert_eq!(
        c.get("/release-notes-2024.txt")
            .send()
            .await
            .header("cache-control"),
        "public, max-age=3600"
    );
    assert_eq!(
        c.get("/app.js").send().await.header("cache-control"),
        "public, max-age=3600"
    );

    let custom = client(&site("cc"), StaticOptions::new().cache_control("no-cache"));
    assert_eq!(
        custom.get("/app.js").send().await.header("cache-control"),
        "no-cache"
    );
}

#[tokio::test]
async fn spa_fallback_serves_index_for_navigations_only() {
    let c = client(
        &site("spa"),
        StaticOptions::new().spa_fallback("index.html"),
    );
    let nav = c
        .get("/orders/17")
        .header("accept", "text/html,application/xhtml+xml")
        .send()
        .await;
    assert_eq!((nav.status(), nav.text().as_str()), (200, "<h1>app</h1>"));

    // missing assets, API-style requests and other methods stay 404
    assert_eq!(
        c.get("/missing.js")
            .header("accept", "text/html")
            .send()
            .await
            .status(),
        404
    );
    assert_eq!(
        c.get("/orders/17")
            .header("accept", "application/json")
            .send()
            .await
            .status(),
        404
    );
    assert_eq!(c.get("/orders/17").send().await.status(), 404);
    // real files win
    assert_eq!(
        c.get("/app.js")
            .header("accept", "text/html")
            .send()
            .await
            .text(),
        "console.log('plain')"
    );
    // dotfile probes are not rescued by the fallback either
    assert_eq!(
        c.get("/.env")
            .header("accept", "text/html")
            .send()
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn without_spa_fallback_unknown_paths_are_404() {
    let c = client(&site("nospa"), StaticOptions::new());
    assert_eq!(
        c.get("/orders/17")
            .header("accept", "text/html")
            .send()
            .await
            .status(),
        404
    );
}

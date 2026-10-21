#![cfg(feature = "signing")]
//! Signed cookies through the request/response helpers

use ullun::cookies::Cookie;
use ullun::prelude::*;
use ullun::signed_cookies::CookieSigner;
use ullun::testing::TestClient;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

fn app(signer: CookieSigner) -> App {
    let read = signer.clone();
    App::new()
        .get("/set", move |_req: Request| {
            let signer = signer.clone();
            async move {
                Ok(Response::text("set")
                    .signed_cookie(&signer, Cookie::new("theme", "dark").http_only()))
            }
        })
        .get("/get", move |req: Request| {
            let signer = read.clone();
            async move {
                Ok(Response::text(
                    req.signed_cookie(&signer, "theme")
                        .unwrap_or_else(|| "none".into()),
                ))
            }
        })
}

#[tokio::test]
async fn signed_cookie_roundtrip_with_the_cookie_jar() {
    let c = TestClient::new(app(CookieSigner::new(KEY).unwrap())).with_cookies();
    assert_eq!(c.get("/get").send().await.text(), "none");
    let set = c.get("/set").send().await;
    let header = set.header("set-cookie");
    assert!(header.starts_with("theme=dark."), "{header}");
    assert!(header.contains("HttpOnly"));
    assert_eq!(c.get("/get").send().await.text(), "dark");
}

#[tokio::test]
async fn forged_or_foreign_cookies_are_ignored() {
    let c = TestClient::new(app(CookieSigner::new(KEY).unwrap()));
    for forged in ["theme=evil", "theme=evil.AAAA", "theme=dark.AAAA"] {
        let res = c.get("/get").header("cookie", forged).send().await;
        assert_eq!(res.text(), "none", "{forged}");
    }
    // a cookie signed with another key
    let other = CookieSigner::new(b"ffffffffffffffffffffffffffffffff").unwrap();
    let signed = other.sign("theme", "dark");
    let res = c
        .get("/get")
        .header("cookie", &format!("theme={signed}"))
        .send()
        .await;
    assert_eq!(res.text(), "none");
}

#[tokio::test]
async fn rotated_keys_keep_old_cookies_working() {
    let old = CookieSigner::new(KEY).unwrap();
    let cookie = old.sign("theme", "dark");
    let rotated = CookieSigner::new(b"ffffffffffffffffffffffffffffffff")
        .unwrap()
        .previous_key(KEY)
        .unwrap();
    let c = TestClient::new(app(rotated));
    let res = c
        .get("/get")
        .header("cookie", &format!("theme={cookie}"))
        .send()
        .await;
    assert_eq!(res.text(), "dark");
}

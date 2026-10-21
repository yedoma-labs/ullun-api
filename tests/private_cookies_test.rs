#![cfg(feature = "encryption")]
//! Private (encrypted) cookies through the request/response helpers

use ullun::cookies::Cookie;
use ullun::prelude::*;
use ullun::private_cookies::CookieCipher;
use ullun::testing::TestClient;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const OTHER: &[u8] = b"ffffffffffffffffffffffffffffffff";

fn app(cipher: CookieCipher) -> App {
    let read = cipher.clone();
    App::new()
        .get("/set", move |_req: Request| {
            let cipher = cipher.clone();
            async move {
                Response::text("set")
                    .private_cookie(&cipher, Cookie::new("uid", "user-42").http_only())
            }
        })
        .get("/get", move |req: Request| {
            let cipher = read.clone();
            async move {
                Ok(Response::text(
                    req.private_cookie(&cipher, "uid")
                        .unwrap_or_else(|| "none".into()),
                ))
            }
        })
}

#[tokio::test]
async fn private_cookie_roundtrip_hides_the_value() {
    let c = TestClient::new(app(CookieCipher::new(KEY).unwrap())).with_cookies();
    assert_eq!(c.get("/get").send().await.text(), "none");
    let set = c.get("/set").send().await;
    let header = set.header("set-cookie");
    assert!(header.starts_with("uid="), "{header}");
    assert!(!header.contains("user-42"), "{header}");
    assert!(header.contains("HttpOnly"));
    assert_eq!(c.get("/get").send().await.text(), "user-42");
}

#[tokio::test]
async fn forged_foreign_and_moved_cookies_are_ignored() {
    let c = TestClient::new(app(CookieCipher::new(KEY).unwrap()));
    for forged in ["uid=user-42", "uid=AAAA", "uid=", "uid=!!!"] {
        let res = c.get("/get").header("cookie", forged).send().await;
        assert_eq!(res.text(), "none", "{forged}");
    }
    // sealed with another key
    let foreign = CookieCipher::new(OTHER)
        .unwrap()
        .seal("uid", "root")
        .unwrap();
    let res = c
        .get("/get")
        .header("cookie", &format!("uid={foreign}"))
        .send()
        .await;
    assert_eq!(res.text(), "none");
    // sealed for a different cookie name
    let moved = CookieCipher::new(KEY)
        .unwrap()
        .seal("role", "admin")
        .unwrap();
    let res = c
        .get("/get")
        .header("cookie", &format!("uid={moved}"))
        .send()
        .await;
    assert_eq!(res.text(), "none");
}

#[tokio::test]
async fn rotated_keys_keep_old_cookies_working() {
    let old = CookieCipher::new(KEY)
        .unwrap()
        .seal("uid", "user-42")
        .unwrap();
    let rotated = CookieCipher::new(OTHER).unwrap().previous_key(KEY).unwrap();
    let c = TestClient::new(app(rotated));
    let res = c
        .get("/get")
        .header("cookie", &format!("uid={old}"))
        .send()
        .await;
    assert_eq!(res.text(), "user-42");
}

#![cfg(feature = "jwt-asymmetric")]
//! Bearer auth with RS256 / ES256 tokens verified against a key set

use ullun::jwt::{expires_in, Claims, JwtAuth};
use ullun::jwt_keys::{KeySet, SigningKey};
use ullun::prelude::*;
use ullun::testing::TestClient;

const RSA_PKCS8: &[u8] = include_bytes!("fixtures/rsa_pkcs8.der");

fn app(keys: KeySet) -> App {
    App::new().group("/api", |g| {
        g.middleware(JwtAuth::with_keys(keys))
            .get("/me", |req: Request| async move {
                let claims = req.extensions.get::<Claims>().expect("set by JwtAuth");
                Ok(Response::text(claims.0["sub"].as_str().unwrap_or("?")))
            })
    })
}

fn token(key: &SigningKey) -> String {
    key.encode(&serde_json::json!({ "sub": "ann", "exp": expires_in(60) }))
        .unwrap()
}

#[tokio::test]
async fn rs256_token_from_a_jwks_is_accepted() {
    let key = SigningKey::rs256_pkcs8(RSA_PKCS8).unwrap().with_kid("k1");
    let c = TestClient::new(app(KeySet::new().with(key.verifying_key())));

    let res = c
        .get("/api/me")
        .header("authorization", &format!("Bearer {}", token(&key)))
        .send()
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.text(), "ann");

    assert_eq!(c.get("/api/me").send().await.status(), 401);
}

#[tokio::test]
async fn tokens_from_unknown_keys_and_hs256_tokens_are_rejected() {
    let trusted = SigningKey::rs256_pkcs8(RSA_PKCS8).unwrap();
    let c = TestClient::new(app(KeySet::new().with(trusted.verifying_key())));

    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &rng,
    )
    .unwrap();
    let stranger = SigningKey::es256_pkcs8(pkcs8.as_ref()).unwrap();
    let hs = ullun::jwt::encode(
        &serde_json::json!({ "sub": "root", "exp": expires_in(60) }),
        b"0123456789abcdef0123456789abcdef",
    )
    .unwrap();

    for bad in [token(&stranger), hs, "garbage".into()] {
        let res = c
            .get("/api/me")
            .header("authorization", &format!("Bearer {bad}"))
            .send()
            .await;
        assert_eq!(res.status(), 401, "{bad}");
    }
}

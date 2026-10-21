//! HS256 JSON Web Tokens and a bearer-token middleware (feature `jwt`)
//!
//! Only `HS256` is accepted here (RS256 / ES256: see `ullun::jwt_keys`): tokens with any other `alg` (including `none`)
//! are rejected, signatures are compared in constant time, and `exp` / `nbf`
//! are checked. Secrets must be at least 32 bytes (RFC 7518, section 3.2).
//!
//! ```
//! use ullun::jwt;
//!
//! let secret = b"0123456789abcdef0123456789abcdef";
//! let token = jwt::encode(&serde_json::json!({ "sub": "ann", "exp": jwt::expires_in(3600) }), secret).unwrap();
//! let claims: serde_json::Value = jwt::decode(&token, secret).unwrap();
//! assert_eq!(claims["sub"], "ann");
//! ```
//!
//! As middleware, valid claims are stored in `req.extensions`:
//!
//! ```no_run
//! use ullun::prelude::*;
//! use ullun::jwt::{Claims, JwtAuth};
//!
//! # async fn run() {
//! App::new()
//!     .group("/api", |g| {
//!         g.middleware(JwtAuth::new("0123456789abcdef0123456789abcdef"))
//!             .get("/me", |req: Request| async move {
//!                 let claims = req.extensions.get::<Claims>().expect("set by JwtAuth");
//!                 Ok(Response::json(&claims.0))
//!             })
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! ```

use crate::error::{Error, Result};
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

const MIN_SECRET_LEN: usize = 32;
const HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

/// Verified claims, inserted into `Request::extensions` by [`JwtAuth`]
#[derive(Debug, Clone, PartialEq)]
pub struct Claims(pub Value);

/// What `decode_with` checks besides signature, `exp` and `nbf`
#[derive(Debug, Clone, Default)]
pub struct Validation {
    /// Clock skew tolerated for `exp` and `nbf`, in seconds
    pub leeway_secs: u64,
    /// Required `iss` claim
    pub issuer: Option<String>,
    /// Required `aud` claim (string or array containing it)
    pub audience: Option<String>,
}

/// Seconds since the Unix epoch
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `exp` value for a token that lives `secs` seconds from now
pub fn expires_in(secs: u64) -> u64 {
    now() + secs
}

fn mac(secret: &[u8]) -> Result<HmacSha256> {
    if secret.len() < MIN_SECRET_LEN {
        return Err(Error::internal(format!(
            "JWT secret must be at least {} bytes",
            MIN_SECRET_LEN
        )));
    }
    HmacSha256::new_from_slice(secret).map_err(|_| Error::internal("Invalid JWT secret"))
}

/// Sign `claims` as an HS256 token
pub fn encode<T: Serialize>(claims: &T, secret: &[u8]) -> Result<String> {
    let payload =
        serde_json::to_vec(claims).map_err(|e| Error::internal(format!("JWT claims: {}", e)))?;
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(HEADER),
        URL_SAFE_NO_PAD.encode(payload)
    );
    let mut m = mac(secret)?;
    m.update(signing_input.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(m.finalize().into_bytes());
    Ok(format!("{}.{}", signing_input, signature))
}

/// Verify signature, `exp` and `nbf` and return the claims
pub fn decode<T: DeserializeOwned>(token: &str, secret: &[u8]) -> Result<T> {
    decode_with(token, secret, &Validation::default())
}

/// Like [`decode`] with leeway, issuer and audience checks
pub fn decode_with<T: DeserializeOwned>(
    token: &str,
    secret: &[u8],
    validation: &Validation,
) -> Result<T> {
    let invalid = || Error::unauthorized("Invalid token");

    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid());
    };

    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h).map_err(|_| invalid())?)
        .map_err(|_| invalid())?;
    if header.get("alg").and_then(Value::as_str) != Some("HS256") {
        return Err(invalid());
    }

    let signature = URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid())?;
    let mut m = mac(secret)?;
    m.update(h.as_bytes());
    m.update(b".");
    m.update(p.as_bytes());
    m.verify_slice(&signature).map_err(|_| invalid())?;

    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p).map_err(|_| invalid())?)
        .map_err(|_| invalid())?;
    check_claims(&claims, validation, now())?;
    serde_json::from_value(claims).map_err(|_| invalid())
}

pub(crate) fn check_claims(claims: &Value, v: &Validation, now: u64) -> Result<()> {
    if let Some(exp) = claims.get("exp") {
        let exp = exp
            .as_u64()
            .ok_or_else(|| Error::unauthorized("Invalid token"))?;
        if now >= exp.saturating_add(v.leeway_secs) {
            return Err(Error::unauthorized("Token expired"));
        }
    }
    if let Some(nbf) = claims.get("nbf") {
        let nbf = nbf
            .as_u64()
            .ok_or_else(|| Error::unauthorized("Invalid token"))?;
        if now.saturating_add(v.leeway_secs) < nbf {
            return Err(Error::unauthorized("Token not yet valid"));
        }
    }
    if let Some(iss) = &v.issuer {
        if claims.get("iss").and_then(Value::as_str) != Some(iss) {
            return Err(Error::unauthorized("Invalid token"));
        }
    }
    if let Some(aud) = &v.audience {
        let ok = match claims.get("aud") {
            Some(Value::String(a)) => a == aud,
            Some(Value::Array(list)) => list.iter().any(|a| a.as_str() == Some(aud)),
            _ => false,
        };
        if !ok {
            return Err(Error::unauthorized("Invalid token"));
        }
    }
    Ok(())
}

/// Middleware requiring `Authorization: Bearer <jwt>`
///
/// Answers `401` otherwise. CORS preflights pass through unchecked.
pub struct JwtAuth {
    secret: Vec<u8>,
    #[cfg(feature = "jwt-asymmetric")]
    keys: Option<crate::jwt_keys::KeySet>,
    validation: Validation,
}

impl JwtAuth {
    /// # Panics
    /// If the secret is shorter than 32 bytes
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        let secret = secret.into();
        assert!(
            secret.len() >= MIN_SECRET_LEN,
            "JWT secret must be at least {} bytes",
            MIN_SECRET_LEN
        );
        Self {
            secret,
            #[cfg(feature = "jwt-asymmetric")]
            keys: None,
            validation: Validation::default(),
        }
    }

    /// Verify RS256 / ES256 tokens against `keys` instead of an HS256 secret
    #[cfg(feature = "jwt-asymmetric")]
    pub fn with_keys(keys: crate::jwt_keys::KeySet) -> Self {
        Self {
            secret: Vec::new(),
            keys: Some(keys),
            validation: Validation::default(),
        }
    }

    pub fn validation(mut self, validation: Validation) -> Self {
        self.validation = validation;
        self
    }
}

#[async_trait]
impl Middleware for JwtAuth {
    async fn handle(&self, mut req: Request, next: Next) -> Result<Response> {
        if req.method == http::Method::OPTIONS {
            return next.run(req).await;
        }
        let token = req
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                let (scheme, token) = v.split_once(' ')?;
                scheme
                    .eq_ignore_ascii_case("bearer")
                    .then_some(token.trim())
            })
            .ok_or_else(|| Error::unauthorized("Missing bearer token"))?;

        #[cfg(feature = "jwt-asymmetric")]
        let claims: Value = match &self.keys {
            Some(keys) => keys.decode(token, &self.validation)?,
            None => decode_with(token, &self.secret, &self.validation)?,
        };
        #[cfg(not(feature = "jwt-asymmetric"))]
        let claims: Value = decode_with(token, &self.secret, &self.validation)?;
        req.extensions.insert(Claims(claims));
        next.run(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn tamper(token: &str, part: usize, f: impl Fn(&str) -> String) -> String {
        let mut p: Vec<String> = token.split('.').map(String::from).collect();
        p[part] = f(&p[part]);
        p.join(".")
    }

    #[test]
    fn roundtrip_with_typed_claims() {
        #[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
        struct C {
            sub: String,
            exp: u64,
        }
        let c = C {
            sub: "ann".into(),
            exp: expires_in(60),
        };
        let token = encode(&c, SECRET).unwrap();
        assert_eq!(token.split('.').count(), 3);
        assert_eq!(decode::<C>(&token, SECRET).unwrap(), c);
    }

    #[test]
    fn matches_the_jwt_io_reference_vector() {
        // https://jwt.io default HS256 example (secret is shorter than our
        // minimum, so check the primitives directly)
        let header = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let payload = "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ";
        assert_eq!(URL_SAFE_NO_PAD.encode(HEADER), header);

        let mut m = HmacSha256::new_from_slice(b"your-256-bit-secret").unwrap();
        m.update(format!("{}.{}", header, payload).as_bytes());
        assert_eq!(
            URL_SAFE_NO_PAD.encode(m.finalize().into_bytes()),
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        );
    }

    #[test]
    fn rejects_tampering_wrong_secret_and_malformed() {
        let token = encode(&json!({ "sub": "ann" }), SECRET).unwrap();
        let other = b"ffffffffffffffffffffffffffffffff";
        assert!(decode::<Value>(&token, other).is_err());
        assert!(decode::<Value>(
            &tamper(&token, 1, |_| URL_SAFE_NO_PAD.encode(r#"{"sub":"root"}"#)),
            SECRET
        )
        .is_err());
        assert!(decode::<Value>(&tamper(&token, 2, |s| format!("{}A", s)), SECRET).is_err());
        for bad in ["", "a", "a.b", "a.b.c", "a.b.c.d", "..", "!!!.!!!.!!!"] {
            assert_eq!(
                decode::<Value>(bad, SECRET).unwrap_err().status_code(),
                401,
                "{bad}"
            );
        }
    }

    #[test]
    fn rejects_alg_none_and_other_algs() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"sub":"root"}"#);
        for alg in ["none", "HS512", "RS256", "hs256"] {
            let header = URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"{}","typ":"JWT"}}"#, alg));
            let unsigned = format!("{}.{}.", header, payload);
            assert!(decode::<Value>(&unsigned, SECRET).is_err(), "{alg}");

            // even with a correct HS256 signature over that header
            let mut m = HmacSha256::new_from_slice(SECRET).unwrap();
            m.update(format!("{}.{}", header, payload).as_bytes());
            let signed = format!(
                "{}.{}.{}",
                header,
                payload,
                URL_SAFE_NO_PAD.encode(m.finalize().into_bytes())
            );
            assert!(decode::<Value>(&signed, SECRET).is_err(), "{alg} signed");
        }
    }

    #[test]
    fn exp_nbf_leeway_issuer_audience() {
        let v = Validation::default();
        let n = 1_000_000;
        assert!(check_claims(&json!({ "exp": n + 1 }), &v, n).is_ok());
        assert_eq!(
            check_claims(&json!({ "exp": n }), &v, n)
                .unwrap_err()
                .message(),
            "Token expired"
        );
        assert!(check_claims(
            &json!({ "exp": n }),
            &Validation {
                leeway_secs: 5,
                ..Default::default()
            },
            n
        )
        .is_ok());
        assert!(check_claims(&json!({ "nbf": n + 10 }), &v, n).is_err());
        assert!(check_claims(
            &json!({ "nbf": n + 10 }),
            &Validation {
                leeway_secs: 10,
                ..Default::default()
            },
            n
        )
        .is_ok());
        assert!(check_claims(&json!({ "exp": "soon" }), &v, n).is_err());

        let strict = Validation {
            issuer: Some("me".into()),
            audience: Some("api".into()),
            ..Default::default()
        };
        assert!(check_claims(&json!({ "iss": "me", "aud": "api" }), &strict, n).is_ok());
        assert!(check_claims(&json!({ "iss": "me", "aud": ["x", "api"] }), &strict, n).is_ok());
        assert!(check_claims(&json!({ "iss": "you", "aud": "api" }), &strict, n).is_err());
        assert!(check_claims(&json!({ "iss": "me" }), &strict, n).is_err());
    }

    #[test]
    fn expired_token_is_rejected_end_to_end() {
        let token = encode(&json!({ "exp": now() - 10 }), SECRET).unwrap();
        assert_eq!(
            decode::<Value>(&token, SECRET).unwrap_err().message(),
            "Token expired"
        );
    }

    #[test]
    fn short_secrets_are_refused() {
        assert!(encode(&json!({}), b"short").is_err());
        let token = encode(&json!({}), SECRET).unwrap();
        assert!(decode::<Value>(&token, b"short").is_err());
    }

    #[test]
    #[should_panic(expected = "at least 32 bytes")]
    fn middleware_constructor_panics_on_short_secret() {
        let _ = JwtAuth::new("short");
    }
}

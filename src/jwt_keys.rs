//! RS256 and ES256 JSON Web Tokens and JWKS (feature `jwt-asymmetric`)
//!
//! Use asymmetric tokens when the party that verifies them must not be able to
//! mint them: an identity provider signs, your API only holds public keys.
//! [`KeySet::from_jwks`] reads a provider's JWKS document, [`SigningKey`]
//! signs tokens yourself. As with HS256 only the algorithm of the key decides
//! how a token is verified, never the `alg` header: `none`, `HS256` and a
//! mismatching algorithm are rejected, and `exp` / `nbf` are checked.
//!
//! ```
//! use ullun::jwt_keys::{KeySet, SigningKey};
//! use ullun::jwt::Validation;
//!
//! // ES256 key pair (PKCS#8); in production load it from a secret store
//! let rng = ring::rand::SystemRandom::new();
//! let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(
//!     &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
//!     &rng,
//! )
//! .unwrap();
//! let signing = SigningKey::es256_pkcs8(pkcs8.as_ref()).unwrap().with_kid("k1");
//! let token = signing
//!     .encode(&serde_json::json!({ "sub": "ann", "exp": ullun::jwt::expires_in(60) }))
//!     .unwrap();
//!
//! let keys = KeySet::new().with(signing.verifying_key());
//! let claims: serde_json::Value = keys.decode(&token, &Validation::default()).unwrap();
//! assert_eq!(claims["sub"], "ann");
//! ```
//!
//! As middleware: `JwtAuth::with_keys(keys)`.

use crate::error::{Error, Result};
use crate::jwt::{check_claims, now, Validation};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{
    EcdsaKeyPair, KeyPair, RsaKeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_FIXED,
    ECDSA_P256_SHA256_FIXED_SIGNING, RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_SHA256,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

/// Supported asymmetric algorithms
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// RSASSA-PKCS1-v1_5 with SHA-256 (2048 to 8192 bit keys)
    RS256,
    /// ECDSA over P-256 with SHA-256
    ES256,
}

impl Algorithm {
    fn name(self) -> &'static str {
        match self {
            Algorithm::RS256 => "RS256",
            Algorithm::ES256 => "ES256",
        }
    }
}

fn invalid() -> Error {
    Error::unauthorized("Invalid token")
}

fn bad_key(what: &str) -> Error {
    Error::internal(format!("Invalid JWT key: {what}"))
}

/// A public key that verifies tokens
#[derive(Debug, Clone)]
pub struct VerifyingKey {
    algorithm: Algorithm,
    kid: Option<String>,
    /// RS256: DER `RSAPublicKey`; ES256: uncompressed SEC1 point
    public: Vec<u8>,
}

impl VerifyingKey {
    /// RS256 key from a DER `RSAPublicKey` (PKCS#1)
    pub fn rs256_der(der: &[u8]) -> Self {
        Self {
            algorithm: Algorithm::RS256,
            kid: None,
            public: der.to_vec(),
        }
    }

    /// ES256 key from an uncompressed P-256 point (`0x04 || x || y`, 65 bytes)
    pub fn es256_point(point: &[u8]) -> Result<Self> {
        if point.len() != 65 || point[0] != 4 {
            return Err(bad_key("expected an uncompressed P-256 point"));
        }
        Ok(Self {
            algorithm: Algorithm::ES256,
            kid: None,
            public: point.to_vec(),
        })
    }

    /// Key from a JSON Web Key (`kty` `RSA` or `EC` with `crv` `P-256`)
    pub fn from_jwk(jwk: &Value) -> Result<Self> {
        let field = |name: &str| -> Result<Vec<u8>> {
            let v = jwk
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| bad_key(&format!("JWK is missing `{name}`")))?;
            URL_SAFE_NO_PAD
                .decode(v)
                .map_err(|_| bad_key(&format!("JWK `{name}` is not base64url")))
        };
        let mut key = match jwk.get("kty").and_then(Value::as_str) {
            Some("RSA") => {
                let (n, e) = (field("n")?, field("e")?);
                let body = [der_integer(&n), der_integer(&e)].concat();
                Self::rs256_der(&der_wrap(0x30, &body))
            }
            Some("EC") => {
                if jwk.get("crv").and_then(Value::as_str) != Some("P-256") {
                    return Err(bad_key("only the P-256 curve is supported"));
                }
                let (x, y) = (field("x")?, field("y")?);
                if x.len() != 32 || y.len() != 32 {
                    return Err(bad_key("JWK coordinates must be 32 bytes"));
                }
                Self::es256_point(&[&[4][..], &x, &y].concat())?
            }
            _ => return Err(bad_key("unsupported JWK `kty`")),
        };
        key.kid = jwk.get("kid").and_then(Value::as_str).map(String::from);
        Ok(key)
    }

    /// Set the key id matched against the token's `kid` header
    pub fn with_kid(mut self, kid: impl Into<String>) -> Self {
        self.kid = Some(kid.into());
        self
    }

    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    pub fn kid(&self) -> Option<&str> {
        self.kid.as_deref()
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        let alg: &dyn ring::signature::VerificationAlgorithm = match self.algorithm {
            Algorithm::RS256 => &RSA_PKCS1_2048_8192_SHA256,
            Algorithm::ES256 => &ECDSA_P256_SHA256_FIXED,
        };
        UnparsedPublicKey::new(alg, &self.public)
            .verify(message, signature)
            .is_ok()
    }
}

/// DER length octets
fn der_len(len: usize) -> Vec<u8> {
    match len {
        0..=127 => vec![len as u8],
        128..=255 => vec![0x81, len as u8],
        _ => vec![0x82, (len >> 8) as u8, len as u8],
    }
}

fn der_wrap(tag: u8, body: &[u8]) -> Vec<u8> {
    [&[tag][..], &der_len(body.len()), body].concat()
}

/// Unsigned big-endian integer as a DER INTEGER
fn der_integer(bytes: &[u8]) -> Vec<u8> {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    let mut body = bytes[start..].to_vec();
    if body.first().is_none_or(|b| b & 0x80 != 0) {
        body.insert(0, 0);
    }
    der_wrap(0x02, &body)
}

/// A private key that signs tokens
pub struct SigningKey {
    kind: Kind,
    kid: Option<String>,
    rng: SystemRandom,
}

enum Kind {
    Rsa(RsaKeyPair),
    Ec(EcdsaKeyPair),
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("algorithm", &self.algorithm())
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl SigningKey {
    /// RS256 key from PKCS#8 DER
    pub fn rs256_pkcs8(der: &[u8]) -> Result<Self> {
        let pair = RsaKeyPair::from_pkcs8(der).map_err(|_| bad_key("not an RSA PKCS#8 key"))?;
        Ok(Self::new(Kind::Rsa(pair)))
    }

    /// ES256 key from PKCS#8 DER
    pub fn es256_pkcs8(der: &[u8]) -> Result<Self> {
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der, &SystemRandom::new())
                .map_err(|_| bad_key("not a P-256 PKCS#8 key"))?;
        Ok(Self::new(Kind::Ec(pair)))
    }

    fn new(kind: Kind) -> Self {
        Self {
            kind,
            kid: None,
            rng: SystemRandom::new(),
        }
    }

    /// Put `kid` in the header of issued tokens
    pub fn with_kid(mut self, kid: impl Into<String>) -> Self {
        self.kid = Some(kid.into());
        self
    }

    pub fn algorithm(&self) -> Algorithm {
        match self.kind {
            Kind::Rsa(_) => Algorithm::RS256,
            Kind::Ec(_) => Algorithm::ES256,
        }
    }

    /// The matching public key, to publish or to verify with
    pub fn verifying_key(&self) -> VerifyingKey {
        let (algorithm, public) = match &self.kind {
            Kind::Rsa(p) => (Algorithm::RS256, p.public_key().as_ref().to_vec()),
            Kind::Ec(p) => (Algorithm::ES256, p.public_key().as_ref().to_vec()),
        };
        VerifyingKey {
            algorithm,
            kid: self.kid.clone(),
            public,
        }
    }

    /// Sign `claims` as a token
    pub fn encode<T: Serialize>(&self, claims: &T) -> Result<String> {
        let mut header = serde_json::json!({ "alg": self.algorithm().name(), "typ": "JWT" });
        if let Some(kid) = &self.kid {
            header["kid"] = Value::String(kid.clone());
        }
        let payload = serde_json::to_vec(claims)
            .map_err(|e| Error::internal(format!("JWT claims: {}", e)))?;
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(payload)
        );
        let signature = match &self.kind {
            Kind::Rsa(pair) => {
                let mut sig = vec![0u8; pair.public().modulus_len()];
                pair.sign(&RSA_PKCS1_SHA256, &self.rng, input.as_bytes(), &mut sig)
                    .map_err(|_| Error::internal("JWT signing failed"))?;
                sig
            }
            Kind::Ec(pair) => pair
                .sign(&self.rng, input.as_bytes())
                .map_err(|_| Error::internal("JWT signing failed"))?
                .as_ref()
                .to_vec(),
        };
        Ok(format!("{}.{}", input, URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// Public keys a token may be verified with (a JWKS)
#[derive(Debug, Clone, Default)]
pub struct KeySet {
    keys: Arc<Vec<VerifyingKey>>,
}

impl KeySet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a key
    pub fn with(mut self, key: VerifyingKey) -> Self {
        Arc::make_mut(&mut self.keys).push(key);
        self
    }

    /// Parse a JWKS document (`{"keys": [...]}`). Keys of an unsupported type
    /// (other curves, `oct`) are skipped; it is an error if none remain.
    pub fn from_jwks(json: &str) -> Result<Self> {
        let doc: Value = serde_json::from_str(json).map_err(|_| bad_key("JWKS is not JSON"))?;
        let list = doc
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| bad_key("JWKS has no `keys` array"))?;
        let mut set = Self::new();
        for jwk in list {
            if let Ok(key) = VerifyingKey::from_jwk(jwk) {
                set = set.with(key);
            }
        }
        if set.keys.is_empty() {
            return Err(bad_key("JWKS contains no supported keys"));
        }
        Ok(set)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Verify signature, `exp` and `nbf` (and `validation`) and return the claims
    pub fn decode<T: DeserializeOwned>(&self, token: &str, validation: &Validation) -> Result<T> {
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h).map_err(|_| invalid())?)
                .map_err(|_| invalid())?;
        let algorithm = match header.get("alg").and_then(Value::as_str) {
            Some("RS256") => Algorithm::RS256,
            Some("ES256") => Algorithm::ES256,
            _ => return Err(invalid()),
        };
        let kid = header.get("kid").and_then(Value::as_str);
        let signature = URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid())?;
        let message = format!("{h}.{p}");

        let verified = self
            .keys
            .iter()
            .filter(|k| k.algorithm == algorithm)
            // a token naming a key must hit that key; keys without a kid match any token
            .filter(|k| match (kid, k.kid.as_deref()) {
                (Some(t), Some(k)) => t == k,
                _ => true,
            })
            .any(|k| k.verify(message.as_bytes(), &signature));
        if !verified {
            return Err(invalid());
        }

        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p).map_err(|_| invalid())?)
                .map_err(|_| invalid())?;
        check_claims(&claims, validation, now())?;
        serde_json::from_value(claims).map_err(|_| invalid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RSA_PKCS8: &[u8] = include_bytes!("../tests/fixtures/rsa_pkcs8.der");
    const RSA_PUBLIC: &[u8] = include_bytes!("../tests/fixtures/rsa_public.der");

    fn ec() -> SigningKey {
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .unwrap();
        SigningKey::es256_pkcs8(pkcs8.as_ref()).unwrap()
    }

    fn rsa() -> SigningKey {
        SigningKey::rs256_pkcs8(RSA_PKCS8).unwrap()
    }

    fn claims() -> Value {
        json!({ "sub": "ann", "exp": now() + 60 })
    }

    fn v() -> Validation {
        Validation::default()
    }

    #[test]
    fn es256_and_rs256_roundtrip() {
        for key in [ec(), rsa()] {
            let token = key.encode(&claims()).unwrap();
            let set = KeySet::new().with(key.verifying_key());
            let got: Value = set.decode(&token, &v()).unwrap();
            assert_eq!(got["sub"], "ann", "{:?}", key.algorithm());
        }
    }

    #[test]
    fn rsa_public_der_fixture_matches_the_private_key() {
        assert_eq!(rsa().verifying_key().public, RSA_PUBLIC);
    }

    #[test]
    fn rejects_tampering_wrong_key_and_malformed() {
        let key = ec();
        let token = key.encode(&claims()).unwrap();
        let set = KeySet::new().with(key.verifying_key());

        let other = KeySet::new().with(ec().verifying_key());
        assert!(other.decode::<Value>(&token, &v()).is_err());

        let mut parts: Vec<String> = token.split('.').map(String::from).collect();
        parts[1] = URL_SAFE_NO_PAD.encode(r#"{"sub":"root"}"#);
        assert!(set.decode::<Value>(&parts.join("."), &v()).is_err());

        for bad in ["", "a", "a.b", "a.b.c", "a.b.c.d", "!!!.!!!.!!!"] {
            assert_eq!(
                set.decode::<Value>(bad, &v()).unwrap_err().status_code(),
                401,
                "{bad}"
            );
        }
    }

    #[test]
    fn algorithm_confusion_is_rejected() {
        // an RS256 token must not verify against an ES256 key and vice versa
        let (e, r) = (ec(), rsa());
        let rs_token = r.encode(&claims()).unwrap();
        let es_token = e.encode(&claims()).unwrap();
        let only_ec = KeySet::new().with(e.verifying_key());
        let only_rsa = KeySet::new().with(r.verifying_key());
        assert!(only_ec.decode::<Value>(&rs_token, &v()).is_err());
        assert!(only_rsa.decode::<Value>(&es_token, &v()).is_err());

        // `none` and HS256 headers never reach a verifier
        let payload = URL_SAFE_NO_PAD.encode(r#"{"sub":"root"}"#);
        for alg in ["none", "HS256"] {
            let header = URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"{alg}"}}"#));
            let token = format!("{header}.{payload}.");
            assert!(only_ec.decode::<Value>(&token, &v()).is_err(), "{alg}");
        }
    }

    #[test]
    fn expired_tokens_and_claim_checks_apply() {
        let key = ec();
        let set = KeySet::new().with(key.verifying_key());
        let old = key.encode(&json!({ "exp": now() - 10 })).unwrap();
        assert_eq!(
            set.decode::<Value>(&old, &v()).unwrap_err().message(),
            "Token expired"
        );
        let strict = Validation {
            issuer: Some("me".into()),
            ..Default::default()
        };
        let token = key.encode(&json!({ "iss": "you" })).unwrap();
        assert!(set.decode::<Value>(&token, &strict).is_err());
        let token = key.encode(&json!({ "iss": "me" })).unwrap();
        assert!(set.decode::<Value>(&token, &strict).is_ok());
    }

    #[test]
    fn kid_selects_the_key() {
        let (a, b) = (ec().with_kid("a"), ec().with_kid("b"));
        let set = KeySet::new()
            .with(a.verifying_key())
            .with(b.verifying_key());
        assert!(set
            .decode::<Value>(&a.encode(&claims()).unwrap(), &v())
            .is_ok());
        assert!(set
            .decode::<Value>(&b.encode(&claims()).unwrap(), &v())
            .is_ok());

        // a token claiming kid "a" but signed by b's key must fail
        let forged = ec().with_kid("a").encode(&claims()).unwrap();
        assert!(set.decode::<Value>(&forged, &v()).is_err());
        assert_eq!(a.verifying_key().kid(), Some("a"));
    }

    #[test]
    fn jwks_with_rsa_and_ec_keys() {
        // build the JWKS for our own keys
        let rsa_key = rsa();
        let ec_key = ec().with_kid("ec1");
        let pub_ec = ec_key.verifying_key().public;
        let (n, e) = rsa_components(RSA_PUBLIC);
        let jwks = json!({ "keys": [
            { "kty": "oct", "k": "AAAA" },
            { "kty": "EC", "crv": "P-384", "x": "AA", "y": "AA" },
            { "kty": "RSA", "kid": "rsa1", "n": URL_SAFE_NO_PAD.encode(&n), "e": URL_SAFE_NO_PAD.encode(&e) },
            { "kty": "EC", "crv": "P-256", "kid": "ec1",
              "x": URL_SAFE_NO_PAD.encode(&pub_ec[1..33]),
              "y": URL_SAFE_NO_PAD.encode(&pub_ec[33..]) },
        ]});
        let set = KeySet::from_jwks(&jwks.to_string()).unwrap();
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());

        let rs_token = rsa_key.with_kid("rsa1").encode(&claims()).unwrap();
        assert!(set.decode::<Value>(&rs_token, &v()).is_ok());
        assert!(set
            .decode::<Value>(&ec_key.encode(&claims()).unwrap(), &v())
            .is_ok());
    }

    #[test]
    fn bad_jwks_and_keys_are_refused() {
        assert!(KeySet::from_jwks("not json").is_err());
        assert!(KeySet::from_jwks("{}").is_err());
        assert!(KeySet::from_jwks(r#"{"keys":[]}"#).is_err());
        assert!(KeySet::from_jwks(r#"{"keys":[{"kty":"oct","k":"AA"}]}"#).is_err());
        assert!(VerifyingKey::from_jwk(&json!({ "kty": "RSA", "n": "AA" })).is_err());
        assert!(VerifyingKey::from_jwk(
            &json!({ "kty": "EC", "crv": "P-256", "x": "AA", "y": "AA" })
        )
        .is_err());
        assert!(VerifyingKey::es256_point(&[4; 10]).is_err());
        assert!(SigningKey::rs256_pkcs8(b"junk").is_err());
        assert!(SigningKey::es256_pkcs8(b"junk").is_err());
        assert!(KeySet::new().is_empty());
    }

    #[test]
    fn der_integers_are_minimal_and_positive() {
        assert_eq!(der_integer(&[0, 0, 1]), [2, 1, 1]);
        assert_eq!(der_integer(&[0x80]), [2, 2, 0, 0x80]);
        assert_eq!(der_integer(&[]), [2, 1, 0]);
        assert_eq!(der_len(300), [0x82, 1, 44]);
        assert_eq!(der_len(200), [0x81, 200]);
    }

    /// (n, e) of a DER RSAPublicKey, to build test JWKs
    fn rsa_components(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        fn read(buf: &[u8]) -> (u8, &[u8], &[u8]) {
            let (tag, rest) = (buf[0], &buf[1..]);
            let (len, rest) = match rest[0] {
                n if n < 0x80 => (n as usize, &rest[1..]),
                0x81 => (rest[1] as usize, &rest[2..]),
                _ => (((rest[1] as usize) << 8) | rest[2] as usize, &rest[3..]),
            };
            (tag, &rest[..len], &rest[len..])
        }
        let (_, body, _) = read(der);
        let (_, n, rest) = read(body);
        let (_, e, _) = read(rest);
        let trim = |b: &[u8]| {
            b.iter()
                .skip_while(|&&x| x == 0)
                .copied()
                .collect::<Vec<_>>()
        };
        (trim(n), trim(e))
    }

    #[test]
    fn der_encoding_roundtrips_the_rsa_fixture() {
        let (n, e) = rsa_components(RSA_PUBLIC);
        let jwk =
            json!({ "kty": "RSA", "n": URL_SAFE_NO_PAD.encode(n), "e": URL_SAFE_NO_PAD.encode(e) });
        assert_eq!(VerifyingKey::from_jwk(&jwk).unwrap().public, RSA_PUBLIC);
    }
}

//! JSON Web Tokens: HS256 for tokens your app issues, RS256 for tokens from an identity
//! provider (Clerk, Auth0, Supabase Auth, Cognito...) checked against its JWKS keys.
//!
//! Verification picks the key by the token's `alg` and `kid`, so a token can't choose a weaker
//! algorithm than its key was made for: `none` is refused, and an HS256 token is never checked
//! against an RSA public key (the classic "alg confusion" attack). `exp` and `nbf` are enforced
//! with 60 seconds of clock leeway. Signatures come from `ring`, already compiled in.

use ring::{hmac, signature};
use serde_json::Value;

const LEEWAY_SECS: i64 = 60;

/// A key that can verify (and, for HS256, sign) tokens.
#[derive(Clone, Debug)]
pub enum Key {
    Hs256(Vec<u8>),
    /// An RSA public key from a JWKS: modulus and exponent, big-endian bytes.
    Rs256 {
        n: Vec<u8>,
        e: Vec<u8>,
        kid: Option<String>,
    },
}

impl Key {
    pub fn hs256(secret: impl AsRef<[u8]>) -> Key {
        Key::Hs256(secret.as_ref().to_vec())
    }
}

/// Why a token was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum JwtError {
    Malformed,
    /// `none`, or an algorithm no key here is for.
    Algorithm,
    Signature,
    Expired,
    NotYetValid,
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            JwtError::Malformed => "malformed token",
            JwtError::Algorithm => "unsupported algorithm",
            JwtError::Signature => "bad signature",
            JwtError::Expired => "token expired",
            JwtError::NotYetValid => "token not valid yet",
        })
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url without padding.
pub fn b64url(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 4 / 3 + 3);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            s.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    s
}

/// Decode base64url (padding and the standard alphabet's `+/` are accepted too).
pub fn unb64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'+' => 62,
            b'/' => 63,
            _ => B64.iter().position(|x| *x == c)? as u32,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// A signed HS256 token for `claims` (add `exp` yourself).
pub fn sign_hs256(claims: &Value, secret: &[u8]) -> String {
    let input = format!("{}.{}", b64url(br#"{"alg":"HS256","typ":"JWT"}"#), b64url(claims.to_string().as_bytes()));
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), input.as_bytes());
    format!("{input}.{}", b64url(tag.as_ref()))
}

/// The claims of a valid token, checked against `keys` at unix time `now`.
pub fn verify(token: &str, keys: &[Key], now: i64) -> Result<Value, JwtError> {
    let mut parts = token.trim().split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(JwtError::Malformed);
    };
    let header: Value = unb64url(h).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(JwtError::Malformed)?;
    let sig = unb64url(s).ok_or(JwtError::Malformed)?;
    let input = &token.trim()[..h.len() + 1 + p.len()];
    let kid = header["kid"].as_str();
    let ok = match header["alg"].as_str() {
        Some("HS256") => keys.iter().any(|k| match k {
            Key::Hs256(secret) => hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, secret), input.as_bytes(), &sig).is_ok(),
            _ => false,
        }),
        Some("RS256") => keys.iter().any(|k| match k {
            Key::Rs256 { n, e, kid: key_id } if kid.is_none() || key_id.is_none() || key_id.as_deref() == kid => {
                signature::RsaPublicKeyComponents { n, e }.verify(&signature::RSA_PKCS1_2048_8192_SHA256, input.as_bytes(), &sig).is_ok()
            }
            _ => false,
        }),
        _ => return Err(JwtError::Algorithm),
    };
    let has_key =
        keys.iter().any(|k| matches!((k, header["alg"].as_str()), (Key::Hs256(_), Some("HS256")) | (Key::Rs256 { .. }, Some("RS256"))));
    if !has_key {
        return Err(JwtError::Algorithm);
    }
    if !ok {
        return Err(JwtError::Signature);
    }
    let claims: Value = unb64url(p).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(JwtError::Malformed)?;
    if !claims.is_object() {
        return Err(JwtError::Malformed);
    }
    if claims["exp"].as_f64().is_some_and(|exp| (now - LEEWAY_SECS) as f64 >= exp) {
        return Err(JwtError::Expired);
    }
    if claims["nbf"].as_f64().is_some_and(|nbf| ((now + LEEWAY_SECS) as f64) < nbf) {
        return Err(JwtError::NotYetValid);
    }
    Ok(claims)
}

/// The keys in a JWKS document (`{"keys": [...]}`, as served at `/.well-known/jwks.json`).
/// RSA signing keys and shared secrets are kept; others (encryption keys, EC) are skipped.
pub fn keys_from_jwks(jwks: &Value) -> Vec<Key> {
    let field = |k: &Value, name: &str| k[name].as_str().and_then(unb64url);
    jwks["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|k| k["use"].as_str().is_none_or(|u| u == "sig"))
        .filter_map(|k| match k["kty"].as_str()? {
            "RSA" => Some(Key::Rs256 { n: field(k, "n")?, e: field(k, "e")?, kid: k["kid"].as_str().map(str::to_string) }),
            "oct" => Some(Key::Hs256(field(k, "k")?)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // RFC 7515 appendix A.1 and A.2: the same payload (expired in 2011) under HS256 and RS256.
    const HS256_TOKEN: &str = "eyJ0eXAiOiJKV1QiLA0KICJhbGciOiJIUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const HS256_KEY: &str = "AyM1SysPpbyDfgZld3umj1qzKObwVMkoqQ-EstJQLr_T-1qS0gZH75aKtMN3Yj0iPS4hcgUuTwjAzZr1Z9CAow";
    const RS256_TOKEN: &str = "eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.cC4hiUPoj9Eetdgtv3hF80EGrhuB__dzERat0XF9g2VtQgr9PJbu3XOiZj5RZmh7AAuHIm4Bh-0Qc_lF5YKt_O8W2Fp5jujGbds9uJdbF9CUAr7t1dnZcAcQjbKBYNX4BAynRFdiuB--f_nZLgrnbyTyWzO75vRK5h6xBArLIARNPvkSjtQBMHlb1L07Qe7K0GarZRmB_eSN9383LcOLn6_dO--xi12jzDwusC-eOkHWEsqtFZESc6BfI7noOPqvhJ1phCnvWh6IeYI2w9QOYEUipUTI8np6LbgGY9Fs98rqVt5AXLIhWkWywlVmtVrBp0igcN_IoypGlUPQGe77Rw";
    const RS256_N: &str = "ofgWCuLjybRlzo0tZWJjNiuSfb4p4fAkd_wWJcyQoTbji9k0l8W26mPddxHmfHQp-Vaw-4qPCJrcS2mJPMEzP1Pt0Bm4d4QlL-yRT-SFd2lZS-pCgNMsD1W_YpRPEwOWvG6b32690r2jZ47soMZo9wGzjb_7OMg0LOL-bSf63kpaSHSXndS5z5rexMdbBYUsLA9e-KXBdQOS-UTo7WTBEMa2R2CapHg665xsmtdVMTBQY4uDZlxvb3qCo5ZwKh9kG4LT6_I5IhlJH7aGhyxXFvUK-DWNmoudF8NAco9_h9iaGNj8q2ethFkMLs91kzk2PAcDTW9gb54h4FRWyuXpoQ";
    const BEFORE_EXP: i64 = 1_300_819_000;

    fn rsa_jwks() -> Vec<Key> {
        keys_from_jwks(&json!({ "keys": [{ "kty": "RSA", "kid": "k1", "use": "sig", "n": RS256_N, "e": "AQAB" }] }))
    }

    #[test]
    fn rfc_7515_vectors() {
        let hs = [Key::Hs256(unb64url(HS256_KEY).unwrap())];
        let claims = verify(HS256_TOKEN, &hs, BEFORE_EXP).unwrap();
        assert_eq!((claims["iss"].as_str(), claims["http://example.com/is_root"].as_bool()), (Some("joe"), Some(true)));
        assert_eq!(verify(RS256_TOKEN, &rsa_jwks(), BEFORE_EXP).unwrap()["iss"], "joe");
        assert_eq!(verify(HS256_TOKEN, &hs, 1_400_000_000), Err(JwtError::Expired));
    }

    #[test]
    fn refuses_tampering_and_algorithm_tricks() {
        let hs = [Key::hs256("secret")];
        let good = sign_hs256(&json!({ "sub": "7", "exp": BEFORE_EXP + 900 }), b"secret");
        assert_eq!(verify(&good, &hs, BEFORE_EXP).unwrap()["sub"], "7");
        assert_eq!(verify(&good, &[Key::hs256("other")], BEFORE_EXP), Err(JwtError::Signature));
        let (h, rest) = good.split_once('.').unwrap();
        let forged = format!("{h}.{}.{}", b64url(br#"{"sub":"1"}"#), rest.split_once('.').unwrap().1);
        assert_eq!(verify(&forged, &hs, BEFORE_EXP), Err(JwtError::Signature), "a changed payload");
        let unsigned = format!("{}.{}.", b64url(br#"{"alg":"none"}"#), b64url(br#"{"sub":"1"}"#));
        assert_eq!(verify(&unsigned, &hs, BEFORE_EXP), Err(JwtError::Algorithm), "alg none");
        // Alg confusion: an HS256 token "signed" with the RSA public key as the secret.
        let confused = sign_hs256(&json!({ "sub": "1" }), &unb64url(RS256_N).unwrap());
        assert_eq!(verify(&confused, &rsa_jwks(), BEFORE_EXP), Err(JwtError::Algorithm));
        assert_eq!(verify(RS256_TOKEN, &hs, BEFORE_EXP), Err(JwtError::Algorithm), "RS256 with only HMAC keys");
        let early = sign_hs256(&json!({ "nbf": BEFORE_EXP + 3600 }), b"secret");
        assert_eq!(verify(&early, &hs, BEFORE_EXP), Err(JwtError::NotYetValid));
        for junk in ["", "a.b", "a.b.c.d", "!!.!!.!!"] {
            assert!(verify(junk, &hs, BEFORE_EXP).is_err());
        }
    }

    #[test]
    fn kid_selects_the_key() {
        let wrong_kid = keys_from_jwks(&json!({ "keys": [{ "kty": "RSA", "kid": "other", "n": RS256_N, "e": "AQAB" }] }));
        let token_with_kid = |kid: &str| {
            let (_, rest) = RS256_TOKEN.split_once('.').unwrap();
            format!("{}.{rest}", b64url(format!(r#"{{"alg":"RS256","kid":"{kid}"}}"#).as_bytes()))
        };
        // Changing the header invalidates the signature, so this only shows the kid filter runs.
        assert_eq!(verify(&token_with_kid("k1"), &wrong_kid, BEFORE_EXP), Err(JwtError::Signature));
        assert!(keys_from_jwks(&json!({ "keys": [{ "kty": "RSA", "use": "enc", "n": RS256_N, "e": "AQAB" }] })).is_empty());
    }

    #[test]
    fn base64url_round_trips() {
        for n in 0..40u8 {
            let bytes: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(n)).collect();
            assert_eq!(unb64url(&b64url(&bytes)).unwrap(), bytes);
        }
        assert_eq!(b64url(b"\xfb\xff"), "-_8");
    }
}

// ---------------------------------------------------------------- signing requests in

/// Which user a token's claim names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The claim is the user's id (tokens your app issues with [`JwtAuth::issue`]).
    Id,
    /// The claim is a username (map an identity provider's `email` or `sub` onto your users).
    Username,
}

/// Sign requests in with JWTs: `App::new().jwt(JwtAuth::hs256(secret))`. A valid token in
/// `Authorization: Bearer` (or in `cookie`, if set) makes its user the current user, with
/// that user's ordinary permissions; the admin still needs a password sign-in.
#[derive(Clone, Debug)]
pub struct JwtAuth {
    pub(crate) keys: Vec<Key>,
    pub(crate) cookie: Option<String>,
    pub(crate) claim: String,
    pub(crate) lookup: Lookup,
}

impl JwtAuth {
    /// Tokens your app issues: HS256 with `secret` (32+ random bytes), `sub` = user id.
    pub fn hs256(secret: impl AsRef<[u8]>) -> Self {
        JwtAuth { keys: vec![Key::hs256(secret)], cookie: None, claim: "sub".into(), lookup: Lookup::Id }
    }
    /// Tokens from an identity provider, checked against its JWKS document; the `claim`
    /// (often `email`) is matched to a username.
    pub fn jwks(jwks: &Value, claim: &str) -> Self {
        JwtAuth { keys: keys_from_jwks(jwks), cookie: None, claim: claim.into(), lookup: Lookup::Username }
    }
    /// Also read the token from this cookie (for browsers; keep it `HttpOnly`).
    pub fn cookie(mut self, name: &str) -> Self {
        self.cookie = Some(name.into());
        self
    }
    /// A token for `user` that lasts `ttl` (HS256 configurations only).
    pub fn issue(&self, user: &crate::auth::User, ttl: std::time::Duration) -> Option<String> {
        let Key::Hs256(secret) = self.keys.iter().find(|k| matches!(k, Key::Hs256(_)))? else { return None };
        let now = crate::auth::now();
        let claims = serde_json::json!({ "sub": user.id?.to_string(), "iat": now, "exp": now + ttl.as_secs() as i64 });
        Some(sign_hs256(&claims, secret))
    }

    /// The active user a token names, or why not.
    pub async fn user(&self, token: &str) -> crate::Result<Option<crate::auth::User>> {
        use crate::auth::User;
        use crate::orm::Model;
        let Ok(claims) = verify(token, &self.keys, crate::auth::now()) else { return Ok(None) };
        let value = match &claims[self.claim.as_str()] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return Ok(None),
        };
        let user = match self.lookup {
            Lookup::Id => match value.parse::<i64>() {
                Ok(id) => User::objects().filter(User::ID.eq(id)).first().await?,
                Err(_) => None,
            },
            Lookup::Username => User::objects().filter(User::USERNAME.eq(value)).first().await?,
        };
        Ok(user.filter(|u| u.is_active))
    }
}

pub(crate) static AUTH: std::sync::OnceLock<JwtAuth> = std::sync::OnceLock::new();

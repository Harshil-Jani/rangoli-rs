//! Users, database-backed sessions and login throttling.
//!
//! No SECRET_KEY exists anywhere: session keys are random and live in the
//! database, so there is nothing to sign and nothing to leak.

use crate::orm::Model;
use crate::{Error, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_user", display = "username")]
pub struct User {
    pub id: Option<i64>,
    #[field(max_length = 150, unique)]
    pub username: String,
    #[field(password)]
    pub password: String,
    pub is_active: bool,
    pub is_staff: bool,
    pub is_superuser: bool,
}

#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_session")]
pub struct Session {
    pub id: Option<i64>,
    #[field(max_length = 64, unique)]
    pub key: String,
    #[field(fk = User, cascade)]
    pub user_id: Option<i64>,
    pub expires_at: i64,
}

pub const COOKIE: &str = "rangoli_sid";
const SESSION_SECS: i64 = 14 * 24 * 3600;
const MAX_FAILURES: u32 = 5;
const LOCKOUT_SECS: i64 = 15 * 60;

pub(crate) fn now() -> i64 {
    // A clock set before 1970 reads as 0 rather than panicking on every request.
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub(crate) fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| Error::Config(format!("OS random number generator unavailable: {e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Argon2id hash, computed off the async runtime.
pub async fn hash_password(raw: &str) -> Result<String> {
    let raw = raw.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| Error::Config(format!("OS random number generator unavailable: {e}")))?;
        let salt = SaltString::encode_b64(&salt).map_err(|e| Error::Config(e.to_string()))?;
        Argon2::default().hash_password(raw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| Error::Config(e.to_string()))
    })
    .await
    .map_err(|e| Error::Config(format!("password hashing failed: {e}")))?
}

pub async fn verify_password(raw: &str, hash: &str) -> bool {
    let (raw, hash) = (raw.to_owned(), hash.to_owned());
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).is_ok_and(|h| Argon2::default().verify_password(raw.as_bytes(), &h).is_ok())
    })
    .await
    .unwrap_or(false)
}

/// A poisoned lock (a panic elsewhere while it was held) must not lock everyone out forever.
fn failures() -> std::sync::MutexGuard<'static, HashMap<String, (u32, i64)>> {
    FAILURES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ponytail: per-process lockout map; move to the database or Redis when running several instances.
static FAILURES: LazyLock<Mutex<HashMap<String, (u32, i64)>>> = LazyLock::new(Default::default);

/// Check credentials. Locks a username for 15 minutes after 5 failures
/// (Django has no built-in brute-force protection).
pub async fn authenticate(username: &str, password: &str) -> Result<Option<User>> {
    if let Some((n, until)) = failures().get(username) {
        if *n >= MAX_FAILURES && *until > now() {
            return Err(Error::Locked);
        }
    }
    let user = User::objects().filter(User::USERNAME.eq(username)).first().await?;
    // Hash even for unknown users so response time doesn't reveal which usernames exist.
    static DUMMY: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    let dummy = DUMMY.get_or_init(|| async { hash_password("rangoli-timing-pad").await.unwrap_or_default() }).await;
    let ok = verify_password(password, user.as_ref().map_or(dummy, |u| &u.password)).await;
    let mut failures = failures();
    match user {
        Some(u) if ok && u.is_active => {
            failures.remove(username);
            Ok(Some(u))
        }
        _ => {
            let e = failures.entry(username.to_owned()).or_insert((0, 0));
            e.0 = if e.1 > now() { e.0 + 1 } else { 1 }; // failures count within a sliding 15 minute window
            e.1 = now() + LOCKOUT_SECS;
            Ok(None)
        }
    }
}

pub async fn create_user(username: &str, password: &str, staff: bool) -> Result<User> {
    let mut u = User {
        id: None,
        username: username.into(),
        password: hash_password(password).await?,
        is_active: true,
        is_staff: staff,
        is_superuser: staff,
    };
    u.save().await?;
    Ok(u)
}

fn cookie(value: &str, max_age: i64) -> String {
    let secure = if crate::settings().debug { "" } else { "; Secure" };
    format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
}

/// Start a fresh session for `user`; returns the `Set-Cookie` header value.
/// A new key every login prevents session fixation.
pub async fn login(user: &User) -> Result<String> {
    Session::objects().filter(Session::EXPIRES_AT.lt(now())).delete().await?;
    let mut s = Session { id: None, key: random_hex(32)?, user_id: user.id, expires_at: now() + SESSION_SECS };
    s.save().await?;
    Ok(cookie(&s.key, SESSION_SECS))
}

/// End the session in `cookie_header`; returns a `Set-Cookie` that clears it.
pub async fn logout(cookie_header: Option<&str>) -> Result<String> {
    if let Some(key) = cookie_header.and_then(session_key) {
        Session::objects().filter(Session::KEY.eq(key)).delete().await?;
    }
    Ok(cookie("", 0))
}

pub(crate) fn session_key(header: &str) -> Option<&str> {
    header.split(';').find_map(|kv| kv.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// The logged-in user, if any. Use as an axum extractor.
#[derive(Clone, Debug, Default)]
pub struct CurrentUser(pub Option<User>);

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> std::result::Result<Self, Self::Rejection> {
        Ok(parts.extensions.get::<CurrentUser>().cloned().unwrap_or_default())
    }
}

/// Middleware: resolve the session cookie to a `CurrentUser`.
pub async fn session_middleware(mut req: Request, next: Next) -> Response {
    let key = req.headers().get(axum::http::header::COOKIE).and_then(|v| v.to_str().ok()).and_then(session_key).map(str::to_owned);
    let mut user = None;
    if let Some(key) = key {
        let found = Session::objects().filter(Session::KEY.eq(key) & Session::EXPIRES_AT.gt(now())).first().await;
        if let Ok(Some(Session { user_id: Some(uid), .. })) = found {
            user = User::get(uid).await.ok().filter(|u| u.is_active);
        }
    }
    req.extensions_mut().insert(CurrentUser(user));
    next.run(req).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_session_cookie_among_others() {
        assert_eq!(super::session_key("a=1; rangoli_sid=abc; b=2"), Some("abc"));
        assert_eq!(super::session_key("rangoli_sidx=abc"), None);
    }
}

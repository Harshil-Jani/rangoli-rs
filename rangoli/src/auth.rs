//! Users, database-backed sessions, API tokens and login throttling.
//!
//! No SECRET_KEY exists anywhere: session keys are random and live in the
//! database, so there is nothing to sign and nothing to leak.

use crate::orm::Model;
use crate::{DateTime, Error, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
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

/// A bearer token for non-browser API clients (DRF's `authtoken`, but only a hash is stored).
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_api_token", display = "name")]
pub struct ApiToken {
    pub id: Option<i64>,
    #[field(max_length = 100)]
    pub name: String,
    /// The first characters of the token (`rgl_1a2b3c4d`), to tell tokens apart; not a secret.
    #[field(max_length = 12)]
    pub prefix: String,
    /// SHA-256 of the token. The token itself is shown once, at creation, and never stored.
    #[field(max_length = 64, unique)]
    pub key_hash: String,
    #[field(fk = User, cascade)]
    pub user_id: i64,
    #[field(auto_now_add)]
    pub created_at: DateTime,
    pub last_used_at: Option<DateTime>,
    pub expires_at: Option<DateTime>,
    /// Reads only: any create, update or delete is refused (403), over the API and MCP alike.
    pub read_only: bool,
    /// Tables this token may touch, comma-separated (`blog_post,blog_author`); empty or none means
    /// all its user can reach. Others look like they don't exist (404, and no MCP tools).
    #[field(text)]
    pub tables: Option<String>,
    /// Writes wait in the admin's Pending changes until a person approves them.
    pub needs_approval: bool,
}

/// What a token may do beyond its user's own rights. Set on bearer requests only; sessions
/// (people in a browser) have no scope.
#[derive(Clone, Debug, Default)]
pub struct TokenScope {
    pub read_only: bool,
    pub tables: Option<Vec<String>>,
    /// Writes become pending changes for a person to approve in the admin.
    pub needs_approval: bool,
    /// The token's name, shown to reviewers ("Claude on my laptop").
    pub token: String,
}

impl TokenScope {
    fn of(t: &ApiToken) -> Self {
        let tables = t
            .tables
            .as_deref()
            .map(|s| s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect::<Vec<_>>())
            .filter(|v| !v.is_empty());
        TokenScope { read_only: t.read_only, tables, needs_approval: t.needs_approval, token: t.name.clone() }
    }

    /// May this token see `table` at all?
    pub fn allows_table(&self, table: &str) -> bool {
        self.tables.as_ref().is_none_or(|ts| ts.iter().any(|t| t == table))
    }
}

/// Every token starts with this, so secret scanners (and people) can recognise a leaked one.
pub const TOKEN_PREFIX: &str = "rgl_";
/// `last_used_at` is written at most this often per token, not on every request.
const TOKEN_TOUCH_SECS: i64 = 60;

fn token_digest(token: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Create a token for `user` with all of that user's rights. Returns the stored row and the
/// token, which is never retrievable again. `valid_for: None` never expires; revoke by deleting the row.
pub async fn create_token(user: &User, name: &str, valid_for: Option<std::time::Duration>) -> Result<(ApiToken, String)> {
    create_scoped_token(user, name, valid_for, &TokenScope::default()).await
}

/// `create_token`, narrowed: read-only, and/or limited to some tables. What an AI agent should get.
pub async fn create_scoped_token(
    user: &User,
    name: &str,
    valid_for: Option<std::time::Duration>,
    scope: &TokenScope,
) -> Result<(ApiToken, String)> {
    let token = format!("{TOKEN_PREFIX}{}", random_hex(20)?); // 160 random bits
    let mut row = ApiToken {
        id: None,
        name: name.into(),
        prefix: token[..12].into(),
        key_hash: token_digest(&token),
        user_id: user.id.ok_or(Error::NotFound)?,
        created_at: DateTime::now(),
        last_used_at: None,
        expires_at: valid_for.map(|d| DateTime::from_unix(now() + d.as_secs() as i64)),
        read_only: scope.read_only,
        tables: scope.tables.as_ref().map(|t| t.join(",")),
        needs_approval: scope.needs_approval,
    };
    row.save().await?;
    Ok((row, token))
}

/// The active user a token belongs to, with the token's scope; `None` for unknown, expired or
/// malformed tokens.
pub async fn token_user(token: &str) -> Result<Option<(User, TokenScope)>> {
    if !token.starts_with(TOKEN_PREFIX) || token.len() != TOKEN_PREFIX.len() + 40 {
        return Ok(None); // not ours: no query
    }
    let live = ApiToken::KEY_HASH.eq(token_digest(token)) & (ApiToken::EXPIRES_AT.is_null() | ApiToken::EXPIRES_AT.gt(DateTime::now()));
    let Some(mut row) = ApiToken::objects().filter(live).first().await? else { return Ok(None) };
    if row.last_used_at.is_none_or(|t| t.unix() < now() - TOKEN_TOUCH_SECS) {
        row.last_used_at = Some(DateTime::now());
        ApiToken::objects().filter(ApiToken::ID.eq(row.id.unwrap_or(-1))).update([ApiToken::LAST_USED_AT.set(DateTime::now())]).await?;
    }
    let user = User::objects().filter(User::ID.eq(row.user_id) & User::IS_ACTIVE.eq(true)).first().await?;
    Ok(user.map(|u| (u, TokenScope::of(&row))))
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

/// Failed logins per username, in the database so every instance shares one count
/// (a per-process count gives an attacker 5 guesses per instance, and a restart resets it).
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_login_failure")]
pub struct LoginFailure {
    pub id: Option<i64>,
    #[field(max_length = 150, unique)]
    pub username: String,
    pub failures: i64,
    /// Unix seconds when the window closes; failures before it count towards a lockout.
    pub locked_until: i64,
}

/// The key a username's failures are counted under. Real usernames fit the column (150
/// characters); a longer attempt can't match an account but is still counted, by its prefix.
fn failure_key(username: &str) -> String {
    username.chars().take(150).collect()
}

/// Count one more login attempt for `username` and return the count in the current window.
/// The increment is one atomic UPDATE (the first attempt inserts the row), so concurrent
/// attempts on any number of instances can't lose a count. The re-read can only see more
/// attempts than this one's, never fewer, so a race errs on the side of locking.
async fn record_attempt(username: &str) -> Result<i64> {
    let username = &failure_key(username);
    count_attempt(username).await?;
    let row = LoginFailure::objects().filter(LoginFailure::USERNAME.eq(username.as_str())).first().await?;
    Ok(row.map_or(i64::from(MAX_FAILURES) + 1, |r| r.failures)) // vanished: a racing success cleared it; fail closed
}

async fn count_attempt(username: &str) -> Result<()> {
    let d = crate::orm::db().dialect;
    let (t, f, u, n) = (d.quote(LoginFailure::TABLE), d.quote("failures"), d.quote("locked_until"), d.quote("username"));
    let sql = format!(
        "UPDATE {t} SET {f} = CASE WHEN {u} > {p1} THEN {f} + 1 ELSE 1 END, {u} = {p2} WHERE {n} = {p3}",
        p1 = d.ph(1),
        p2 = d.ph(2),
        p3 = d.ph(3)
    );
    let int = |v: i64| (crate::orm::Value::Int(v), crate::orm::FieldType::Int);
    let params = vec![int(now()), int(now() + LOCKOUT_SECS), (crate::orm::Value::Text(username.into()), crate::orm::FieldType::Text)];
    let mut last = None;
    for attempt in 0..5u64 {
        let updated = match crate::orm::execute(&sql, &params).await {
            Ok(r) => r.rows_affected() > 0,
            Err(e) if e.is_retryable() => {
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(5 * (attempt + 1))).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        if updated {
            return Ok(());
        }
        let mut row = LoginFailure { id: None, username: username.into(), failures: 1, locked_until: now() + LOCKOUT_SECS };
        match row.save().await {
            Ok(()) => return Ok(()),
            // Another instance inserted it first, or InnoDB picked this insert as a deadlock victim: go again.
            Err(e) if e.is_unique_violation() || e.is_retryable() => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or(Error::Locked))
}

/// Check credentials. Locks a username for 15 minutes after 5 failures
/// (Django has no built-in brute-force protection).
pub async fn authenticate(username: &str, password: &str) -> Result<Option<User>> {
    // No stored username contains NUL (and Postgres can't even compare one): a plain failure.
    if username.contains('\0') {
        return Ok(None);
    }
    // Count the attempt before checking the password, so a burst of parallel guesses can't all
    // pass a check that only sees failures recorded after the (slow) verification.
    if record_attempt(username).await? > i64::from(MAX_FAILURES) {
        return Err(Error::Locked);
    }
    let user = User::objects().filter(User::USERNAME.eq(username)).first().await?;
    // Hash even for unknown users so response time doesn't reveal which usernames exist.
    static DUMMY: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    let dummy = DUMMY.get_or_init(|| async { hash_password("rangoli-timing-pad").await.unwrap_or_default() }).await;
    let ok = verify_password(password, user.as_ref().map_or(dummy, |u| &u.password)).await;
    match user {
        Some(u) if ok && u.is_active => {
            LoginFailure::objects().filter(LoginFailure::USERNAME.eq(failure_key(username))).delete().await?;
            Ok(Some(u))
        }
        _ => {
            // Already counted above; unknown usernames count too, so a lockout reveals nothing.
            // Sweep expired rows. Best effort: on MySQL this range DELETE can lose a deadlock to a
            // concurrent failure, which is harmless (the next sweep gets them), so errors are ignored.
            // ponytail: swept on failures only; a background task if the table ever matters.
            let _ = LoginFailure::objects().filter(LoginFailure::LOCKED_UNTIL.lt(now())).delete().await;
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

/// Middleware: resolve `Authorization: Bearer rgl_…` or the session cookie to a `CurrentUser`.
/// A Rangoli token that doesn't check out is a 401, like DRF. Other bearer tokens (an app's own
/// JWT or OAuth, a webhook's secret) are left alone for the app's handlers to read.
pub async fn session_middleware(mut req: Request, next: Next) -> Response {
    use axum::response::IntoResponse;
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| t.trim().starts_with(TOKEN_PREFIX));
    if let Some(token) = bearer {
        // Tokens are for programs and agents. The admin is for people, signed in with a password.
        if req.uri().path().starts_with("/admin") {
            let body = axum::Json(serde_json::json!({ "detail": "API tokens can't open the admin." }));
            return (axum::http::StatusCode::FORBIDDEN, body).into_response();
        }
        let (user, scope) = match token_user(token.trim()).await {
            Ok(Some(found)) => found,
            Ok(None) => {
                let body = axum::Json(serde_json::json!({ "detail": "Invalid token." }));
                return (axum::http::StatusCode::UNAUTHORIZED, [(axum::http::header::WWW_AUTHENTICATE, "Bearer")], body).into_response();
            }
            Err(e) => return e.into_response(), // the database is down: 503, not "your token is bad"
        };
        req.extensions_mut().insert(CurrentUser(Some(user)));
        req.extensions_mut().insert(scope);
        return next.run(req).await;
    }
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

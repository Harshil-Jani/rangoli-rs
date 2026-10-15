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

/// Staff permissions, granted to the users in a group when `App::admin_permissions()` is on.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_group", display = "name")]
pub struct Group {
    pub id: Option<i64>,
    #[field(max_length = 150, unique)]
    pub name: String,
    /// One per line: `blog_post.view`, `blog_post.add`, `blog_post.change`, `blog_post.delete`,
    /// `blog_post.*` for all four, or `*.view` to see every model.
    #[field(text)]
    pub permissions: String,
}

#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_user", display = "username", m2m(groups = Group))]
pub struct User {
    pub id: Option<i64>,
    #[field(max_length = 150, unique)]
    pub username: String,
    #[field(password)]
    pub password: String,
    pub is_active: bool,
    pub is_staff: bool,
    pub is_superuser: bool,
    /// Two-factor sign-in key (base32), once turned on in the admin. Never leaves the server.
    #[field(max_length = 64, private)]
    pub totp_secret: Option<String>,
    /// The newest code step used, so a code works only once.
    pub totp_last_step: i64,
    /// Where password reset links go.
    #[field(max_length = 254)]
    pub email: Option<String>,
}

/// A password reset link, stored as a hash: valid for an hour, used once.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_password_reset")]
pub struct PasswordReset {
    pub id: Option<i64>,
    #[field(max_length = 64, unique)]
    pub token_hash: String,
    #[field(fk = User, cascade)]
    pub user_id: i64,
    pub created_at: DateTime,
    pub expires_at: DateTime,
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
    /// At most this many creates, updates and deletes per UTC day; then 429 until midnight.
    pub max_writes_per_day: Option<i64>,
    /// The UTC day (days since 1970) `writes_count` belongs to.
    pub writes_day: i64,
    pub writes_count: i64,
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
    /// The token's row, and its daily write budget if it has one.
    pub token_id: Option<i64>,
    pub max_writes_per_day: Option<i64>,
}

/// A site-wide switch kept in the database, so every instance sees it at once.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_setting")]
pub struct Setting {
    pub id: Option<i64>,
    #[field(max_length = 100)]
    pub name: String,
    #[field(text)]
    pub value: String,
}

const AGENTS_PAUSED: &str = "agents_paused";

/// Is the admin's "Pause all agents" on? While it is, no token (API or MCP) can change anything.
pub async fn agents_paused() -> Result<bool> {
    use crate::orm::Model;
    Ok(Setting::objects().filter(Setting::NAME.eq(AGENTS_PAUSED)).filter(Setting::VALUE.eq("1")).count().await? > 0)
}

pub async fn pause_agents(paused: bool) -> Result<()> {
    use crate::orm::Model;
    Setting::objects().filter(Setting::NAME.eq(AGENTS_PAUSED)).delete().await?;
    Setting { id: None, name: AGENTS_PAUSED.into(), value: if paused { "1" } else { "0" }.into() }.save().await
}

/// Count one write against the token's daily budget; false when it's used up. One conditional
/// UPDATE, so concurrent requests can't overshoot it, and a new UTC day starts from zero.
pub(crate) async fn spend_write_budget(scope: &TokenScope) -> Result<bool> {
    let (Some(id), Some(max)) = (scope.token_id, scope.max_writes_per_day) else { return Ok(true) };
    let d = crate::orm::db().dialect;
    let today = now().div_euclid(86_400);
    let (t, day, count) = (d.quote("rangoli_api_token"), d.quote("writes_day"), d.quote("writes_count"));
    // MySQL assigns left to right, so the count (which reads the old day) is set first.
    let sql = format!(
        "UPDATE {t} SET {count} = CASE WHEN {day} = {p1} THEN {count} + 1 ELSE 1 END, {day} = {p2} WHERE {id} = {p3} AND ({day} <> {p4} OR {count} < {p5})",
        id = d.quote("id"),
        p1 = d.ph(1),
        p2 = d.ph(2),
        p3 = d.ph(3),
        p4 = d.ph(4),
        p5 = d.ph(5)
    );
    use crate::orm::{FieldType, Value};
    let int = |v: i64| (Value::Int(v), FieldType::Int);
    let params = vec![int(today), int(today), int(id), int(today), int(max)];
    Ok(crate::orm::execute(&sql, &params).await?.rows_affected() == 1)
}

impl TokenScope {
    fn of(t: &ApiToken) -> Self {
        let tables = t
            .tables
            .as_deref()
            .map(|s| s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect::<Vec<_>>())
            .filter(|v| !v.is_empty());
        TokenScope {
            read_only: t.read_only,
            tables,
            needs_approval: t.needs_approval,
            token: t.name.clone(),
            token_id: t.id,
            max_writes_per_day: t.max_writes_per_day,
        }
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
        max_writes_per_day: scope.max_writes_per_day,
        writes_day: 0,
        writes_count: 0,
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

/// A user another system vouched for (Django's `auth_user`), mirrored into `rangoli_user` at login.
#[derive(Clone, Debug)]
pub struct ExternalUser {
    pub username: String,
    pub is_staff: bool,
    pub is_superuser: bool,
    pub is_active: bool,
}

type ExternalLogin = fn(String, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<ExternalUser>>> + Send>>;
static EXTERNAL_LOGIN: std::sync::OnceLock<ExternalLogin> = std::sync::OnceLock::new();

/// Check passwords against another user store first (the Django bridge sets this to Django's
/// `auth_user`). A match signs in as a mirrored Rangoli user; lockouts still apply.
pub fn set_external_login(f: ExternalLogin) {
    let _ = EXTERNAL_LOGIN.set(f);
}

/// The Rangoli user mirroring an external one: created or updated to match, with an unusable
/// password, so only the external store can let them in.
async fn mirror(ext: ExternalUser) -> Result<User> {
    let mut u = match User::objects().filter(User::USERNAME.eq(ext.username.as_str())).first().await? {
        Some(u) => u,
        None => User {
            id: None,
            username: ext.username.clone(),
            password: "!".into(),
            is_active: true,
            is_staff: false,
            is_superuser: false,
            totp_secret: None,
            totp_last_step: 0,
            email: None,
        },
    };
    (u.is_active, u.is_staff, u.is_superuser) = (ext.is_active, ext.is_staff, ext.is_superuser);
    u.save().await?;
    Ok(u)
}

/// Check credentials. Locks a username for 15 minutes after 5 failures
/// (Django has no built-in brute-force protection).
/// Check a username and password. For an account with two-factor sign-in on, use
/// `authenticate_with_code`; this one answers `Err(CodeRequired)` for it.
pub async fn authenticate(username: &str, password: &str) -> Result<Option<User>> {
    authenticate_with_code(username, password, None).await
}

/// The second factor, checked before the failure count is cleared: with only the password
/// right, a missing code is `Err(CodeRequired)` and a wrong or reused one a failed login, so
/// knowing the password doesn't let anyone guess codes past the lockout.
async fn second_factor(u: User, code: Option<&str>) -> Result<Option<User>> {
    let Some(secret) = u.totp_secret.clone().filter(|s| !s.is_empty()) else { return Ok(Some(u)) };
    let Some(code) = code.filter(|c| !c.trim().is_empty()) else { return Err(Error::CodeRequired) };
    let Some(step) = crate::totp::verify(&secret, code, now(), u.totp_last_step) else { return Ok(None) };
    // A code works once, even when two requests race with it: only one moves the step forward.
    let moved = User::objects()
        .filter(User::ID.eq(u.id.unwrap_or(-1)) & User::TOTP_LAST_STEP.lt(step))
        .update([User::TOTP_LAST_STEP.set(step)])
        .await?;
    Ok((moved == 1).then_some(u))
}

/// `authenticate`, with the two-factor code when the account has one.
pub async fn authenticate_with_code(username: &str, password: &str, code: Option<&str>) -> Result<Option<User>> {
    // No stored username contains NUL (and Postgres can't even compare one): a plain failure.
    if username.contains('\0') {
        return Ok(None);
    }
    // Count the attempt before checking the password, so a burst of parallel guesses can't all
    // pass a check that only sees failures recorded after the (slow) verification.
    if record_attempt(username).await? > i64::from(MAX_FAILURES) {
        return Err(Error::Locked);
    }
    if let Some(external) = EXTERNAL_LOGIN.get() {
        if let Some(ext) = external(username.to_string(), password.to_string()).await? {
            if ext.is_active {
                let Some(u) = second_factor(mirror(ext).await?, code).await? else { return Ok(None) };
                LoginFailure::objects().filter(LoginFailure::USERNAME.eq(failure_key(username))).delete().await?;
                return Ok(Some(u));
            }
        }
    }
    let user = User::objects().filter(User::USERNAME.eq(username)).first().await?;
    // Hash even for unknown users so response time doesn't reveal which usernames exist.
    static DUMMY: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    let dummy = DUMMY.get_or_init(|| async { hash_password("rangoli-timing-pad").await.unwrap_or_default() }).await;
    let ok = verify_password(password, user.as_ref().map_or(dummy, |u| &u.password)).await;
    match user {
        Some(u) if ok && u.is_active => {
            let Some(u) = second_factor(u, code).await? else { return Ok(None) };
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
        totp_secret: None,
        totp_last_step: 0,
        email: None,
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
    cookie_value(header, COOKIE)
}

/// One cookie's value from a `Cookie` header.
pub(crate) fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('='))
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
                // On /mcp, say where to sign in again, so an expired connection renews itself.
                let challenge = if req.uri().path() == "/mcp" && cfg!(feature = "admin") {
                    crate::oauth::challenge(req.headers())
                } else {
                    "Bearer".into()
                };
                return (axum::http::StatusCode::UNAUTHORIZED, [(axum::http::header::WWW_AUTHENTICATE, challenge)], body).into_response();
            }
            Err(e) => return e.into_response(), // the database is down: 503, not "your token is bad"
        };
        req.extensions_mut().insert(CurrentUser(Some(user)));
        req.extensions_mut().insert(scope);
        return next.run(req).await;
    }
    if let Some(jwt) = crate::jwt::AUTH.get() {
        // Only JWT-shaped bearer values: other schemes still reach the app's own handlers.
        let header = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| t.split('.').count() == 3);
        let cookie = jwt.cookie.as_deref().and_then(|name| {
            req.headers().get(axum::http::header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|c| cookie_value(c, name))
        });
        if let Some(token) = header.or(cookie).map(str::to_owned) {
            let user = match jwt.user(&token).await {
                Ok(u) => u,
                Err(e) => return e.into_response(),
            };
            if user.is_none() && header.is_some() {
                let body = axum::Json(serde_json::json!({ "detail": "Invalid token." }));
                return (axum::http::StatusCode::UNAUTHORIZED, [(axum::http::header::WWW_AUTHENTICATE, "Bearer")], body).into_response();
            }
            if user.is_some() && req.uri().path().starts_with("/admin") {
                let body = axum::Json(serde_json::json!({ "detail": "Sign in to the admin with your password." }));
                return (axum::http::StatusCode::FORBIDDEN, body).into_response();
            }
            if user.is_some() {
                req.extensions_mut().insert(CurrentUser(user));
                return next.run(req).await;
            }
        }
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

// ---------------------------------------------------------------- staff permissions

pub(crate) static ENFORCE_PERMISSIONS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What a staff member may do in the admin: everything (superusers, or when
/// `App::admin_permissions()` is off), or what their groups grant.
#[derive(Clone, Debug, Default)]
pub struct Permissions {
    all: bool,
    granted: std::collections::HashSet<String>,
}

impl Permissions {
    pub fn everything() -> Self {
        Permissions { all: true, granted: Default::default() }
    }

    /// `action` is `view`, `add`, `change` or `delete`. Being allowed to change implies view.
    pub fn allows(&self, table: &str, action: &str) -> bool {
        if self.all {
            return true;
        }
        let has = |a: &str| {
            [format!("{table}.{a}"), format!("{table}.*"), format!("*.{a}"), "*.*".to_string()].iter().any(|p| self.granted.contains(p))
        };
        has(action) || (action == "view" && has("change"))
    }
}

/// The permissions of `user`, from their groups (one query).
pub async fn permissions(user: &User) -> Result<Permissions> {
    if user.is_superuser || !ENFORCE_PERMISSIONS.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(Permissions::everything());
    }
    let groups = User::GROUPS.prefetch(std::slice::from_ref(user)).await?;
    let granted = groups
        .into_values()
        .flatten()
        .flat_map(|g| g.permissions.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect::<Vec<_>>())
        .collect();
    Ok(Permissions { all: false, granted })
}

// ---------------------------------------------------------------- password reset

const RESET_SECS: i64 = 3600;
/// At most one reset email per account in this window, so the form can't flood an inbox.
const RESET_COOLDOWN_SECS: i64 = 120;

/// Email a reset link to the active user(s) with this address. Says nothing about whether one
/// exists (callers show the same message either way). `link` turns a token into the URL to send,
/// e.g. `|t| format!("https://app.example.com/reset/{t}")`.
pub async fn request_password_reset(email: &str, link: impl Fn(&str) -> String) -> Result<()> {
    let email = email.trim();
    if email.is_empty() || !email.contains('@') || email.len() > 254 {
        return Ok(());
    }
    let now = DateTime::now();
    let users = User::objects().filter(User::EMAIL.iexact(email) & User::IS_ACTIVE.eq(true)).all().await?;
    for u in users {
        let uid = u.id.ok_or(Error::NotFound)?;
        let since = DateTime::from_unix(now.unix() - RESET_COOLDOWN_SECS);
        if PasswordReset::objects().filter(PasswordReset::USER_ID.eq(uid) & PasswordReset::CREATED_AT.gt(since)).exists().await? {
            continue;
        }
        let token = random_hex(32)?;
        PasswordReset {
            id: None,
            token_hash: token_digest(&token),
            user_id: uid,
            created_at: now,
            expires_at: DateTime::from_unix(now.unix() + RESET_SECS),
        }
        .save()
        .await?;
        let body = format!(
            "Someone asked to reset the password for {} .\n\nTo choose a new password, open this link within an hour:\n{}\n\nIf it wasn't you, ignore this email; your password stays the same.",
            u.username,
            link(&token)
        );
        crate::mail::send(crate::mail::Mail::new(email, "Reset your password", &body)).await?;
    }
    Ok(())
}

/// The user a reset token belongs to, if it's known and fresh.
pub async fn password_reset_user(token: &str) -> Result<Option<User>> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let found = PasswordReset::objects()
        .filter(PasswordReset::TOKEN_HASH.eq(token_digest(token)) & PasswordReset::EXPIRES_AT.gt(DateTime::now()))
        .first()
        .await?;
    match found {
        Some(r) => Ok(User::objects().filter(User::ID.eq(r.user_id) & User::IS_ACTIVE.eq(true)).first().await?),
        None => Ok(None),
    }
}

/// Set a new password with a reset token: the token (and any other open reset) is spent,
/// every session of the account is signed out, and its failed-login count is cleared.
pub async fn reset_password(token: &str, new_password: &str) -> Result<bool> {
    let Some(mut user) = password_reset_user(token).await? else { return Ok(false) };
    let uid = user.id.ok_or(Error::NotFound)?;
    user.password = hash_password(new_password).await?;
    let spent = crate::atomic(async {
        // Spend this token first: of two racing resets, only one deletes it.
        if PasswordReset::objects().filter(PasswordReset::TOKEN_HASH.eq(token_digest(token))).delete().await? != 1 {
            return Ok(false);
        }
        user.save().await?;
        PasswordReset::objects().filter(PasswordReset::USER_ID.eq(uid)).delete().await?;
        Session::objects().filter(Session::USER_ID.eq(uid)).delete().await?;
        LoginFailure::objects().filter(LoginFailure::USERNAME.eq(failure_key(&user.username))).delete().await?;
        Ok(true)
    })
    .await?;
    Ok(spent)
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_session_cookie_among_others() {
        assert_eq!(super::session_key("a=1; rangoli_sid=abc; b=2"), Some("abc"));
        assert_eq!(super::session_key("rangoli_sidx=abc"), None);
    }
}

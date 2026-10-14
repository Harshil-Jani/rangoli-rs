//! Rangoli: a batteries-included, Django-style web framework for Rust.
//!
//! ```ignore
//! #[derive(Model)]
//! struct Post { id: Option<i64>, #[field(max_length = 200)] title: String, published: bool }
//!
//! #[tokio::main]
//! async fn main() -> rangoli::Result<()> {
//!     rangoli::App::new().admin::<Post>().run().await // runserver | migrate | makemigrations | createsuperuser
//! }
//! ```

extern crate self as rangoli;

pub mod admin;
pub mod api;
pub mod approvals;
pub mod auth;
pub mod cache;
mod cors;
mod crypt;
pub mod datetime;
pub mod decimal;
pub mod django;
pub mod forms;
mod health;
pub mod jwt;
pub mod limit;
pub mod mcp;
pub mod migrate;
pub mod oauth;
pub mod orm;
#[cfg(feature = "realtime")]
pub mod realtime;
pub mod storage;
pub mod tasks;
pub mod totp;
pub mod uuid;
#[cfg(feature = "web")]
pub mod web;
pub mod webhooks;

pub use axum;
pub use datetime::{Date, DateTime};
pub use decimal::Decimal;
pub use orm::{atomic, Json, Model};
pub use rangoli_macros::{migrations, Choices, Model};
pub use serde;
pub use uuid::Uuid;

pub mod prelude {
    pub use crate::admin::ModelAdmin;
    pub use crate::api::{Access, Api};
    pub use crate::auth::CurrentUser;
    pub use crate::forms::ModelForm;
    pub use crate::tasks::Task;
    #[cfg(feature = "web")]
    pub use crate::web::{context, render};
    pub use crate::{atomic, App, Choices, Date, DateTime, Decimal, Error, Json, Model, Result, Uuid};
}

use axum::extract::Request;
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use orm::ModelMeta;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

// ---------------------------------------------------------------- errors

pub enum Error {
    Db(sqlx::Error),
    /// No row matched. Renders as 404, so handlers never need `get_object_or_404`.
    NotFound,
    MultipleObjectsReturned,
    Decode(String),
    Config(String),
    Migration(String),
    Io(std::io::Error),
    /// Too many failed logins for this username.
    Locked,
    /// The password was right; this account also needs its two-factor code.
    CodeRequired,
    /// A template failed to load or render.
    Template(String),
    /// A background task failed.
    Task(String),
    /// Not allowed, with the reason to show (403).
    Forbidden(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Db(e) => write!(f, "database error: {e}"),
            Error::NotFound => f.write_str("not found"),
            Error::MultipleObjectsReturned => f.write_str("query returned more than one row"),
            Error::Decode(m) => write!(f, "decode error: {m}"),
            Error::Config(m) => write!(f, "configuration error: {m}"),
            Error::Migration(m) => write!(f, "migration error: {m}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Locked => f.write_str("too many failed logins, try again in 15 minutes"),
            Error::CodeRequired => f.write_str("enter the 6-digit code from your authenticator app"),
            Error::Template(m) => write!(f, "template error: {m}"),
            Error::Task(m) => write!(f, "task error: {m}"),
            Error::Forbidden(m) => f.write_str(m),
        }
    }
}

// `main() -> Result` prints Debug; make that the readable message.
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for Error {}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Db(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Error::Db(e) if e.as_database_error().is_some_and(|d| d.is_unique_violation()))
    }
    pub fn is_foreign_key_violation(&self) -> bool {
        matches!(self, Error::Db(e) if e.as_database_error().is_some_and(|d| d.is_foreign_key_violation()))
    }
    /// A deadlock or serialization failure: the database chose this statement as the victim,
    /// and running it again is the fix (SQLSTATE 40001/40P01; MySQL reports 1213 as 40001).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Db(e) if e.as_database_error().and_then(|d| d.code()).is_some_and(|c| c == "40001" || c == "40P01"))
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // The database being unreachable is an outage, not a bug: say 503 and when to retry.
        if let Error::Db(sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_)) = self {
            eprintln!("rangoli: database unavailable: {self}");
            return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "5")], "Service Unavailable").into_response();
        }
        let status = match self {
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Locked => StatusCode::TOO_MANY_REQUESTS,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            eprintln!("rangoli: {self}");
            if !settings().debug {
                return (status, "Internal Server Error").into_response();
            }
        }
        (status, self.to_string()).into_response()
    }
}

// ---------------------------------------------------------------- settings

/// Typed settings, read once from the environment (no importable `settings.py`).
#[derive(Clone, Debug)]
pub struct Settings {
    /// `RANGOLI_DATABASE_URL` or `DATABASE_URL`; defaults to a local SQLite file.
    pub database_url: String,
    /// `RANGOLI_DEBUG=1`. Off by default, unlike a fresh Django project.
    pub debug: bool,
    /// `RANGOLI_BIND`, default `127.0.0.1:8000`.
    pub bind: String,
    /// `RANGOLI_MIGRATIONS`, default `migrations`.
    pub migrations: PathBuf,
    /// `RANGOLI_TEMPLATES`, default `templates`.
    pub templates: PathBuf,
    /// `RANGOLI_REQUEST_TIMEOUT` seconds before a request is answered with 503 (default 30).
    pub request_timeout: u64,
    /// `RANGOLI_DB_TIMEOUT` seconds to wait for a database connection before failing (default 5).
    pub db_timeout: u64,
    /// Database connections per process (`RANGOLI_DB_POOL`, default 10).
    pub db_pool: u32,
    /// `RANGOLI_TRUSTED_PROXIES`: comma-separated addresses or networks (`10.0.0.0/8`) of your
    /// load balancers, whose `X-Forwarded-For` is believed. Empty: the header is ignored.
    pub trusted_proxies: Vec<limit::Net>,
    /// `RANGOLI_WORKERS`: task loops `runserver` runs in-process (default 1, 0 turns them off);
    /// also the concurrency of the `worker` command.
    pub workers: usize,
}

impl Settings {
    /// On Postgres, let `realtime::publish` reach every instance. A failure only costs that.
    async fn join_cluster(&self) {
        #[cfg(all(feature = "realtime", feature = "postgres"))]
        if orm::db().dialect == orm::Dialect::Postgres {
            if let Err(e) = realtime::join_cluster(&self.database_url).await {
                eprintln!("warning: realtime stays on this instance: {e}");
            }
        }
    }

    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Settings {
            database_url: var("RANGOLI_DATABASE_URL")
                .or_else(|| var("DATABASE_URL"))
                .unwrap_or_else(|| "sqlite://db.sqlite3?mode=rwc".into()),
            debug: var("RANGOLI_DEBUG").is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes")),
            bind: var("RANGOLI_BIND").unwrap_or_else(|| "127.0.0.1:8000".into()),
            migrations: var("RANGOLI_MIGRATIONS").unwrap_or_else(|| "migrations".into()).into(),
            templates: var("RANGOLI_TEMPLATES").unwrap_or_else(|| "templates".into()).into(),
            workers: var("RANGOLI_WORKERS").and_then(|v| v.parse().ok()).unwrap_or(1),
            request_timeout: var("RANGOLI_REQUEST_TIMEOUT").and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(30),
            db_timeout: var("RANGOLI_DB_TIMEOUT").and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(5),
            db_pool: var("RANGOLI_DB_POOL").and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(10),
            trusted_proxies: var("RANGOLI_TRUSTED_PROXIES")
                .map(|v| {
                    v.split(',')
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| {
                            limit::Net::parse(s).unwrap_or_else(|| panic!("RANGOLI_TRUSTED_PROXIES: `{s}` is not an address or network"))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

pub fn settings() -> &'static Settings {
    SETTINGS.get_or_init(Settings::from_env)
}

// ---------------------------------------------------------------- security

/// Cross-origin request protection plus safe default headers.
///
/// CSRF is enforced for every unsafe request using the browser's
/// `Sec-Fetch-Site`/`Origin` headers, so there are no tokens to thread through
/// forms and no `csrf_exempt` footguns. Requests without either header
/// (curl, server-to-server) carry no ambient browser cookies and pass.
pub async fn security_middleware(req: Request, next: Next) -> Response {
    let trusted = req.extensions().get::<cors::TrustedOrigin>().is_some();
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) && !trusted && is_cross_origin(&req) {
        return (StatusCode::FORBIDDEN, "Cross-origin request blocked").into_response();
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    for (name, value) in [
        (header::CONTENT_SECURITY_POLICY, "default-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "same-origin"),
        (HeaderName::from_static("cross-origin-opener-policy"), "same-origin"),
    ] {
        // Handlers that set their own policy win.
        h.entry(name).or_insert(HeaderValue::from_static(value));
    }
    res
}

fn is_cross_origin(req: &Request) -> bool {
    let get = |n| req.headers().get(n).and_then(|v: &HeaderValue| v.to_str().ok());
    if let Some(site) = get("sec-fetch-site") {
        return !matches!(site, "same-origin" | "none");
    }
    match (get("origin"), get("host")) {
        (Some(origin), Some(host)) => origin.split_once("://").map(|(_, h)| h) != Some(host),
        (Some(_), None) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------- app

pub struct App {
    models: Vec<&'static ModelMeta>,
    admin: Vec<(&'static ModelMeta, admin::Options)>,
    api: Vec<(&'static ModelMeta, api::Options)>,
    routes: Router,
    request_timeout: Option<std::time::Duration>,
    api_rate: Option<u32>,
    mcp_rate: Option<u32>,
    cors: Option<Arc<cors::Cors>>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Starts with the built-in user and session models (users appear in the admin).
    pub fn new() -> Self {
        use orm::Model;
        tasks::register::<webhooks::Delivery>();
        App {
            models: vec![
                auth::User::meta(),
                auth::Session::meta(),
                auth::ApiToken::meta(),
                auth::LoginFailure::meta(),
                approvals::PendingChange::meta(),
                webhooks::Webhook::meta(),
                oauth::OAuthClient::meta(),
                oauth::OAuthCode::meta(),
                admin::LogEntry::meta(),
                tasks::TaskRecord::meta(),
            ],
            admin: vec![
                // The two-factor key never appears in the admin, even to staff.
                (
                    auth::User::meta(),
                    admin::ModelAdmin::<auth::User>::new().exclude([&auth::User::TOTP_SECRET, &auth::User::TOTP_LAST_STEP]).opts,
                ),
                (
                    webhooks::Webhook::meta(),
                    // Where to POST changes: Slack, Zapier, n8n, your own service.
                    admin::ModelAdmin::<webhooks::Webhook>::new()
                        .list_display([
                            &webhooks::Webhook::NAME,
                            &webhooks::Webhook::URL,
                            &webhooks::Webhook::TABLE_NAME,
                            &webhooks::Webhook::ACTIVE,
                        ])
                        .list_filter([&webhooks::Webhook::ACTIVE])
                        .opts,
                ),
                (
                    approvals::PendingChange::meta(),
                    // What agents asked to change, waiting for a person. Approve or reject in bulk.
                    admin::ModelAdmin::<approvals::PendingChange>::new()
                        .list_display([
                            &approvals::PendingChange::SUMMARY,
                            &approvals::PendingChange::TOKEN,
                            &approvals::PendingChange::USER_ID,
                            &approvals::PendingChange::STATUS,
                            &approvals::PendingChange::CREATED_AT,
                        ])
                        .list_filter([&approvals::PendingChange::STATUS, &approvals::PendingChange::CREATED_AT])
                        .readonly_fields([
                            &approvals::PendingChange::USER_ID,
                            &approvals::PendingChange::TOKEN,
                            &approvals::PendingChange::SUMMARY,
                            &approvals::PendingChange::TABLE_NAME,
                            &approvals::PendingChange::ACTION,
                            &approvals::PendingChange::OBJECT_ID,
                            &approvals::PendingChange::PAYLOAD,
                            &approvals::PendingChange::VIA,
                            &approvals::PendingChange::STATUS,
                            &approvals::PendingChange::DECIDED_BY,
                            &approvals::PendingChange::DECIDED_AT,
                            &approvals::PendingChange::RESULT,
                        ])
                        .ordering(approvals::PendingChange::ID.desc())
                        .no_add()
                        .action(
                            "approve",
                            "Approve selected changes",
                            |selected| async move { approvals::approve(selected.all().await?).await },
                        )
                        .action(
                            "reject",
                            "Reject selected changes",
                            |selected| async move { approvals::reject(selected.all().await?).await },
                        )
                        .opts,
                ),
                (
                    auth::ApiToken::meta(),
                    // Made with `createtoken` or `auth::create_token`, which show the token once.
                    // Here: see them, rename, set an expiry, delete to revoke.
                    admin::ModelAdmin::<auth::ApiToken>::new()
                        .list_display([
                            &auth::ApiToken::NAME,
                            &auth::ApiToken::PREFIX,
                            &auth::ApiToken::USER_ID,
                            &auth::ApiToken::READ_ONLY,
                            &auth::ApiToken::LAST_USED_AT,
                            &auth::ApiToken::EXPIRES_AT,
                        ])
                        .list_filter([&auth::ApiToken::USER_ID, &auth::ApiToken::LAST_USED_AT])
                        .readonly_fields([
                            &auth::ApiToken::PREFIX,
                            &auth::ApiToken::KEY_HASH,
                            &auth::ApiToken::USER_ID,
                            &auth::ApiToken::LAST_USED_AT,
                        ])
                        .no_add()
                        .opts,
                ),
                (
                    tasks::TaskRecord::meta(),
                    admin::ModelAdmin::<tasks::TaskRecord>::new()
                        .list_display([
                            &tasks::TaskRecord::NAME,
                            &tasks::TaskRecord::STATUS,
                            &tasks::TaskRecord::ATTEMPTS,
                            &tasks::TaskRecord::RUN_AT,
                            &tasks::TaskRecord::FINISHED_AT,
                        ])
                        .search_fields([&tasks::TaskRecord::NAME, &tasks::TaskRecord::LAST_ERROR])
                        .list_filter([&tasks::TaskRecord::STATUS, &tasks::TaskRecord::CREATED_AT])
                        .action("retry", "Retry selected failed runs now", tasks::retry)
                        .opts,
                ),
            ],
            api: vec![],
            request_timeout: None,
            api_rate: None,
            mcp_rate: Some(120),
            cors: None,
            routes: Router::new(),
        }
    }

    /// Compile the app's migrations into the binary: `.embed_migrations(rangoli::migrations!())`.
    /// Deploy the binary alone; `migrate` finds them without a `migrations/` directory.
    pub fn embed_migrations(self, files: &'static [(&'static str, &'static str)]) -> Self {
        migrate::embed(files);
        self
    }

    /// Serve a model found at run time (the Django bridge's introspected tables) in the admin,
    /// the JSON API and MCP, for staff. `writes` lets staff change it; otherwise it's read-only.
    pub fn dynamic_model(mut self, meta: &'static ModelMeta, writes: bool) -> Self {
        self.models.push(meta);
        self.admin.push((meta, admin::Options::default()));
        let write = if writes { api::Access::Staff } else { api::Access::Nobody };
        self.api.push((meta, api::Options { read: api::Access::Staff, write, owner: None, rows: None }));
        self
    }

    /// Track a model in migrations.
    pub fn model<M: orm::Model>(mut self) -> Self {
        let encrypted = M::meta().fields.iter().any(|f| f.ty == orm::FieldType::Encrypted);
        if encrypted && !crypt::configured() {
            panic!("rangoli: {} has encrypted fields: set RANGOLI_ENCRYPTION_KEY to 64 hex characters (openssl rand -hex 32)", M::TABLE);
        }
        if !self.models.iter().any(|m| m.table == M::TABLE) {
            self.models.push(M::meta());
        }
        self
    }

    /// Track a model in migrations and show it in the admin with default settings.
    pub fn admin<M: orm::Model>(self) -> Self {
        self.admin_with(admin::ModelAdmin::<M>::new())
    }

    /// Show a model in the admin with custom settings (Django's `ModelAdmin`).
    /// Panics at startup on settings that can't work, like Django's admin checks.
    pub fn admin_with<M: orm::Model>(mut self, settings: admin::ModelAdmin<M>) -> Self {
        if let Err(e) = settings.check() {
            panic!("rangoli admin: {e}");
        }
        for w in settings.warnings() {
            eprintln!("rangoli admin warning: {w}");
        }
        self.admin.retain(|(m, _)| m.table != M::TABLE);
        self.admin.push((M::meta(), settings.opts));
        self.model::<M>()
    }

    /// Serve a JSON API for a model at `/api/<table>/` (see [`api`]). Staff-only unless
    /// `Api::read`/`Api::write` open it up.
    pub fn api<M: orm::Model>(mut self, settings: api::Api<M>) -> Self {
        self.api.retain(|(m, _)| m.table != M::TABLE);
        self.api.push((M::meta(), settings.opts));
        self.model::<M>()
    }

    /// Let browser apps on these origins call this app (CORS), e.g. `["https://app.example.com"]`.
    /// Listed origins may also make writes, which the cross-origin protection otherwise blocks.
    /// `"*"` allows any site to read public data, without cookies. Cookies and auth headers
    /// from listed origins need `cors_credentials(true)`.
    pub fn cors<I: IntoIterator<Item = S>, S: Into<String>>(mut self, origins: I) -> Self {
        let origins = origins.into_iter().map(Into::into).collect();
        let credentials = self.cors.as_ref().is_some_and(|c| c.credentials);
        self.cors = Some(Arc::new(cors::Cors { origins, credentials }));
        self
    }

    /// Send `Access-Control-Allow-Credentials` to listed origins (cookies, auth headers).
    pub fn cors_credentials(mut self, on: bool) -> Self {
        let origins = self.cors.as_ref().map(|c| c.origins.clone()).unwrap_or_default();
        self.cors = Some(Arc::new(cors::Cors { origins, credentials: on }));
        self
    }

    /// Limit JSON API requests per caller per minute (the signed-in user, else the IP). Off by
    /// default; over the limit is a 429 with `Retry-After`.
    pub fn rate_limit(mut self, per_minute: u32) -> Self {
        self.api_rate = Some(per_minute);
        self
    }

    /// Limit MCP requests per caller per minute: 120 by default, so a runaway agent loop hits a
    /// wall instead of the database. `None` turns it off.
    pub fn mcp_rate_limit(mut self, per_minute: Option<u32>) -> Self {
        self.mcp_rate = per_minute;
        self
    }

    /// How long a request may take before it is answered with 503 (default `RANGOLI_REQUEST_TIMEOUT`).
    pub fn request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Register a background task so workers in this process can run it.
    /// Run `task` on a UTC cron schedule (`"*/5 * * * *"`, `"0 3 * * 1-5"`, `"@daily"`), with no
    /// separate beat process: workers queue each tick exactly once between them.
    /// Panics at startup on an invalid expression.
    pub fn schedule<T: tasks::Task>(self, cron: &str, task: T) -> Self {
        if let Err(e) = tasks::schedule(cron, &task) {
            panic!("rangoli: {e}");
        }
        self
    }

    /// Sign requests in with JWTs (see [`jwt::JwtAuth`]): tokens you issue, or an identity
    /// provider's (Clerk, Auth0, Supabase Auth) checked against its JWKS keys.
    pub fn jwt(self, auth: jwt::JwtAuth) -> Self {
        if jwt::AUTH.set(auth).is_err() {
            panic!("rangoli: App::jwt can be set once per process");
        }
        self
    }

    pub fn task<T: tasks::Task>(self) -> Self {
        tasks::register::<T>();
        self
    }

    #[cfg(feature = "web")]
    /// Where `rangoli::web::render` finds templates (overrides `RANGOLI_TEMPLATES`).
    pub fn templates(self, dir: impl Into<PathBuf>) -> Self {
        web::set_dir(dir.into());
        self
    }

    #[cfg(feature = "web")]
    /// Serve the files in `dir` under `prefix`, e.g. `.static_files("/static", "static")`.
    /// Paths can't escape `dir`; content types come from file extensions.
    pub fn static_files(mut self, prefix: &str, dir: impl Into<PathBuf>) -> Self {
        self.routes = self.routes.nest_service(prefix, tower_http::services::ServeDir::new(dir.into()));
        self
    }

    /// Your own axum routes, merged next to `/admin`.
    pub fn routes(mut self, r: Router) -> Self {
        self.routes = self.routes.merge(r);
        self
    }

    pub fn models(&self) -> &[&'static ModelMeta] {
        &self.models
    }

    /// The complete application as an axum `Router` (useful in tests).
    pub fn router(&self) -> Router {
        let routes = self.routes.clone();
        #[cfg(feature = "admin")]
        let routes = routes.merge(admin::router(self.admin.clone(), self.models.clone()));
        // Approved changes replay through an un-limited copy of the API.
        approvals::set_api(api::router(self.api.clone()));
        let mut app = routes
            .route("/-/health", axum::routing::get(health::health))
            .merge(limited(api::router(self.api.clone()), self.api_rate))
            .merge(limited(mcp::router(self.api.clone()), self.mcp_rate))
            .layer(from_fn(auth::session_middleware))
            .layer(from_fn(security_middleware));
        if let Some(c) = &self.cors {
            // Outside the cross-origin check, so it can vouch for listed origins and answer preflights.
            app = app.layer(axum::middleware::from_fn_with_state(c.clone(), cors::middleware));
        }
        app.layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            self.request_timeout.unwrap_or(std::time::Duration::from_secs(settings().request_timeout)),
        ))
        // Outermost: a panic in any handler or middleware becomes one 500, never a dead server.
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(panic_response))
    }

    /// The `manage.py` replacement: dispatches on command-line arguments.
    pub async fn run(self) -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let cmd = args.first().map(String::as_str).unwrap_or("runserver");
        let flag = |f: &str| args.iter().any(|a| a == f);
        let positional = args.iter().skip(1).find(|a| !a.starts_with("--")).map(String::as_str);
        let s = settings();

        if cmd == "makemigrations" {
            if flag("--check") {
                let ops = migrate::diff(&migrate::replay(&migrate::load(&s.migrations)?)?, &migrate::model_state(&self.models));
                if !ops.is_empty() {
                    return Err(Error::Migration(format!("{} model change(s) have no migration; run `makemigrations`", ops.len())));
                }
                println!("No changes detected.");
                return Ok(());
            }
            match migrate::make(&s.migrations, &self.models, positional)? {
                Some(p) => println!("Created {}", p.display()),
                None => println!("No changes detected."),
            }
            return Ok(());
        }
        if matches!(cmd, "help" | "--help" | "-h") {
            println!(
                "commands:\n  runserver [addr]            serve the app (default {})\n  migrate [--check]           apply pending migrations\n  makemigrations [name] [--check]\n  createsuperuser [username]  password from RANGOLI_PASSWORD or a prompt\n  createtoken <username> [name] [--days N] [--read-only] [--needs-approval] [--tables a,b]  print a new API token (shown once)\n  worker                      run background tasks (RANGOLI_WORKERS loops)",
                s.bind
            );
            return Ok(());
        }

        orm::connect(&s.database_url).await?;
        match cmd {
            "migrate" if flag("--check") => {
                let pending = migrate::pending(&s.migrations).await?;
                if !pending.is_empty() {
                    return Err(Error::Migration(format!("unapplied: {}", pending.join(", "))));
                }
                println!("All migrations applied.");
            }
            "migrate" => {
                let done = migrate::run(&s.migrations).await?;
                done.iter().for_each(|m| println!("Applied {m}"));
                if done.is_empty() {
                    println!("No migrations to apply.");
                }
                // Like Django's warning. Also catches a binary built before its newest migration
                // file existed (cargo doesn't notice new files in an embedded directory).
                let ahead = migrate::diff(&migrate::replay(&migrate::load(&s.migrations)?)?, &migrate::model_state(&self.models));
                if !ahead.is_empty() {
                    eprintln!(
                        "warning: {} model change(s) have no migration in this binary or {}; run `makemigrations`, then rebuild",
                        ahead.len(),
                        s.migrations.display()
                    );
                }
            }
            "createsuperuser" => {
                let username = match positional {
                    Some(u) => u.to_owned(),
                    None => prompt("Username: ")?,
                };
                let password = match std::env::var("RANGOLI_PASSWORD") {
                    Ok(p) => p,
                    Err(_) => rpassword::prompt_password("Password: ")?,
                };
                if password.len() < 8 {
                    return Err(Error::Config("password must be at least 8 characters".into()));
                }
                auth::create_user(&username, &password, true).await?;
                println!("Superuser `{username}` created.");
            }
            "createtoken" => {
                let username = positional.ok_or_else(|| {
                    Error::Config("usage: createtoken <username> [name] [--days N] [--read-only] [--needs-approval] [--tables a,b]".into())
                })?;
                let user = auth::User::objects().filter(auth::User::USERNAME.eq(username)).get().await?;
                let args: Vec<String> = std::env::args().collect();
                let after = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
                let days =
                    after("--days").map(|d| d.parse::<u64>().map_err(|_| Error::Config("--days takes a number".into()))).transpose()?;
                let name =
                    args.iter().skip_while(|a| *a != username).nth(1).filter(|a| !a.starts_with("--")).cloned().unwrap_or("cli".into());
                let tables =
                    after("--tables").map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>());
                let scope = auth::TokenScope {
                    read_only: flag("--read-only"),
                    tables: tables.filter(|t| !t.is_empty()),
                    needs_approval: flag("--needs-approval"),
                    ..Default::default()
                };
                let ttl = days.map(|d| std::time::Duration::from_secs(d * 86_400));
                let (_, token) = auth::create_scoped_token(&user, &name, ttl, &scope).await?;
                println!("{token}");
                eprintln!("Token `{name}` for `{username}`. Store it now: only its hash is kept.");
            }
            "worker" => {
                s.join_cluster().await;
                println!("Rangoli worker running {} task loop(s)", s.workers.max(1));
                tasks::work(s.workers).await;
            }
            "runserver" => {
                s.join_cluster().await;
                if s.workers > 0 {
                    tokio::spawn(tasks::work(s.workers));
                }
                let pending = migrate::pending(&s.migrations).await?;
                if !pending.is_empty() {
                    eprintln!("warning: {} unapplied migration(s); run `migrate`", pending.len());
                }
                let addr = positional.unwrap_or(&s.bind);
                let listener = tokio::net::TcpListener::bind(addr).await?;
                println!("Rangoli serving on http://{addr} (admin at /admin/){}", if s.debug { " [debug]" } else { "" });
                // Client addresses reach the rate limiter as `ConnectInfo`.
                axum::serve(listener, self.router().into_make_service_with_connect_info::<std::net::SocketAddr>())
                    .with_graceful_shutdown(async {
                        tokio::signal::ctrl_c().await.ok();
                    })
                    .await?;
            }
            other => return Err(Error::Config(format!("unknown command `{other}` (try `help`)"))),
        }
        Ok(())
    }
}

fn limited(router: Router, per_minute: Option<u32>) -> Router {
    match per_minute {
        Some(n) => router.layer(axum::middleware::from_fn_with_state(limit::Limiter::new(n), limit::middleware)),
        None => router,
    }
}

fn panic_response(panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let msg = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
    eprintln!("rangoli: a handler panicked: {msg}");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

fn prompt(label: &str) -> Result<String> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

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
pub mod auth;
pub mod datetime;
pub mod migrate;
pub mod orm;
pub mod tasks;
pub mod web;

pub use axum;
pub use datetime::DateTime;
pub use orm::{atomic, Json, Model};
pub use rangoli_macros::{Choices, Model};
pub use serde;

pub mod prelude {
    pub use crate::admin::ModelAdmin;
    pub use crate::api::{Access, Api};
    pub use crate::auth::CurrentUser;
    pub use crate::tasks::Task;
    pub use crate::web::{context, render, ModelForm};
    pub use crate::{atomic, App, Choices, DateTime, Error, Json, Model, Result};
}

use axum::extract::Request;
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use orm::ModelMeta;
use std::path::PathBuf;
use std::sync::OnceLock;

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
    /// A template failed to load or render.
    Template(String),
    /// A background task failed.
    Task(String),
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
            Error::Template(m) => write!(f, "template error: {m}"),
            Error::Task(m) => write!(f, "task error: {m}"),
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
    /// `RANGOLI_WORKERS`: task loops `runserver` runs in-process (default 1, 0 turns them off);
    /// also the concurrency of the `worker` command.
    pub workers: usize,
}

impl Settings {
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
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) && is_cross_origin(&req) {
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
        App {
            models: vec![auth::User::meta(), auth::Session::meta(), admin::LogEntry::meta(), tasks::TaskRecord::meta()],
            admin: vec![
                (auth::User::meta(), admin::Options::default()),
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
                        .opts,
                ),
            ],
            api: vec![],
            request_timeout: None,
            routes: Router::new(),
        }
    }

    /// Track a model in migrations.
    pub fn model<M: orm::Model>(mut self) -> Self {
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

    /// How long a request may take before it is answered with 503 (default `RANGOLI_REQUEST_TIMEOUT`).
    pub fn request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Register a background task so workers in this process can run it.
    pub fn task<T: tasks::Task>(self) -> Self {
        tasks::register::<T>();
        self
    }

    /// Where `rangoli::web::render` finds templates (overrides `RANGOLI_TEMPLATES`).
    pub fn templates(self, dir: impl Into<PathBuf>) -> Self {
        web::set_dir(dir.into());
        self
    }

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
        self.routes
            .clone()
            .merge(admin::router(self.admin.clone(), self.models.clone()))
            .merge(api::router(self.api.clone()))
            .layer(from_fn(auth::session_middleware))
            .layer(from_fn(security_middleware))
            .layer(tower_http::timeout::TimeoutLayer::with_status_code(
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
                "commands:\n  runserver [addr]            serve the app (default {})\n  migrate [--check]           apply pending migrations\n  makemigrations [name] [--check]\n  createsuperuser [username]  password from RANGOLI_PASSWORD or a prompt\n  worker                      run background tasks (RANGOLI_WORKERS loops)",
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
            "worker" => {
                println!("Rangoli worker running {} task loop(s)", s.workers.max(1));
                tasks::work(s.workers).await;
            }
            "runserver" => {
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
                axum::serve(listener, self.router())
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

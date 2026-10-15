//! Test helpers, like Django's `TestCase` and test `Client`.
//!
//! ```ignore
//! #[tokio::test]
//! async fn only_owners_see_drafts() {
//!     let app = App::new().api::<Post>(Api::new().read(Access::Authenticated).owned_by(&Post::AUTHOR_ID));
//!     testing::database(&app).await.unwrap();                 // a fresh SQLite, or RANGOLI_TEST_DATABASE_URL
//!     let alice = auth::create_user("alice", "pw", false).await.unwrap();
//!     let mut client = testing::Client::new(&app);
//!     client.login(&alice).await;
//!     let res = client.get("/api/blog_post/").await;
//!     assert_eq!(res.status, 200);
//!     assert_eq!(res.json()["count"], 0);
//! }
//! ```

use crate::auth::User;
use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use tower::ServiceExt;

/// Connect the test database and create the app's tables from its models: a new SQLite file
/// in the temp directory, or `RANGOLI_TEST_DATABASE_URL` if set (point it at a throwaway
/// database: the app's tables are dropped and recreated on every run). Migrations are generated
/// into a temp directory, so the test doesn't need (or touch) your `migrations/`.
pub async fn database(app: &crate::App) -> crate::Result<()> {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!(
            "rangoli-test-{}-{}.sqlite3",
            std::process::id(),
            crate::auth::random_hex(4).unwrap_or_default()
        ));
        format!("sqlite://{}?mode=rwc", path.display())
    });
    let db = crate::orm::connect(&url).await?;
    // A clean slate on every run: drop the app's own tables (never the framework's) and forget
    // earlier test migrations, so a reused test database behaves like a new one.
    let d = db.dialect;
    let mut tables: Vec<&str> = app.models().iter().filter(|m| !m.table.starts_with("rangoli_")).map(|m| m.table).collect();
    tables.extend(app.models().iter().flat_map(|m| m.m2m.iter().map(|r| r.through)));
    if d == crate::orm::Dialect::Mysql {
        sqlx::query("SET FOREIGN_KEY_CHECKS = 0").execute(&db.pool).await.ok();
    }
    for t in tables.iter().rev().chain(tables.iter()) {
        // Twice, in both orders: whichever order the foreign keys need.
        let cascade = if d == crate::orm::Dialect::Postgres { " CASCADE" } else { "" };
        sqlx::query(&format!("DROP TABLE IF EXISTS {}{cascade}", d.quote(t))).execute(&db.pool).await.ok();
    }
    if d == crate::orm::Dialect::Mysql {
        sqlx::query("SET FOREIGN_KEY_CHECKS = 1").execute(&db.pool).await.ok();
    }
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%test'").execute(&db.pool).await.ok(); // no built-in ends so
    let dir = std::env::temp_dir().join(format!("rangoli-test-migrations-{}-{}", std::process::id(), crate::auth::random_hex(4)?));
    std::fs::create_dir_all(&dir)?;
    crate::migrate::make(&dir, app.models(), Some("test"))?;
    crate::migrate::run(&dir).await?;
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// A response, read whole.
#[derive(Debug)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Response {
    /// The body as JSON (`Null` if it isn't).
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// Requests to the app without a server: as nobody, as a signed-in user, or with a token.
pub struct Client {
    router: axum::Router,
    cookie: Option<String>,
    bearer: Option<String>,
}

impl Client {
    pub fn new(app: &crate::App) -> Client {
        Client { router: app.router(), cookie: None, bearer: None }
    }

    /// Sign in as `user` (a session cookie, as the admin uses).
    pub async fn login(&mut self, user: &User) {
        let set = crate::auth::login(user).await.expect("rangoli testing: could not create a session");
        self.cookie = set.split(';').next().map(str::to_string);
    }

    /// Send `Authorization: Bearer <token>` from now on (an API token or a JWT).
    pub fn token(&mut self, token: &str) {
        self.bearer = Some(token.to_string());
    }

    /// Back to anonymous.
    pub fn logout(&mut self) {
        (self.cookie, self.bearer) = (None, None);
    }

    pub async fn request(&self, method: Method, path: &str, content_type: Option<&str>, body: String) -> Response {
        let mut req = Request::builder().method(method).uri(path).header("sec-fetch-site", "same-origin");
        if let Some(ct) = content_type {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        if let Some(c) = &self.cookie {
            req = req.header(header::COOKIE, c);
        }
        if let Some(t) = &self.bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let res = self.router.clone().oneshot(req.body(Body::from(body)).expect("rangoli testing: bad request")).await.expect("infallible");
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap_or_default();
        Response { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
    }

    pub async fn get(&self, path: &str) -> Response {
        self.request(Method::GET, path, None, String::new()).await
    }

    pub async fn delete(&self, path: &str) -> Response {
        self.request(Method::DELETE, path, None, String::new()).await
    }

    pub async fn post_json(&self, path: &str, body: &serde_json::Value) -> Response {
        self.request(Method::POST, path, Some("application/json"), body.to_string()).await
    }

    pub async fn patch_json(&self, path: &str, body: &serde_json::Value) -> Response {
        self.request(Method::PATCH, path, Some("application/json"), body.to_string()).await
    }

    pub async fn put_json(&self, path: &str, body: &serde_json::Value) -> Response {
        self.request(Method::PUT, path, Some("application/json"), body.to_string()).await
    }

    /// A form post, as a browser sends one (the admin's forms).
    pub async fn post_form(&self, path: &str, fields: &[(&str, &str)]) -> Response {
        let body = serde_urlencoded::to_string(fields).unwrap_or_default();
        self.request(Method::POST, path, Some("application/x-www-form-urlencoded"), body).await
    }
}

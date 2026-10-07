//! The framework must never be the reason a backend goes down.
//!
//! A deterministic fuzzer throws hostile and random input at every entry point (admin
//! pages, forms, the JSON API, login, paths) and requires that no request is answered
//! with a server error. Then: a panicking handler must cost one 500, not the server; a
//! slow request must time out; a closed database must give 503s while everything that
//! doesn't need it keeps working. Set ROBUSTNESS_ITERS for a longer run (default 250).
#![cfg(all(feature = "admin", feature = "sqlite"))]

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use rangoli::api::{Access, Api};
use rangoli::{migrate, App, Choices, DateTime, Json, Model};
use std::time::Duration;
use tower::ServiceExt;

#[derive(Choices, Clone, Copy, Debug, PartialEq)]
enum Status {
    Draft,
    Published,
}

#[derive(Model, Clone, Debug)]
#[model(table = "fz_author", display = "name")]
struct Author {
    id: Option<i64>,
    #[field(max_length = 40, unique)]
    name: String,
}

#[derive(Model, Clone, Debug)]
#[model(table = "fz_tag", display = "name")]
struct Tag {
    id: Option<i64>,
    #[field(max_length = 20, unique)]
    name: String,
}

#[derive(Model, Clone, Debug)]
#[model(table = "fz_post", display = "title", m2m(tags = Tag), index(status, published_at))]
struct Post {
    id: Option<i64>,
    #[field(max_length = 60, index)]
    title: String,
    #[field(text)]
    body: String,
    views: i64,
    score: Option<f64>,
    flag: bool,
    #[field(fk = Author)]
    author_id: i64,
    #[field(choices, default = "draft")]
    status: Status,
    published_at: Option<DateTime>,
    extra: Option<Json>,
    #[field(auto_now_add)]
    created_at: DateTime,
}

/// xorshift64*: tiny, deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }

    fn string(&mut self) -> String {
        const NASTY: &[&str] = &[
            "",
            " ",
            "0",
            "-1",
            "1.5",
            "99999999999999999999999",
            "-9223372036854775808",
            "1e309",
            "NaN",
            "inf",
            "true",
            "on",
            "null",
            "'",
            "\"",
            "' OR 1=1 --",
            "%",
            "_",
            "\\",
            "\0",
            "%00",
            "../../etc/passwd",
            "<script>alert(1)</script>",
            "{{ 7*7 }}",
            "😀",
            "\u{202e}gnp.exe",
            "2026-02-30T25:61",
            "9999-12-31T23:59",
            "-0001-01-01",
            "\r\n\r\n",
            "draft",
            "published",
            "__all__",
            "f.flag",
            "1;DROP TABLE fz_post",
            "ﷺ",
            "\u{0}\u{1}\u{7f}",
        ];
        match self.below(4) {
            0 => self.pick(NASTY).to_string(),
            1 => "x".repeat([1, 61, 1000, 10_000][self.below(4)]),
            _ => (0..self.below(24))
                .map(|_| match self.below(5) {
                    0 => char::from_u32(self.below(0x80) as u32).unwrap_or('?'),
                    1 => char::from_u32(0x80 + self.below(0x2f80) as u32).unwrap_or('?'),
                    2 => ['%', '&', '=', '?', '#', '/', '+', ';', '\'', '"'][self.below(10)],
                    _ => (b'a' + self.below(26) as u8) as char,
                })
                .collect(),
        }
    }

    fn json(&mut self, depth: u32) -> serde_json::Value {
        use serde_json::{json, Value};
        match if depth > 2 { self.below(5) } else { self.below(7) } {
            0 => Value::Null,
            1 => json!(self.chance(50)),
            2 => json!(self.next() as i64),
            3 => json!([0.0, -1.5, 1e300, f64::MIN_POSITIVE][self.below(4)]),
            4 => json!(self.string()),
            5 => Value::Array((0..self.below(4)).map(|_| self.json(depth + 1)).collect()),
            _ => {
                const KEYS: &[&str] = &[
                    "title",
                    "body",
                    "views",
                    "score",
                    "flag",
                    "author_id",
                    "status",
                    "published_at",
                    "extra",
                    "tags",
                    "id",
                    "created_at",
                    "nope",
                ];
                Value::Object((0..self.below(9)).map(|_| (self.pick(KEYS).to_string(), self.json(depth + 1))).collect())
            }
        }
    }
}

fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_framework_never_takes_the_server_down() {
    let iters: usize = std::env::var("ROBUSTNESS_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(250);
    let dir = std::env::temp_dir().join(format!("rangoli-fuzz-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("sqlite://{}/fuzz.sqlite3?mode=rwc", dir.display()));

    let app = App::new()
        .admin::<Author>()
        .admin::<Tag>()
        .admin::<Post>()
        .api::<Post>(Api::new().read(Access::Public).write(Access::Staff))
        .api::<Author>(Api::new().read(Access::Public).write(Access::Staff))
        .routes(
            axum::Router::new()
                .route("/panic", axum::routing::get(|| async { panic!("handler bug") as &'static str }))
                .route("/health", axum::routing::get(|| async { "ok" })),
        );
    let migrations = dir.join("migrations");
    migrate::make(&migrations, app.models(), None).unwrap();
    let db = rangoli::orm::connect(&url).await.unwrap();
    for t in [
        "fz_post_tags",
        "fz_post",
        "fz_tag",
        "fz_author",
        "rangoli_session",
        "rangoli_admin_log",
        "rangoli_task",
        "rangoli_user",
        "rangoli_migrations",
    ] {
        sqlx::query(&format!("DROP TABLE IF EXISTS {}", db.dialect.quote(t))).execute(&db.pool).await.unwrap();
    }
    migrate::run(&migrations).await.unwrap();
    let router = app.router();

    let send = |req: Request<Body>| {
        let router = router.clone();
        async move {
            let res = router.oneshot(req).await.unwrap();
            let status = res.status();
            let body = to_bytes(res.into_body(), usize::MAX).await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            (status, body)
        }
    };
    let get = |uri: String, cookie: &str| Request::get(uri).header(header::COOKIE, cookie).body(Body::empty()).unwrap();
    let post = |uri: String, cookie: &str, ty: &str, body: String| {
        Request::post(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, ty)
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body))
            .unwrap()
    };

    // A staff session and a little data to aim at.
    rangoli::auth::create_user("staff", "correct horse", true).await.unwrap();
    let res = router
        .clone()
        .oneshot(post("/admin/login".into(), "", "application/x-www-form-urlencoded", "username=staff&password=correct+horse".into()))
        .await
        .unwrap();
    let cookie = res.headers()[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string();
    let mut author = Author { id: None, name: "Ada".into() };
    author.save().await.unwrap();
    let mut tag = Tag { id: None, name: "rust".into() };
    tag.save().await.unwrap();
    let mut seed_post = Post {
        id: None,
        title: "Seed".into(),
        body: "b".into(),
        views: 1,
        score: None,
        flag: true,
        author_id: author.id.unwrap(),
        status: Status::Published,
        published_at: None,
        extra: None,
        created_at: DateTime::now(),
    };
    seed_post.save().await.unwrap();
    let pid = seed_post.id.unwrap();

    let mut rng = Rng(0x5eed_cafe_f00d_d00d);
    let mut failures: Vec<String> = vec![];
    let mut check = |what: String, status: StatusCode, body: &str| {
        if status.is_server_error() {
            failures.push(format!("{status} for {what}: {}", body.chars().take(200).collect::<String>()));
        }
    };
    const PARAMS: &[&str] =
        &["q", "o", "p", "f.flag", "f.status", "f.author_id", "f.published_at", "f.created_at", "log", "warn", "deleted", "what", "nope"];
    const API_PARAMS: &[&str] = &[
        "limit",
        "offset",
        "ordering",
        "search",
        "title",
        "views",
        "flag",
        "status",
        "score",
        "author_id",
        "published_at",
        "extra",
        "nope",
    ];
    const FIELDS: &[&str] =
        &["title", "body", "views", "score", "flag", "author_id", "status", "published_at", "extra", "tags", "_save", "_continue", "csrf"];

    for i in 0..iters {
        // Admin changelist with random filters, search, sorting and paging.
        let qs: Vec<(String, String)> = (0..rng.below(5)).map(|_| (rng.pick(PARAMS).to_string(), rng.string())).collect();
        let uri = format!(
            "/admin/{}/?{}",
            rng.pick(&["fz_post", "fz_author", "fz_tag", "rangoli_user", "rangoli_task"]),
            serde_urlencoded::to_string(&qs).unwrap()
        );
        let (s, b) = send(get(uri.clone(), &cookie)).await;
        check(format!("GET {uri}"), s, &b);

        // Admin add / change forms with random fields.
        let form: Vec<(String, String)> = (0..rng.below(10)).map(|_| (rng.pick(FIELDS).to_string(), rng.string())).collect();
        let target = if rng.chance(50) { "/admin/fz_post/add".to_string() } else { format!("/admin/fz_post/{pid}/") };
        let (s, b) =
            send(post(target.clone(), &cookie, "application/x-www-form-urlencoded", serde_urlencoded::to_string(&form).unwrap())).await;
        check(format!("POST {target} {form:?}"), s, &b);

        // Random admin and API paths.
        let path = format!(
            "/{}/{}/{}/{}",
            rng.pick(&["admin", "api"]),
            percent(&rng.string()),
            percent(&rng.string()),
            rng.pick(&["", "delete", "history"])
        );
        let (s, b) = send(get(path.clone(), &cookie)).await;
        check(format!("GET {path}"), s, &b);

        // API list with random query parameters.
        let qs: Vec<(String, String)> = (0..rng.below(5)).map(|_| (rng.pick(API_PARAMS).to_string(), rng.string())).collect();
        let uri = format!("/api/fz_post/?{}", serde_urlencoded::to_string(&qs).unwrap());
        let (s, b) = send(get(uri.clone(), "")).await;
        check(format!("GET {uri}"), s, &b);

        // API writes with random JSON (sometimes not JSON at all).
        let body = if rng.chance(10) { rng.string() } else { rng.json(0).to_string() };
        let (method, uri) = match rng.below(3) {
            0 => ("POST", "/api/fz_post/".to_string()),
            1 => ("PATCH", format!("/api/fz_post/{pid}")),
            _ => ("PUT", format!("/api/fz_post/{pid}")),
        };
        let req = Request::builder()
            .method(method)
            .uri(&uri)
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.clone()))
            .unwrap();
        let (s, b) = send(req).await;
        check(format!("{method} {uri} {body}"), s, &b);

        // Bulk actions and login with garbage.
        let form: Vec<(String, String)> =
            (0..rng.below(6)).map(|_| (rng.pick(&["action", "_selected_action", "post", "index"]).to_string(), rng.string())).collect();
        let (s, b) =
            send(post("/admin/fz_post/".into(), &cookie, "application/x-www-form-urlencoded", serde_urlencoded::to_string(&form).unwrap()))
                .await;
        check(format!("POST bulk {form:?}"), s, &b);
        if i % 10 == 0 {
            let creds =
                serde_urlencoded::to_string([("username", rng.string()), ("password", rng.string()), ("next", rng.string())]).unwrap();
            let (s, b) = send(post("/admin/login".into(), "", "application/x-www-form-urlencoded", creds)).await;
            check("POST /admin/login".into(), s, &b);
        }

        // Parsers on their own.
        let _ = DateTime::parse(&rng.string());
    }

    // Many requests at once, reads and writes mixed.
    let mut joins = vec![];
    for i in 0..200 {
        let (router, cookie) = (router.clone(), cookie.clone());
        joins.push(tokio::spawn(async move {
            let req = if i % 3 == 0 {
                let body = format!(r#"{{"title": "c{i}", "body": "b", "views": {i}, "author_id": 1}}"#);
                Request::post("/api/fz_post/")
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(body))
                    .unwrap()
            } else {
                Request::get(if i % 2 == 0 { "/api/fz_post/?limit=50" } else { "/admin/fz_post/" })
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap()
            };
            router.oneshot(req).await.unwrap().status()
        }));
    }
    for j in joins {
        let status = j.await.unwrap();
        check("concurrent request".into(), status, "");
    }
    assert!(
        failures.is_empty(),
        "{} server errors, first ones:\n{}",
        failures.len(),
        failures.iter().take(10).cloned().collect::<Vec<_>>().join("\n")
    );

    // A panicking handler costs one 500; the server keeps serving.
    assert_eq!(send(get("/panic".into(), "")).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(send(get("/health".into(), "")).await, (StatusCode::OK, "ok".to_string()));

    // A stuck request is cut off by the request timeout (1s for this app).
    let slow = App::new()
        .request_timeout(Duration::from_secs(1))
        .routes(axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "too late"
            }),
        ))
        .router();
    let started = std::time::Instant::now();
    assert_eq!(slow.oneshot(get("/slow".into(), "")).await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(started.elapsed() < Duration::from_secs(3), "timed out after {:?}", started.elapsed());

    // An oversized body is refused, not buffered without limit.
    let huge = format!(r#"{{"title": "{}"}}"#, "x".repeat(3 * 1024 * 1024));
    let (s, _) = send(post("/api/fz_post/".into(), &cookie, "application/json", huge)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);

    // The database goes away: requests that need it get 503 with Retry-After, the rest still work.
    db.pool.close().await;
    let res = router.clone().oneshot(get("/api/fz_post/".into(), "")).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.headers()[header::RETRY_AFTER], "5");
    assert_eq!(send(get("/health".into(), "")).await, (StatusCode::OK, "ok".to_string()));
    assert_eq!(send(get("/admin/static/admin.css".into(), "")).await.0, StatusCode::OK);
    let (s, _) = send(get("/admin/fz_post/".into(), &cookie)).await;
    assert!(s == StatusCode::SERVICE_UNAVAILABLE || s.is_redirection(), "admin during an outage: {s}");

    let _ = std::fs::remove_dir_all(&dir);
}

//! The query inspector finds an N+1, for staff in the admin and for staff AI agents over MCP.

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use rangoli::api::{Access, Api};
use rangoli::orm::{connect, db, Model};
use rangoli::{auth, App};
use tower::ServiceExt;

#[derive(rangoli::Model, Clone, Debug, serde::Serialize)]
#[model(table = "in_author")]
struct Author {
    id: Option<i64>,
    #[field(max_length = 50)]
    name: String,
}

#[derive(rangoli::Model, Clone, Debug, serde::Serialize)]
#[model(table = "in_book")]
struct Book {
    id: Option<i64>,
    #[field(max_length = 50)]
    title: String,
    #[field(fk = Author)]
    author_id: i64,
}

/// One query per book: the classic N+1.
async fn slow() -> rangoli::Result<Json<Vec<String>>> {
    let mut out = vec![];
    for b in Book::objects().all().await? {
        out.push(format!("{} by {}", b.title, Author::get(b.author_id).await?.name));
    }
    Ok(Json(out))
}

async fn fast() -> rangoli::Result<Json<Vec<String>>> {
    let rows = Book::objects().select_related(Book::AUTHOR).await?;
    Ok(Json(rows.into_iter().map(|(b, a)| format!("{} by {}", b.title, a.map(|a| a.name).unwrap_or_default())).collect()))
}

async fn send(app: &Router, uri: &str, auth_header: Option<String>, body: Option<String>) -> (StatusCode, String) {
    let mut req =
        Request::builder().method(if body.is_some() { "POST" } else { "GET" }).uri(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(h) = auth_header {
        req = req.header(if h.starts_with("rangoli_sid") { header::COOKIE } else { header::AUTHORIZATION }, h);
    }
    let res = app.clone().oneshot(req.body(Body::from(body.unwrap_or_default())).unwrap()).await.unwrap();
    let status = res.status();
    (status, String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn finds_the_n_plus_one() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!("rangoli-inspect-{}.sqlite3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        format!("sqlite://{}?mode=rwc", path.display())
    });
    connect(&url).await.unwrap();
    for t in ["in_book", "in_author"] {
        sqlx::query(&format!("DROP TABLE IF EXISTS {t}")).execute(&db().pool).await.unwrap();
    }
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%inspect_test'").execute(&db().pool).await.ok();
    let app = App::new()
        .api::<Book>(Api::new().read(Access::Authenticated))
        .model::<Author>()
        .inspect_queries()
        .routes(Router::new().route("/slow", get(slow)).route("/fast", get(fast)));
    let dir = std::env::temp_dir().join(format!("rangoli-inspect-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    rangoli::migrate::make(&dir, app.models(), Some("inspect_test")).unwrap();
    rangoli::migrate::run(&dir).await.unwrap();
    for i in 0..8 {
        let mut a = Author { id: None, name: format!("author {i}") };
        a.save().await.unwrap();
        Book { id: None, title: format!("book {i}"), author_id: a.id.unwrap() }.save().await.unwrap();
    }
    let router = app.router();
    assert_eq!(send(&router, "/slow", None, None).await.0, StatusCode::OK);
    assert_eq!(send(&router, "/fast", None, None).await.0, StatusCode::OK);

    let recent = rangoli::inspect::recent();
    let slow_log = recent.iter().find(|r| r.path == "/slow").expect("the request was recorded");
    let fast_log = recent.iter().find(|r| r.path == "/fast").unwrap();
    assert_eq!(slow_log.statements.len(), 9, "1 list + 8 lookups");
    let repeats = slow_log.repeats();
    assert!(repeats.len() == 1 && repeats[0].times == 8 && repeats[0].sql.contains("in_author"), "the N+1 is flagged: {repeats:?}");
    assert!(fast_log.repeats().is_empty() && fast_log.statements.len() == 1, "select_related: one query, nothing flagged");

    // Staff (and their AI agents) can see it; others can't.
    let stamp = std::process::id();
    let boss = auth::create_user(&format!("in_boss_{stamp}"), "correct horse", true).await.unwrap();
    let clerk = auth::create_user(&format!("in_clerk_{stamp}"), "correct horse", false).await.unwrap();
    let cookie = auth::login(&boss).await.unwrap().split(';').next().unwrap().to_string();
    let (_, page) = send(&router, "/admin/requests/", Some(cookie), None).await;
    let page = page.replace("&#x2f;", "/");
    assert!(page.contains("GET /slow") && page.contains("repeated") && page.contains("8×"), "the admin page shows it");
    let tools = r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}"#.to_string();
    let call =
        r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "inspect_requests", "arguments": {"path": "/slow"}}}"#
            .to_string();
    let (_, boss_token) = auth::create_token(&boss, "agent", None).await.unwrap();
    let (_, clerk_token) = auth::create_token(&clerk, "agent", None).await.unwrap();
    let scope = auth::TokenScope { tables: Some(vec!["in_book".into()]), ..Default::default() };
    let (_, scoped) = auth::create_scoped_token(&boss, "scoped", None, &scope).await.unwrap();
    let (_, list) = send(&router, "/mcp", Some(format!("Bearer {boss_token}")), Some(tools.clone())).await;
    assert!(list.contains("inspect_requests"), "staff agents get the tool");
    for t in [&clerk_token, &scoped] {
        let (_, list) = send(&router, "/mcp", Some(format!("Bearer {t}")), Some(tools.clone())).await;
        assert!(!list.contains("inspect_requests"), "not for non-staff or table-scoped tokens");
        let (_, out) = send(&router, "/mcp", Some(format!("Bearer {t}")), Some(call.clone())).await;
        assert!(!out.contains("repeated_statements"), "and calling it anyway gets nothing: {out}");
    }
    let (_, out) = send(&router, "/mcp", Some(format!("Bearer {boss_token}")), Some(call)).await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let first = &v["result"]["structuredContent"]["requests"][0];
    assert_eq!(first["request"], "GET /slow");
    assert_eq!(first["repeated_statements"][0]["times"], 8, "the agent sees the N+1: {first}");

    for t in ["in_book", "in_author"] {
        sqlx::query(&format!("DROP TABLE {t}")).execute(&db().pool).await.unwrap();
    }
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%inspect_test'").execute(&db().pool).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

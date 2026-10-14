//! Semantic search for apps and AI agents: App::embed, GET /api/<table>/similar and the
//! <table>_similar MCP tool, through the same owned_by rules as everything else.

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use rangoli::api::{Access, Api};
use rangoli::orm::{connect, db, Model};
use rangoli::{auth, App, Vector};
use tower::ServiceExt;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "se_ticket")]
struct Ticket {
    id: Option<i64>,
    #[field(max_length = 200)]
    title: String,
    #[field(fk = rangoli::auth::User, cascade)]
    owner_id: i64,
    #[field(vector = 4, index)]
    embedding: Option<Vector>,
}

/// A stand-in embedding model: how much a text is about money, logins, shipping, invoices.
fn embed(text: &str) -> Vec<f32> {
    let t = text.to_lowercase();
    let topics = [
        &["refund", "money", "charge"][..],
        &["login", "password", "sign"],
        &["shipping", "delivery", "parcel"],
        &["invoice", "receipt", "tax"],
    ];
    topics.iter().map(|words| words.iter().filter(|w| t.contains(*w)).count() as f32 + 0.01).collect()
}

async fn send(app: &axum::Router, method: &str, uri: &str, token: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_search_by_meaning_within_their_rights() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!("rangoli-semantic-{}.sqlite3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        format!("sqlite://{}?mode=rwc", path.display())
    });
    connect(&url).await.unwrap();
    if url.starts_with("postgres") {
        let has: Option<(i32,)> =
            sqlx::query_as("SELECT 1 FROM pg_available_extensions WHERE name = 'vector'").fetch_optional(&db().pool).await.unwrap();
        if has.is_none() {
            eprintln!("skipped: this Postgres has no pgvector");
            return;
        }
    }
    sqlx::query("DROP TABLE IF EXISTS se_ticket").execute(&db().pool).await.unwrap();
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%se_ticket%'").execute(&db().pool).await.ok();
    let app = App::new()
        .api::<Ticket>(Api::new().read(Access::Authenticated).write(Access::Authenticated).owned_by(&Ticket::OWNER_ID))
        .embed(|text| async move { Ok(embed(&text)) });
    let dir = std::env::temp_dir().join(format!("rangoli-semantic-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    rangoli::migrate::make(&dir, app.models(), Some("se_ticket")).unwrap();
    rangoli::migrate::run(&dir).await.unwrap();

    let stamp = std::process::id();
    let alice = auth::create_user(&format!("sem_alice_{stamp}"), "correct horse", false).await.unwrap();
    let bob = auth::create_user(&format!("sem_bob_{stamp}"), "correct horse", false).await.unwrap();
    for (title, owner) in [
        ("Refund for a double charge", &alice),
        ("Cannot sign in, password reset fails", &alice),
        ("Parcel delivery is late", &alice),
        ("I want my money back, refund now", &bob),
    ] {
        Ticket { id: None, title: title.into(), owner_id: owner.id.unwrap(), embedding: Some(Vector(embed(title))) }.save().await.unwrap();
    }
    let (_, token) = auth::create_token(&alice, "agent", None).await.unwrap();
    let router = app.router();

    let (status, body) = send(&router, "GET", "/api/se_ticket/similar?text=money+refund&k=2", &token, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let titles: Vec<&str> = body["results"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap()).collect();
    assert_eq!(titles[0], "Refund for a double charge", "closest in meaning first: {titles:?}");
    assert!(!titles.iter().any(|t| t.contains("money back")), "never another user's ticket, however close: {titles:?}");
    assert!(body["results"][0]["distance"].as_f64().unwrap() < body["results"][1]["distance"].as_f64().unwrap());
    assert!(body["results"][0].get("embedding").is_none(), "vectors aren't sent");
    assert_eq!(send(&router, "GET", "/api/se_ticket/similar?text=", &token, "").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(send(&router, "GET", "/api/se_ticket/similar?text=refund", "rgl_nope", "").await.0, StatusCode::UNAUTHORIZED);

    let rpc = |method: &str, params: serde_json::Value| {
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()
    };
    let (_, tools) = send(&router, "POST", "/mcp", &token, &rpc("tools/list", serde_json::json!({}))).await;
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"se_ticket_similar"), "agents get a semantic search tool: {names:?}");
    let call = rpc(
        "tools/call",
        serde_json::json!({ "name": "se_ticket_similar", "arguments": { "text": "my delivery hasn't arrived", "k": 1 } }),
    );
    let (_, out) = send(&router, "POST", "/mcp", &token, &call).await;
    let text = out["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("Parcel delivery is late") && !text.contains("money back"), "{text}");

    sqlx::query("DROP TABLE se_ticket").execute(&db().pool).await.unwrap();
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%se_ticket%'").execute(&db().pool).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

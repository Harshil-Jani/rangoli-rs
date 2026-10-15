//! The admin's "Pause all agents": every token's writes stop at once; reads and people don't.
#![cfg(feature = "admin")]

use rangoli::api::{Access, Api};
use rangoli::auth::{self, TokenScope};
use rangoli::orm::Model;
use rangoli::{testing, App};
use serde_json::json;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "pa_note")]
struct Note {
    id: Option<i64>,
    #[field(max_length = 50)]
    text: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn one_switch_stops_every_agent() {
    let app = App::new().api::<Note>(Api::new().read(Access::Staff).write(Access::Staff));
    testing::database(&app).await.unwrap();
    let mut boss = auth::create_user(&format!("pa_boss_{}", std::process::id()), "correct horse", true).await.unwrap();
    boss.is_superuser = true;
    boss.save().await.unwrap();
    let (_, token) = auth::create_scoped_token(&boss, "agent", None, &TokenScope::default()).await.unwrap();
    let mut agent = testing::Client::new(&app);
    agent.token(&token);
    let mut admin = testing::Client::new(&app);
    admin.login(&boss).await;

    assert_eq!(agent.post_json("/api/pa_note/", &json!({ "text": "before" })).await.status, 201);
    assert!(admin.get("/admin/agents/").await.body.contains("Pause all agents"));
    assert_eq!(admin.post_form("/admin/agents/pause", &[("paused", "1")]).await.status, 303);

    let refused = agent.post_json("/api/pa_note/", &json!({ "text": "during" })).await;
    assert!(refused.status == 503 && refused.body.contains("paused"), "{}", refused.body);
    let mcp = agent
        .post_json("/mcp", &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "pa_note_create", "arguments": { "text": "via mcp" } } }))
        .await
        .json();
    assert_eq!(mcp["result"]["isError"], true, "{mcp}");
    assert_eq!(agent.get("/api/pa_note/").await.status, 200, "reads still work");
    assert_eq!(Note::objects().count().await.unwrap(), 1);
    assert!(admin.get("/admin/agents/").await.body.contains("Resume"));

    admin.post_form("/admin/agents/pause", &[("paused", "0")]).await;
    assert_eq!(agent.post_json("/api/pa_note/", &json!({ "text": "after" })).await.status, 201);
    sqlx::query("DROP TABLE pa_note").execute(&rangoli::orm::db().pool).await.unwrap();
}

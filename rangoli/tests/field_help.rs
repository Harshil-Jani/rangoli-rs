//! A field's `///` doc comment is its help text in OpenAPI and the MCP tools.

use rangoli::api::{Access, Api};
use rangoli::{testing, App};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "fh_ticket")]
struct Ticket {
    id: Option<i64>,
    /// How bad it is, from 1 (cosmetic) to 5 (site down).
    severity: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn doc_comments_reach_openapi_and_agents() {
    let app = App::new().api::<Ticket>(Api::new().read(Access::Public).write(Access::Public));
    testing::database(&app).await.unwrap();
    let c = testing::Client::new(&app);
    let schema = c.get("/api/schema.json").await.json();
    assert_eq!(
        schema["components"]["schemas"]["Ticket"]["properties"]["severity"]["description"],
        "How bad it is, from 1 (cosmetic) to 5 (site down)."
    );
    let tools = c.post_json("/mcp", &serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} })).await.json();
    let create = tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "fh_ticket_create").unwrap().clone();
    assert_eq!(
        create["inputSchema"]["properties"]["severity"]["description"], "How bad it is, from 1 (cosmetic) to 5 (site down).",
        "agents read it"
    );
    sqlx::query("DROP TABLE fh_ticket").execute(&rangoli::orm::db().pool).await.unwrap();
}

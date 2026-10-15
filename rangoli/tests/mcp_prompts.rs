//! MCP prompts (`App::mcp_prompt`) and rows as resources (`rangoli://<table>/<id>`).

use rangoli::api::{Access, Api};
use rangoli::orm::Model;
use rangoli::{testing, App};
use serde_json::{json, Value};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "mp_ticket", display = "title")]
struct Ticket {
    id: Option<i64>,
    #[field(max_length = 50)]
    title: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn prompts_and_resources() {
    let app = App::new().api::<Ticket>(Api::new().read(Access::Public)).mcp_prompt(
        "triage",
        "Rank open tickets",
        "Rank tickets with status {status} for {team}.",
    );
    testing::database(&app).await.unwrap();
    let c = testing::Client::new(&app);
    let rpc = |method: &'static str, params: Value| {
        let c = &c;
        async move { c.post_json("/mcp", &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).await.json() }
    };

    let list = rpc("prompts/list", json!({})).await;
    assert_eq!(list["result"]["prompts"][0]["arguments"][1]["name"], "team", "{list}");
    let got = rpc("prompts/get", json!({ "name": "triage", "arguments": { "status": "open", "team": "billing" } })).await;
    assert_eq!(got["result"]["messages"][0]["content"]["text"], "Rank tickets with status open for billing.");
    assert!(rpc("prompts/get", json!({ "name": "triage", "arguments": {} })).await["error"].is_object(), "a missing argument is an error");

    let mut t = Ticket { id: None, title: "Refund stuck".into() };
    t.save().await.unwrap();
    let templates = rpc("resources/templates/list", json!({})).await;
    assert_eq!(templates["result"]["resourceTemplates"][0]["uriTemplate"], "rangoli://mp_ticket/{id}");
    let read = rpc("resources/read", json!({ "uri": format!("rangoli://mp_ticket/{}", t.id.unwrap()) })).await;
    assert!(read["result"]["contents"][0]["text"].as_str().unwrap().contains("Refund stuck"), "{read}");
    assert_eq!(rpc("resources/read", json!({ "uri": "rangoli://mp_ticket/999999" })).await["error"]["code"], -32002);
    sqlx::query("DROP TABLE mp_ticket").execute(&rangoli::orm::db().pool).await.unwrap();
}

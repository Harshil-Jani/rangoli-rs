//! ?dry_run=1 (and MCP's dry_run): every check runs, the result is reported, nothing is saved.

use rangoli::api::{Access, Api};
use rangoli::orm::Model;
use rangoli::tasks::TaskRecord;
use rangoli::{testing, App};
use serde_json::json;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "dr_item", display = "name")]
struct Item {
    id: Option<i64>,
    #[field(max_length = 50)]
    name: String,
    qty: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_runs_change_nothing() {
    let app = App::new().api::<Item>(Api::new().read(Access::Public).write(Access::Public)).on_change::<Item, _, _>(|_| async { Ok(()) });
    testing::database(&app).await.unwrap();
    let c = testing::Client::new(&app);
    let hooks = || async { TaskRecord::objects().filter(TaskRecord::NAME.eq("rangoli.on_change")).count().await.unwrap() };
    let h0 = hooks().await;

    let r = c.post_json("/api/dr_item/?dry_run=1", &json!({ "name": "Tea", "qty": 2 })).await;
    assert_eq!((r.status.as_u16(), r.json()["would_be"]["name"].as_str()), (200, Some("Tea")), "{}", r.body);
    assert_eq!(Item::objects().count().await.unwrap(), 0, "nothing was created");
    assert_eq!(hooks().await, h0, "and nothing was queued");
    assert_eq!(c.post_json("/api/dr_item/?dry_run=1", &json!({ "name": "Tea", "qty": "two" })).await.status, 400, "validation still runs");

    let id = c.post_json("/api/dr_item/", &json!({ "name": "Tea", "qty": 2 })).await.json()["id"].as_i64().unwrap();
    let r = c.patch_json(&format!("/api/dr_item/{id}?dry_run=1"), &json!({ "qty": 9 })).await;
    assert_eq!(r.json()["would_be"]["qty"], 9);
    assert_eq!(Item::get(id).await.unwrap().qty, 2, "the change wasn't saved");
    let r = c.delete(&format!("/api/dr_item/{id}?dry_run=1")).await;
    assert_eq!(r.json()["would_delete"]["name"], "Tea");
    assert!(Item::get(id).await.is_ok(), "nor the delete");

    let mcp = c
        .post_json(
            "/mcp",
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "dr_item_update", "arguments": { "id": id, "qty": 7, "dry_run": true } } }),
        )
        .await
        .json();
    assert!(mcp["result"]["content"][0]["text"].as_str().unwrap().contains("would_be"), "{mcp}");
    assert_eq!(Item::get(id).await.unwrap().qty, 2, "agents can dry-run too");
    sqlx::query("DROP TABLE dr_item").execute(&rangoli::orm::db().pool).await.unwrap();
}

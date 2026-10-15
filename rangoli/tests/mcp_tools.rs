//! `App::mcp_tool`: your own agent tools, behind the same token rules as the generated ones.

use rangoli::api::{Access, Api};
use rangoli::auth::{self, TokenScope};
use rangoli::mcp::Tool;
use rangoli::orm::Model;
use rangoli::{testing, App};
use serde_json::{json, Value};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "mt_order")]
struct Order {
    id: Option<i64>,
    refunded: bool,
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_tools_follow_token_rules() {
    let refund = Tool::new("refund_order", "Refund an order", json!({ "order_id": { "type": "integer" } }), |user, args| async move {
        let mut o = Order::get(args["order_id"].as_i64().ok_or("order_id must be a number")?).await.map_err(|e| e.to_string())?;
        o.refunded = true;
        o.save().await.map_err(|e| e.to_string())?;
        Ok(json!({ "refunded": o.id, "by": user.username }))
    })
    .tables(["mt_order"])
    .staff_only();
    let app = App::new().api::<Order>(Api::new().read(Access::Staff)).mcp_tool(refund);
    testing::database(&app).await.unwrap();
    let mut o = Order { id: None, refunded: false };
    o.save().await.unwrap();
    let boss = auth::create_user(&format!("mt_boss_{}", std::process::id()), "correct horse", true).await.unwrap();

    let names = |list: &Value| {
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };
    let rpc = |method: &str, params: Value| json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let call = rpc("tools/call", json!({ "name": "refund_order", "arguments": { "order_id": o.id } }));

    let mut ro = testing::Client::new(&app);
    let (_, t) = auth::create_scoped_token(&boss, "reader", None, &TokenScope { read_only: true, ..Default::default() }).await.unwrap();
    ro.token(&t);
    assert!(
        !names(&ro.post_json("/mcp", &rpc("tools/list", json!({}))).await.json()).contains(&"refund_order".into()),
        "hidden from read-only tokens"
    );
    assert!(ro.post_json("/mcp", &call).await.json()["error"].is_object(), "and can't be called");

    let mut other = testing::Client::new(&app);
    let (_, t) = auth::create_scoped_token(
        &boss,
        "other tables",
        None,
        &TokenScope { tables: Some(vec!["somewhere_else".into()]), ..Default::default() },
    )
    .await
    .unwrap();
    other.token(&t);
    assert!(other.post_json("/mcp", &call).await.json()["error"].is_object(), "outside the token's tables");
    assert!(!Order::get(o.id.unwrap()).await.unwrap().refunded);

    let mut full = testing::Client::new(&app);
    let (_, t) = auth::create_scoped_token(
        &boss,
        "agent",
        None,
        &TokenScope { tables: Some(vec!["mt_order".into()]), max_writes_per_day: Some(1), ..Default::default() },
    )
    .await
    .unwrap();
    full.token(&t);
    assert!(names(&full.post_json("/mcp", &rpc("tools/list", json!({}))).await.json()).contains(&"refund_order".into()));
    let r = full.post_json("/mcp", &call).await.json();
    assert_eq!(r["result"]["structuredContent"]["refunded"], json!(o.id), "{r}");
    assert!(Order::get(o.id.unwrap()).await.unwrap().refunded);
    let r = full.post_json("/mcp", &call).await.json();
    assert!(
        r["result"]["isError"] == true && r["result"]["content"][0]["text"].as_str().unwrap().contains("daily limit"),
        "spends the write budget: {r}"
    );
    let r = full.post_json("/mcp", &rpc("tools/call", json!({ "name": "refund_order", "arguments": {} }))).await.json();
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("Missing argument"), "{r}");
    sqlx::query("DROP TABLE mt_order").execute(&rangoli::orm::db().pool).await.unwrap();
}

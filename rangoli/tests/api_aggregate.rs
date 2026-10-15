//! `GET /api/<table>/aggregate` and the MCP `<table>_aggregate` tool: totals over exactly the
//! rows a list with the same filters returns.

use rangoli::api::{Access, Api};
use rangoli::orm::Model;
use rangoli::{testing, App, Decimal};
use serde_json::json;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ag_order")]
struct Order {
    id: Option<i64>,
    #[field(max_length = 20)]
    status: String,
    total: Decimal<2>,
    items: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn totals_match_the_rows() {
    let app = App::new().api::<Order>(Api::new().read(Access::Public));
    testing::database(&app).await.unwrap();
    for (status, total, items) in [("paid", "10.50", 1), ("paid", "4.25", 3), ("open", "100.00", 2)] {
        Order { id: None, status: status.into(), total: total.parse().unwrap(), items }.save().await.unwrap();
    }
    let c = testing::Client::new(&app);
    let all = c.get("/api/ag_order/aggregate?sum=total,items&avg=items&max=total").await.json();
    assert_eq!(all["count"], 3, "{all}");
    assert_eq!(all["sum"]["total"], "114.75", "decimals stay exact");
    assert_eq!(all["sum"]["items"], 6);
    assert_eq!(all["avg"]["items"], 2.0);
    assert_eq!(all["max"]["total"], "100.00");

    let paid = c.get("/api/ag_order/aggregate?status=paid&sum=total").await.json();
    assert_eq!((paid["count"].clone(), paid["sum"]["total"].clone()), (json!(2), json!("14.75")), "filters apply");

    let by = c.get("/api/ag_order/aggregate?group_by=status&sum=items").await.json();
    assert_eq!(
        by["groups"],
        json!([{ "status": "open", "count": 1, "sum": { "items": 2 } }, { "status": "paid", "count": 2, "sum": { "items": 4 } }]),
        "{by}"
    );
    assert_eq!(c.get("/api/ag_order/aggregate?sum=status").await.status, 400, "can't add up text");

    let mcp = c
        .post_json(
            "/mcp",
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "ag_order_aggregate", "arguments": { "filters": { "items__gte": 2 }, "sum": "total" } } }),
        )
        .await
        .json();
    assert_eq!(mcp["result"]["structuredContent"]["sum"]["total"], "104.25", "{mcp}");
    sqlx::query("DROP TABLE ag_order").execute(&rangoli::orm::db().pool).await.unwrap();
}

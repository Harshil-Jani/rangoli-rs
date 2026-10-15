//! A token's daily write budget: enforced, shared by concurrent requests, dry runs free.

use rangoli::api::{Access, Api};
use rangoli::auth::{self, TokenScope};
use rangoli::orm::Model;
use rangoli::{testing, App};
use serde_json::json;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "wb_note")]
struct Note {
    id: Option<i64>,
    #[field(max_length = 50)]
    text: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_get_a_daily_budget() {
    let app = App::new().api::<Note>(Api::new().read(Access::Staff).write(Access::Staff));
    testing::database(&app).await.unwrap();
    let boss = auth::create_user(&format!("wb_boss_{}", std::process::id()), "correct horse", true).await.unwrap();
    let scope = TokenScope { max_writes_per_day: Some(5), ..Default::default() };
    let (_, token) = auth::create_scoped_token(&boss, "careful agent", None, &scope).await.unwrap();
    let mut c = testing::Client::new(&app);
    c.token(&token);

    assert_eq!(c.post_json("/api/wb_note/?dry_run=1", &json!({ "text": "free" })).await.status, 200, "dry runs don't count");
    let racers: Vec<_> = (0..12)
        .map(|i| {
            let c = &c;
            async move { c.post_json("/api/wb_note/", &json!({ "text": format!("n{i}") })).await.status.as_u16() }
        })
        .collect();
    let statuses = futures_util::future::join_all(racers).await;
    assert_eq!(statuses.iter().filter(|s| **s == 201).count(), 5, "exactly 5 of 12 concurrent writes: {statuses:?}");
    assert_eq!(Note::objects().count().await.unwrap(), 5);
    let over = c.post_json("/api/wb_note/", &json!({ "text": "one more" })).await;
    assert_eq!(over.status, 429);
    assert!(over.headers.contains_key("retry-after") && over.body.contains("daily limit of 5"));
    sqlx::query("DROP TABLE wb_note").execute(&rangoli::orm::db().pool).await.unwrap();
}

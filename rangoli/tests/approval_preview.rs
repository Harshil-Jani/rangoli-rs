//! A pending agent change shows reviewers exactly what it would do.
#![cfg(feature = "admin")]

use rangoli::api::{Access, Api};
use rangoli::approvals::PendingChange;
use rangoli::auth::{self, TokenScope};
use rangoli::orm::Model;
use rangoli::{testing, App};
use serde_json::json;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "pv_price", display = "item")]
struct Price {
    id: Option<i64>,
    #[field(max_length = 50)]
    item: String,
    cents: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewers_see_the_diff() {
    let app = App::new().api::<Price>(Api::new().read(Access::Staff).write(Access::Staff));
    testing::database(&app).await.unwrap();
    let mut boss = auth::create_user(&format!("pv_boss_{}", std::process::id()), "correct horse", true).await.unwrap();
    boss.save().await.unwrap();
    let mut p = Price { id: None, item: "Tea".into(), cents: 250 };
    p.save().await.unwrap();
    let scope = TokenScope { needs_approval: true, ..Default::default() };
    let (_, token) = auth::create_scoped_token(&boss, "pricing agent", None, &scope).await.unwrap();
    let mut agent = testing::Client::new(&app);
    agent.token(&token);
    let held = agent.patch_json(&format!("/api/pv_price/{}", p.id.unwrap()), &json!({ "cents": 9900 })).await;
    assert_eq!(held.status, 202, "{}", held.body);
    let pending = PendingChange::objects().order_by(PendingChange::ID.desc()).first().await.unwrap().unwrap();

    let mut admin = testing::Client::new(&app);
    admin.login(&boss).await;
    let page = admin.get(&format!("/admin/rangoli_pending_change/{}/", pending.id.unwrap())).await.body;
    assert!(page.contains("What would change") && page.contains("<tr class=\"changed\"><th scope=\"row\">cents</th><td>250</td><td>9900</td></tr>"), "{page}");
    assert!(page.contains("<tr><th scope=\"row\">item</th><td>Tea</td><td>Tea</td></tr>"), "unchanged fields aren't highlighted");
    assert_eq!(Price::get(p.id.unwrap()).await.unwrap().cents, 250, "previewing changed nothing");
    sqlx::query("DROP TABLE pv_price").execute(&rangoli::orm::db().pool).await.unwrap();
}

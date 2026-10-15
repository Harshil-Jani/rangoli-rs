//! rangoli::testing: a fresh database for the app, and a Client that signs in.

use rangoli::api::{Access, Api};
use rangoli::orm::Model;
use rangoli::{auth, testing, App};
use serde_json::json;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "tc_draft")]
struct Draft {
    id: Option<i64>,
    #[field(max_length = 100)]
    title: String,
    #[field(fk = rangoli::auth::User, cascade)]
    owner_id: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn only_owners_see_their_drafts() {
    let app = App::new().api::<Draft>(Api::new().read(Access::Authenticated).write(Access::Authenticated).owned_by(&Draft::OWNER_ID));
    testing::database(&app).await.unwrap();
    let stamp = std::process::id();
    let alice = auth::create_user(&format!("tc_alice_{stamp}"), "correct horse", false).await.unwrap();
    let bob = auth::create_user(&format!("tc_bob_{stamp}"), "correct horse", false).await.unwrap();

    let mut client = testing::Client::new(&app);
    assert_eq!(client.get("/api/tc_draft/").await.status, 401, "anonymous");
    client.login(&alice).await;
    let made = client.post_json("/api/tc_draft/", &json!({ "title": "Alice's plan" })).await;
    assert_eq!(made.status, 201, "{}", made.body);
    let id = made.json()["id"].as_i64().unwrap();
    assert_eq!(client.get("/api/tc_draft/").await.json()["count"], 1);
    assert_eq!(client.patch_json(&format!("/api/tc_draft/{id}"), &json!({ "title": "Renamed" })).await.status, 200);

    let (_, token) = auth::create_token(&bob, "bob's script", None).await.unwrap();
    client.logout();
    client.token(&token);
    assert_eq!(client.get("/api/tc_draft/").await.json()["count"], 0, "bob sees none of alice's drafts");
    assert_eq!(client.delete(&format!("/api/tc_draft/{id}")).await.status, 404, "and can't delete them");
    assert_eq!(Draft::get(id).await.unwrap().title, "Renamed");
    // Leave a shared test database as we found it (its framework tables are reset by other suites).
    sqlx::query("DROP TABLE tc_draft").execute(&rangoli::orm::db().pool).await.unwrap();
}

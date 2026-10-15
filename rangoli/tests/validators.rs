//! App::validate: whole-row rules on API writes (partial updates included) and admin forms.

use rangoli::api::{Access, Api};
use rangoli::{testing, App};
use serde_json::json;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "vl_booking")]
struct Booking {
    id: Option<i64>,
    starts_at: i64,
    ends_at: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_see_the_whole_row() {
    let app = App::new()
        .api::<Booking>(Api::new().read(Access::Public).write(Access::Public))
        .validate::<Booking>(|b| if b.ends_at <= b.starts_at { Err(vec![("ends_at", "Must be after the start.".into())]) } else { Ok(()) });
    testing::database(&app).await.unwrap();
    let c = testing::Client::new(&app);

    let bad = c.post_json("/api/vl_booking/", &json!({ "starts_at": 10, "ends_at": 5 })).await;
    assert_eq!((bad.status.as_u16(), bad.json()["ends_at"][0].as_str()), (400, Some("Must be after the start.")));
    let ok = c.post_json("/api/vl_booking/", &json!({ "starts_at": 10, "ends_at": 20 })).await;
    assert_eq!(ok.status, 201);
    let id = ok.json()["id"].as_i64().unwrap();
    let patch = c.patch_json(&format!("/api/vl_booking/{id}"), &json!({ "starts_at": 30 })).await;
    assert_eq!(patch.status, 400, "a partial update is checked against the merged row (30 > stored 20)");
    assert_eq!(c.patch_json(&format!("/api/vl_booking/{id}"), &json!({ "starts_at": 15 })).await.status, 200);
    sqlx::query("DROP TABLE vl_booking").execute(&rangoli::orm::db().pool).await.unwrap();
}

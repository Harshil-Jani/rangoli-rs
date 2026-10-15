//! Read replica routing. The "replica" here is a second SQLite file with different contents,
//! so the test can tell which database answered.

use axum::routing::get;
use axum::Router;
use rangoli::orm::{db, on_primary, Model};
use rangoli::{testing, App};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "rp_item")]
struct Item {
    id: Option<i64>,
    #[field(max_length = 50)]
    name: String,
}

async fn names() -> String {
    Item::objects().order_by(Item::ID.asc()).all().await.unwrap().into_iter().map(|i| i.name).collect::<Vec<_>>().join(",")
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_go_to_the_replica_until_you_write() {
    if std::env::var("RANGOLI_TEST_DATABASE_URL").is_ok_and(|u| !u.is_empty()) {
        return; // SQLite files only
    }
    let dir = std::env::temp_dir().join(format!("rangoli-replica-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let replica = dir.join("replica.sqlite3");
    // SAFETY: set before anything reads it, in this test's own process.
    unsafe { std::env::set_var("RANGOLI_DATABASE_REPLICA_URL", format!("sqlite://{}?mode=rwc", replica.display())) };
    let app = App::new().model::<Item>().routes(
        Router::new()
            .route("/read", get(names))
            .route(
                "/write-then-read",
                get(|| async {
                    Item { id: None, name: "fresh".into() }.save().await.unwrap();
                    names().await
                }),
            )
            .route("/forced", get(|| on_primary(names()))),
    );
    testing::database(&app).await.unwrap();
    Item { id: None, name: "primary".into() }.save().await.unwrap();
    // The replica: the same table, a different row.
    let r = sqlx::AnyPool::connect(&format!("sqlite://{}?mode=rwc", replica.display())).await.unwrap();
    sqlx::query("CREATE TABLE rp_item (id INTEGER PRIMARY KEY, name VARCHAR(50) NOT NULL)").execute(&r).await.unwrap();
    sqlx::query("INSERT INTO rp_item (name) VALUES ('replica')").execute(&r).await.unwrap();

    let c = testing::Client::new(&app);
    assert_eq!(c.get("/read").await.body, "\"replica\"".trim_matches('"'), "plain reads go to the replica");
    assert_eq!(c.get("/write-then-read").await.body, "primary,fresh", "after a write, this request reads its own writes");
    assert_eq!(c.get("/read").await.body, "replica", "the next request starts fresh");
    assert_eq!(c.get("/forced").await.body, "primary,fresh", "on_primary forces the primary");
    let _ = db();
    std::fs::remove_dir_all(&dir).ok();
}

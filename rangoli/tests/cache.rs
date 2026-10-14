//! The cache drops values when their tables change: locally, after commit, and on every other
//! instance (Postgres LISTEN/NOTIFY).

use rangoli::cache;
use rangoli::orm::{connect, db, Model};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(rangoli::Model, Clone, Debug, serde::Serialize, serde::Deserialize)]
#[model(table = "ca_item")]
struct Item {
    id: Option<i64>,
    #[field(max_length = 50)]
    name: String,
}

static COMPUTED: AtomicUsize = AtomicUsize::new(0);

async fn names() -> Vec<String> {
    cache::cached("items:names", &["ca_item"], Duration::from_secs(300), || async {
        COMPUTED.fetch_add(1, Ordering::SeqCst);
        Ok(Item::objects().order_by(Item::ID.asc()).all().await?.into_iter().map(|i| i.name).collect())
    })
    .await
    .unwrap()
}

fn computed() -> usize {
    COMPUTED.load(Ordering::SeqCst)
}

async fn exec(sql: &str) {
    sqlx::query(sql).execute(&db().pool).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_clear_what_they_change() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!("rangoli-cache-{}.sqlite3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        format!("sqlite://{}?mode=rwc", path.display())
    });
    connect(&url).await.unwrap();
    exec("DROP TABLE IF EXISTS ca_item").await;
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%ca_item%'").execute(&db().pool).await.ok();
    let dir = std::env::temp_dir().join(format!("rangoli-cache-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let app = rangoli::App::new().model::<Item>();
    rangoli::migrate::make(&dir, app.models(), Some("ca_item")).unwrap();
    rangoli::migrate::run(&dir).await.unwrap();
    Item { id: None, name: "a".into() }.save().await.unwrap();

    assert_eq!(names().await, ["a"]);
    assert_eq!(names().await, ["a"]);
    assert_eq!(computed(), 1, "the second read is served from the cache");

    Item { id: None, name: "b".into() }.save().await.unwrap();
    assert_eq!(names().await, ["a", "b"], "a save clears it");
    Item::objects().filter(Item::NAME.eq("b")).update([Item::NAME.set("bb")]).await.unwrap();
    assert_eq!(names().await, ["a", "bb"], "so does an update");
    Item::objects().filter(Item::NAME.eq("bb")).delete().await.unwrap();
    assert_eq!(names().await, ["a"], "and a delete");
    let before = computed();
    cache::clear_table("some_other_table");
    names().await;
    assert_eq!(computed(), before, "changes to other tables leave it alone");

    // A write while the value is being computed: the (old) value isn't stored.
    let racing = cache::cached("items:race", &["ca_item"], Duration::from_secs(300), || async {
        let rows = Item::objects().count().await?;
        Item { id: None, name: "during".into() }.save().await?; // the write lands mid-compute
        Ok(rows)
    })
    .await
    .unwrap();
    let after =
        cache::cached("items:race", &["ca_item"], Duration::from_secs(300), || async { Item::objects().count().await }).await.unwrap();
    assert_eq!((racing, after), (1, 2), "a value computed across a write is not cached");

    // In a transaction: cleared again once it commits.
    names().await;
    let n = computed();
    rangoli::atomic(async {
        Item { id: None, name: "tx".into() }.save().await?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(names().await.contains(&"tx".to_string()));
    assert_eq!(computed(), n + 1, "the commit cleared it");

    if url.starts_with("postgres") {
        cross_instance(&url).await;
    }
    exec("DROP TABLE ca_item").await;
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%ca_item%'").execute(&db().pool).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// Another instance's write reaches this one through NOTIFY, and this one's writes go out.
#[cfg(all(feature = "postgres", feature = "realtime"))]
async fn cross_instance(url: &str) {
    rangoli::realtime::join_cluster(url).await.unwrap();
    let mut listener = sqlx::postgres::PgListener::connect(url).await.unwrap();
    listener.listen("rangoli_realtime").await.unwrap();

    names().await;
    let n = computed();
    let other = r#"{"from": "another-instance", "group": "rangoli:cache", "message": "ca_item"}"#;
    sqlx::query("SELECT pg_notify('rangoli_realtime', $1)").bind(other).execute(&db().pool).await.unwrap();
    let start = std::time::Instant::now();
    loop {
        names().await;
        if computed() > n {
            break;
        }
        assert!(start.elapsed() < Duration::from_millis(1000), "another instance's change should clear this cache");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    eprintln!("cleared {} ms after another instance's NOTIFY", start.elapsed().as_millis());

    Item { id: None, name: "out".into() }.save().await.unwrap();
    let note = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let n = listener.recv().await.unwrap();
            if n.payload().contains("rangoli:cache") && n.payload().contains("ca_item") {
                return n.payload().to_string();
            }
        }
    })
    .await;
    assert!(note.is_ok(), "this instance's write is announced to the others");
}

#[cfg(not(all(feature = "postgres", feature = "realtime")))]
async fn cross_instance(_: &str) {}

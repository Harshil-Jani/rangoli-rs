//! Adding an index to a live Postgres table doesn't stop writes.
//!
//! A plain `CREATE INDEX` takes a lock that blocks every INSERT, UPDATE and DELETE on the table
//! until the build finishes; on a big table that's minutes of failed requests. Rangoli builds
//! indexes on existing tables with `CREATE INDEX CONCURRENTLY`. This test makes the difference
//! deterministic: an open transaction keeps any index build waiting, and meanwhile another
//! connection tries to write. Concurrently, the write goes through; plainly, it queues.
//! Postgres only: run with RANGOLI_TEST_DATABASE_URL=postgres://...
#![cfg(feature = "postgres")]

use rangoli::orm::{connect, db};
use std::time::Duration;

fn write_migration(dir: &std::path::Path, name: &str, json: &str) {
    std::fs::write(dir.join(format!("{name}.json")), json).unwrap();
}

async fn exec(sql: &str) {
    sqlx::query(sql).execute(&db().pool).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn index_builds_on_live_tables_do_not_block_writes() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").unwrap_or_default();
    if !url.starts_with("postgres") {
        eprintln!("skipped: needs RANGOLI_TEST_DATABASE_URL=postgres://...");
        return;
    }
    connect(&url).await.unwrap();
    exec("DROP TABLE IF EXISTS zd_item").await;
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '2099%'").execute(&db().pool).await.ok(); // a first run has no ledger yet
    let dir = std::env::temp_dir().join(format!("rangoli-zd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_migration(
        &dir,
        "20990101000000_zd_item",
        r#"{"operations": [{"op": "create_table", "table": "zd_item", "columns": [{"name": "name", "type": {"varchar": 50}}]}]}"#,
    );
    rangoli::migrate::run(&dir).await.unwrap();
    exec("INSERT INTO zd_item (name) SELECT 'item ' || g FROM generate_series(1, 20000) g").await;

    // Someone's transaction is open on the table; every index build has to wait for it.
    let mut reader = db().pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *reader).await.unwrap();
    sqlx::query("INSERT INTO zd_item (name) VALUES ('in flight')").execute(&mut *reader).await.unwrap();

    write_migration(
        &dir,
        "20990102000000_zd_index",
        r#"{"operations": [{"op": "add_index", "table": "zd_item", "index": {"name": "zd_item_name_idx", "columns": ["name"]}}]}"#,
    );
    let migration = tokio::spawn({
        let dir = dir.clone();
        async move { rangoli::migrate::run(&dir).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!migration.is_finished(), "the build waits for the open transaction");
    let write =
        tokio::time::timeout(Duration::from_secs(3), sqlx::query("INSERT INTO zd_item (name) VALUES ('during')").execute(&db().pool));
    assert!(write.await.is_ok(), "a write during the concurrent build goes straight through");

    sqlx::query("COMMIT").execute(&mut *reader).await.unwrap();
    assert_eq!(migration.await.unwrap().unwrap(), vec!["20990102000000_zd_index".to_string()]);

    // The same situation with a plain CREATE INDEX: the write queues behind the build.
    sqlx::query("BEGIN").execute(&mut *reader).await.unwrap();
    sqlx::query("INSERT INTO zd_item (name) VALUES ('in flight')").execute(&mut *reader).await.unwrap();
    let plain = tokio::spawn(async { sqlx::query("CREATE INDEX zd_plain ON zd_item (name)").execute(&db().pool).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let blocked =
        tokio::time::timeout(Duration::from_secs(2), sqlx::query("INSERT INTO zd_item (name) VALUES ('blocked')").execute(&db().pool));
    assert!(blocked.await.is_err(), "with a plain CREATE INDEX the same write waits (this is what the feature avoids)");
    sqlx::query("COMMIT").execute(&mut *reader).await.unwrap();
    drop(reader);
    plain.await.unwrap().unwrap();
    let valid: (bool,) =
        sqlx::query_as("SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'zd_item_name_idx'")
            .fetch_one(&db().pool)
            .await
            .unwrap();
    assert!(valid.0, "the index is built and valid");
    exec("DROP TABLE zd_item").await;
    exec("DELETE FROM rangoli_migrations WHERE name LIKE '2099%'").await;
    std::fs::remove_dir_all(&dir).ok();
}

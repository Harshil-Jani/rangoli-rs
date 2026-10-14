//! Turning a column's uniqueness off and on through migrations, on every database.
//! Django needs hand-written SQL (or three migrations) for this on a live table.

use rangoli::orm::{connect, db};

fn write(dir: &std::path::Path, name: &str, ops: &str) {
    std::fs::write(dir.join(format!("{name}.json")), format!(r#"{{"operations": [{ops}]}}"#)).unwrap();
}

async fn exec(sql: &str) -> Result<(), sqlx::Error> {
    sqlx::query(sql).execute(&db().pool).await.map(drop)
}

fn alter(unique: bool) -> String {
    format!(
        r#"{{"op": "alter_column", "table": "mu_user", "column": {{"name": "email", "type": {{"varchar": 100}}, "unique": {unique}}}}}"#
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn uniqueness_can_be_dropped_and_added() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!("rangoli-mu-{}.sqlite3", std::process::id()));
        format!("sqlite://{}?mode=rwc", path.display())
    });
    connect(&url).await.unwrap();
    exec("DROP TABLE IF EXISTS mu_user").await.unwrap();
    exec("DELETE FROM rangoli_migrations WHERE name LIKE '2098%'").await.ok();
    let dir = std::env::temp_dir().join(format!("rangoli-mu-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Created unique inline, so the database named the constraint itself.
    write(
        &dir,
        "20980101000000_mu",
        r#"{"op": "create_table", "table": "mu_user", "columns": [{"name": "email", "type": {"varchar": 100}, "unique": true}]}"#,
    );
    rangoli::migrate::run(&dir).await.unwrap();
    exec("INSERT INTO mu_user (email) VALUES ('a@x.io')").await.unwrap();
    assert!(exec("INSERT INTO mu_user (email) VALUES ('a@x.io')").await.is_err(), "unique at first");

    write(&dir, "20980102000000_mu_not_unique", &alter(false));
    rangoli::migrate::run(&dir).await.unwrap();
    exec("INSERT INTO mu_user (email) VALUES ('a@x.io')").await.expect("duplicates allowed once uniqueness is dropped");

    // Adding it back while duplicates exist fails, and leaves nothing half-built behind.
    write(&dir, "20980103000000_mu_unique_again", &alter(true));
    assert!(rangoli::migrate::run(&dir).await.is_err(), "duplicates block the unique constraint");
    exec("DELETE FROM mu_user").await.unwrap();
    exec("INSERT INTO mu_user (email) VALUES ('a@x.io')").await.unwrap();
    rangoli::migrate::run(&dir).await.expect("applies once the data allows it");
    assert!(exec("INSERT INTO mu_user (email) VALUES ('a@x.io')").await.is_err(), "unique again");

    // And off again: the constraint added by the migration drops too.
    write(&dir, "20980104000000_mu_off_again", &alter(false));
    rangoli::migrate::run(&dir).await.unwrap();
    exec("INSERT INTO mu_user (email) VALUES ('a@x.io')").await.expect("off again");

    exec("DROP TABLE mu_user").await.unwrap();
    exec("DELETE FROM rangoli_migrations WHERE name LIKE '2098%'").await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

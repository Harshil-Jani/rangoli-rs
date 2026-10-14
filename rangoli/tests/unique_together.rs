//! `#[model(unique(a, b))]`: Django's unique_together, enforced by the database.

use rangoli::orm::{connect, db, Model};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ut_page", unique(team, slug))]
struct Page {
    id: Option<i64>,
    team: i64,
    #[field(max_length = 50)]
    slug: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn pairs_are_unique_not_parts() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!("rangoli-ut-{}.sqlite3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        format!("sqlite://{}?mode=rwc", path.display())
    });
    connect(&url).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS ut_page").execute(&db().pool).await.unwrap();
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%ut_page%'").execute(&db().pool).await.ok();
    let dir = std::env::temp_dir().join(format!("rangoli-ut-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let app = rangoli::App::new().model::<Page>();
    rangoli::migrate::make(&dir, app.models(), Some("ut_page")).unwrap();
    rangoli::migrate::run(&dir).await.unwrap();

    Page { id: None, team: 1, slug: "about".into() }.save().await.unwrap();
    let dup = Page { id: None, team: 1, slug: "about".into() }.save().await.unwrap_err();
    assert!(dup.is_unique_violation(), "the same pair is refused: {dup}");
    Page { id: None, team: 2, slug: "about".into() }.save().await.expect("the same slug in another team is fine");
    Page { id: None, team: 1, slug: "contact".into() }.save().await.expect("another slug in the same team is fine");

    sqlx::query("DROP TABLE ut_page").execute(&db().pool).await.unwrap();
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%ut_page%'").execute(&db().pool).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

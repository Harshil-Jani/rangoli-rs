//! Full-text search through `#[model(search(...))]`, on every database: Postgres (GIN over
//! tsvector), MySQL (FULLTEXT) and SQLite (FTS5 kept in sync by triggers).

use rangoli::orm::{connect, db, Model};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "se_article", search(title, body))]
struct Article {
    id: Option<i64>,
    #[field(max_length = 200)]
    title: String,
    #[field(text)]
    body: String,
}

async fn exec(sql: &str) {
    sqlx::query(sql).execute(&db().pool).await.unwrap();
}

async fn titles(q: &str) -> Vec<String> {
    let mut t: Vec<String> = Article::objects().search(q).all().await.unwrap().into_iter().map(|a| a.title).collect();
    t.sort();
    t
}

#[tokio::test(flavor = "multi_thread")]
async fn full_text_search_on_every_database() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!("rangoli-search-{}.sqlite3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        format!("sqlite://{}?mode=rwc", path.display())
    });
    connect(&url).await.unwrap();
    let sqlite = url.starts_with("sqlite");
    if sqlite {
        for t in ["se_article_search_idx_ai", "se_article_search_idx_ad", "se_article_search_idx_au"] {
            exec(&format!("DROP TRIGGER IF EXISTS {t}")).await;
        }
        exec("DROP TABLE IF EXISTS se_article_search_idx").await;
    }
    exec("DROP TABLE IF EXISTS se_article").await;
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%se_article%'").execute(&db().pool).await.ok();
    let dir = std::env::temp_dir().join(format!("rangoli-search-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The whole app's models, so the diff doesn't drop the framework's own tables.
    let app = rangoli::App::new().model::<Article>();
    rangoli::migrate::make(&dir, app.models(), Some("se_article")).unwrap();
    rangoli::migrate::run(&dir).await.unwrap();

    for (title, body) in [
        ("Rust in production", "Lessons from shipping a web service."),
        ("Async Rust patterns", "Futures, executors and pinning."),
        ("Ownership explained", "Why the borrow checker rejects your rust code."),
        ("Django tips", "Querysets, admin and migrations."),
    ] {
        Article { id: None, title: title.into(), body: body.into() }.save().await.unwrap();
    }

    assert_eq!(titles("rust").await, ["Async Rust patterns", "Ownership explained", "Rust in production"], "title or body");
    assert_eq!(titles("RUST").await, titles("rust").await, "case doesn't matter");
    assert_eq!(titles("ownersh").await, ["Ownership explained"], "words match as prefixes");
    assert_eq!(titles("rust async").await, ["Async Rust patterns"], "every word must match");
    assert_eq!(titles("django admin").await, ["Django tips"]);
    assert!(titles("kubernetes").await.is_empty());
    for hostile in ["\"'); DROP TABLE se_article;--", "*", "\"", "NEAR(rust", "rust:* | !django", "a & b", "-rust", "+++", "(((", "😀 rust"]
    {
        let found = Article::objects().search(hostile).all().await;
        assert!(found.is_ok(), "search for {hostile:?} errored: {found:?}");
    }
    assert_eq!(titles("😀 rust").await, titles("rust").await, "symbols are dropped, words kept");
    assert_eq!(Article::objects().search("").count().await.unwrap(), 4, "an empty search matches everything");

    // Edits and deletes show up in search (SQLite: the FTS triggers).
    let mut a = Article::objects().search("django").first().await.unwrap().unwrap();
    a.title = "Flask tips".into();
    a.body = "Blueprints and routing.".into();
    a.save().await.unwrap();
    assert!(titles("django").await.is_empty(), "the old words are gone");
    assert_eq!(titles("flask").await, ["Flask tips"], "the new words are found");
    a.delete().await.unwrap();
    assert!(titles("flask").await.is_empty(), "deleted rows leave the index");
    assert_eq!(
        Article::objects().search("rust").filter(Article::TITLE.eq("Rust in production")).count().await.unwrap(),
        1,
        "combines with filters"
    );

    exec("DROP TABLE se_article").await;
    if sqlite {
        exec("DROP TABLE IF EXISTS se_article_search_idx").await;
    }
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%se_article%'").execute(&db().pool).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

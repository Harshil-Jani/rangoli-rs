//! Trees from a foreign key to the same table: descendants and ancestors in one recursive
//! query, on every database.

use rangoli::orm::{connect, count_queries, db, Model};
use std::collections::{HashMap, HashSet};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "tr_category")]
struct Category {
    id: Option<i64>,
    #[field(max_length = 50)]
    name: String,
    #[field(fk = Category)]
    parent_id: Option<i64>,
}

async fn exec(sql: &str) {
    sqlx::query(sql).execute(&db().pool).await.unwrap();
}

async fn ids(q: rangoli::orm::QuerySet<Category>) -> Vec<i64> {
    let mut v: Vec<i64> = q.all().await.unwrap().into_iter().filter_map(|c| c.id).collect();
    v.sort_unstable();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn descendants_and_ancestors_in_one_query() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!("rangoli-tree-{}.sqlite3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        format!("sqlite://{}?mode=rwc", path.display())
    });
    connect(&url).await.unwrap();
    exec("DROP TABLE IF EXISTS tr_category").await;
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%tr_category%'").execute(&db().pool).await.ok();
    let dir = std::env::temp_dir().join(format!("rangoli-tree-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let app = rangoli::App::new().model::<Category>();
    rangoli::migrate::make(&dir, app.models(), Some("tr_category")).unwrap();
    rangoli::migrate::run(&dir).await.unwrap();

    // 10,000 nodes, ten children each: node n's parent is (n - 2) / 10 + 1.
    const N: i64 = 10_000;
    let mut rows: Vec<Category> =
        (1..=N).map(|n| Category { id: Some(n), name: format!("c{n}"), parent_id: (n > 1).then(|| (n - 2) / 10 + 1) }).collect();
    for chunk in rows.chunks_mut(1000) {
        Category::bulk_create(chunk).await.unwrap();
    }
    let parent: HashMap<i64, i64> = (2..=N).map(|n| (n, (n - 2) / 10 + 1)).collect();
    let mut expected: Vec<i64> = (2..=N)
        .filter(|&n| {
            let mut p = parent[&n];
            loop {
                if p == 2 {
                    return true;
                }
                match parent.get(&p) {
                    Some(&up) => p = up,
                    None => return false,
                }
            }
        })
        .collect();
    expected.sort_unstable();

    let (got, queries) = count_queries(ids(Category::objects().filter(Category::PARENT.descendants(2)))).await;
    assert_eq!(got, expected, "every node under 2, at every depth");
    assert_eq!(queries, 1, "one query, however deep");
    assert_eq!(expected.len(), 1110, "10 + 100 + 1000 below node 2");
    assert_eq!(Category::objects().filter(Category::PARENT.descendants(1)).count().await.unwrap(), N - 1, "the root has everyone");
    assert_eq!(Category::objects().filter(Category::PARENT.descendants(N)).count().await.unwrap(), 0, "a leaf has no one");
    let mut chain = vec![];
    let mut n = 9_999;
    while let Some(&p) = parent.get(&n) {
        chain.push(p);
        n = p;
    }
    chain.sort_unstable();
    assert_eq!(chain, [1, 10, 100, 1000]);
    assert_eq!(ids(Category::objects().filter(Category::PARENT.ancestors(9_999))).await, chain, "up to the root");
    assert!(ids(Category::objects().filter(Category::PARENT.ancestors(1))).await.is_empty(), "the root has no ancestors");
    let deep_c2 = Category::objects().filter(Category::PARENT.descendants(2) & Category::NAME.eq("c120")).count().await.unwrap();
    assert_eq!(deep_c2, 1, "combines with other filters");

    // A cycle in bad data still ends.
    exec("UPDATE tr_category SET parent_id = 9999 WHERE id = 1").await;
    let looped = Category::objects().filter(Category::PARENT.descendants(1)).count().await.unwrap();
    assert_eq!(looped, N, "a cycle returns each row once instead of looping");
    let up: HashSet<i64> = ids(Category::objects().filter(Category::PARENT.ancestors(9_999))).await.into_iter().collect();
    assert_eq!(up, HashSet::from([1, 10, 100, 1000, 9_999]), "and upward too");

    exec("UPDATE tr_category SET parent_id = NULL").await;
    exec("DROP TABLE tr_category").await;
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%tr_category%'").execute(&db().pool).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

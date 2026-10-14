//! Vector search: `#[field(vector = N)]` and `QuerySet::nearest`, on every database. On Postgres
//! it's pgvector with an HNSW index (skipped if the server has no pgvector), checked for recall
//! against exact search.

use rangoli::orm::{connect, db, Model};
use rangoli::Vector;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ve_doc")]
struct Doc {
    id: Option<i64>,
    #[field(max_length = 50)]
    title: String,
    #[field(max_length = 20)]
    lang: String,
    #[field(vector = 3, index)]
    embedding: Option<Vector>,
}

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ve_big")]
struct Big {
    id: Option<i64>,
    #[field(vector = 64, index)]
    embedding: Option<Vector>,
}

async fn exec(sql: &str) {
    sqlx::query(sql).execute(&db().pool).await.unwrap();
}

fn titles(found: &[(Doc, f32)]) -> Vec<&str> {
    found.iter().map(|(d, _)| d.title.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn nearest_neighbours() {
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        let path = std::env::temp_dir().join(format!("rangoli-vector-{}.sqlite3", std::process::id()));
        let _ = std::fs::remove_file(&path);
        format!("sqlite://{}?mode=rwc", path.display())
    });
    connect(&url).await.unwrap();
    let pg = url.starts_with("postgres");
    if pg {
        let has: Option<(i32,)> =
            sqlx::query_as("SELECT 1 FROM pg_available_extensions WHERE name = 'vector'").fetch_optional(&db().pool).await.unwrap();
        if has.is_none() {
            eprintln!("skipped: this Postgres has no pgvector (use the pgvector/pgvector image)");
            return;
        }
    }
    for t in ["ve_doc", "ve_big"] {
        exec(&format!("DROP TABLE IF EXISTS {t}")).await;
    }
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%ve_%'").execute(&db().pool).await.ok();
    let dir = std::env::temp_dir().join(format!("rangoli-vector-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let app = rangoli::App::new().model::<Doc>().model::<Big>();
    rangoli::migrate::make(&dir, app.models(), Some("ve")).unwrap();
    rangoli::migrate::run(&dir).await.unwrap();

    for (title, lang, v) in [
        ("east", "en", Some([1.0, 0.0, 0.0])),
        ("north-east", "en", Some([0.7, 0.7, 0.0])),
        ("north", "fr", Some([0.0, 1.0, 0.0])),
        ("west", "en", Some([-1.0, 0.0, 0.0])),
        ("nowhere", "en", None),
    ] {
        Doc { id: None, title: title.into(), lang: lang.into(), embedding: v.map(|v| Vector(v.to_vec())) }.save().await.unwrap();
    }
    let q = Vector(vec![0.9, 0.1, 0.0]);
    let found = Doc::objects().nearest(Doc::EMBEDDING, &q, 3).await.unwrap();
    assert_eq!(titles(&found), ["east", "north-east", "north"], "closest first, k of them");
    assert!(found[0].1 < 0.01 && found[0].1 < found[1].1 && found[1].1 < found[2].1, "with increasing distances: {found:?}");
    let en = Doc::objects().filter(Doc::LANG.eq("en")).nearest(Doc::EMBEDDING, &q, 10).await.unwrap();
    assert_eq!(titles(&en), ["east", "north-east", "west"], "filters apply, and rows without a vector are skipped");
    let stored = Doc::objects().filter(Doc::TITLE.eq("north")).first().await.unwrap().unwrap();
    assert_eq!(stored.embedding, Some(Vector(vec![0.0, 1.0, 0.0])), "vectors round-trip");

    if pg {
        recall_against_exact_search().await;
    }
    for t in ["ve_doc", "ve_big"] {
        exec(&format!("DROP TABLE {t}")).await;
    }
    sqlx::query("DELETE FROM rangoli_migrations WHERE name LIKE '%ve_%'").execute(&db().pool).await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// 20,000 random 64-dimension vectors: the HNSW index's top 10 against the exact top 10.
async fn recall_against_exact_search() {
    let mut seed = 0x2545F4914F6CDD1Du64;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0
    };
    let mut all: Vec<Vector> = vec![];
    let mut rows: Vec<Big> = (0..20_000)
        .map(|_| {
            let v = Vector((0..64).map(|_| rand()).collect());
            all.push(v.clone());
            Big { id: None, embedding: Some(v) }
        })
        .collect();
    for chunk in rows.chunks_mut(500) {
        Big::bulk_create(chunk).await.unwrap();
    }
    exec("ANALYZE ve_big").await;
    let plan: Vec<(String,)> =
        sqlx::query_as("EXPLAIN SELECT id FROM ve_big ORDER BY embedding <=> (SELECT embedding FROM ve_big LIMIT 1) LIMIT 10")
            .fetch_all(&db().pool)
            .await
            .unwrap();
    let plan = plan.into_iter().map(|r| r.0).collect::<Vec<_>>().join("\n");
    assert!(plan.contains("ve_big_embedding_idx"), "the HNSW index is used:\n{plan}");

    let ids: Vec<i64> = rows.iter().map(|r| r.id.unwrap()).collect();
    let (mut hits, queries, mut took) = (0, 50, vec![]);
    for _ in 0..queries {
        let q = Vector((0..64).map(|_| rand()).collect());
        let mut exact: Vec<(i64, f32)> = ids.iter().zip(&all).map(|(id, v)| (*id, v.cosine_distance(&q))).collect();
        exact.sort_by(|a, b| a.1.total_cmp(&b.1));
        let truth: std::collections::HashSet<i64> = exact[..10].iter().map(|(id, _)| *id).collect();
        let t = std::time::Instant::now();
        let got = Big::objects().nearest(Big::EMBEDDING, &q, 10).await.unwrap();
        took.push(t.elapsed().as_secs_f64() * 1000.0);
        hits += got.iter().filter(|(b, _)| truth.contains(&b.id.unwrap())).count();
    }
    let recall = hits as f64 / (queries * 10) as f64;
    took.sort_by(f64::total_cmp);
    eprintln!("HNSW recall@10 over {queries} queries: {recall:.3}; median query {:.1} ms", took[took.len() / 2]);
    assert!(recall >= 0.95, "recall@10 {recall:.3} is below 0.95");
}

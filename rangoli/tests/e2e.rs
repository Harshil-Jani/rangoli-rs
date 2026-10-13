//! End to end: migrations, ORM, schema evolution and the admin over HTTP.
//! Runs against SQLite by default; CI also points `RANGOLI_TEST_DATABASE_URL`
//! at Postgres and MySQL. One test function because the pool is process-global.
#![cfg(all(feature = "admin", feature = "web", feature = "sqlite"))]

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use rangoli::admin::ModelAdmin;
use rangoli::api::{Access, Api};
use rangoli::orm;
use rangoli::orm::Choice;
use rangoli::tasks::{self, Task, TaskRecord, TaskStatus};
use rangoli::web::ModelForm;
use rangoli::{migrate, App, Choices, Date, DateTime, Decimal, Error, Json, Model, Uuid};
use tower::ServiceExt;

#[derive(Model, Clone, Debug, PartialEq)]
#[model(table = "blog_author", display = "name")]
struct Author {
    id: Option<i64>,
    #[field(max_length = 100)]
    name: String,
    #[field(max_length = 254, unique)]
    email: String,
}

#[derive(Model, Clone, Debug)]
#[model(table = "blog_post", display = "title")]
struct Post {
    id: Option<i64>,
    #[field(max_length = 200)]
    title: String,
    #[field(text)]
    body: String,
    published: bool,
    #[field(fk = Author)]
    author_id: i64,
    rating: Option<f64>,
}

/// Owned rows for the per-object API permissions test.
#[derive(Model, Clone, Debug)]
#[model(table = "e2e_note", display = "title")]
struct Note {
    id: Option<i64>,
    #[field(max_length = 100)]
    title: String,
    #[field(fk = rangoli::auth::User, cascade)]
    owner_id: i64,
    #[field(max_length = 100, private)]
    secret: Option<String>,
}

#[derive(Choices, Clone, Copy, Debug, PartialEq)]
enum Status {
    Draft,
    Published,
    #[choice(value = "old", label = "Archived for good")]
    Archived,
}

#[derive(Model, Clone, Debug, PartialEq)]
#[model(table = "blog_tag", display = "name")]
struct Tag {
    id: Option<i64>,
    #[field(max_length = 50, unique)]
    name: String,
}

/// The same table after a schema change: `rating` dropped, `views` added, `title` widened and
/// indexed, timestamps added, and a many-to-many relation to tags.
#[derive(Model, Clone, Debug)]
#[model(table = "blog_post", display = "title", m2m(tags = Tag))]
struct PostV2 {
    id: Option<i64>,
    #[field(max_length = 300, index)]
    title: String,
    #[field(text)]
    body: String,
    published: bool,
    #[field(fk = Author)]
    author_id: i64,
    views: i64,
    #[field(auto_now_add)]
    created_at: DateTime,
    #[field(auto_now)]
    updated_at: DateTime,
    publish_at: Option<DateTime>,
    due: Option<Date>,
    price: Decimal<2>, // non-null: existing rows get 0.00 when the column is added
    #[field(auto, unique)]
    public_id: Uuid, // added to a table with rows: each existing row needs its own
    #[field(auto)]
    trace_id: Uuid, // not unique, but existing rows still get their own, not all-zero
    #[field(choices, default = "draft")]
    status: Status,
    meta: Option<Json>,
}

// ---- background tasks used by the test
static GREETED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static FLAKY_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[derive(serde::Serialize, serde::Deserialize)]
struct Greet {
    name: String,
}
impl Task for Greet {
    const NAME: &'static str = "greet";
    async fn run(self) -> rangoli::Result<()> {
        GREETED.lock().unwrap().push(self.name);
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Flaky;
impl Task for Flaky {
    const NAME: &'static str = "flaky";
    async fn run(self) -> rangoli::Result<()> {
        match FLAKY_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => Err(Error::Task("boom".into())),
            _ => Ok(()),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Explodes;
impl Task for Explodes {
    const NAME: &'static str = "explodes";
    const MAX_ATTEMPTS: i64 = 1;
    async fn run(self) -> rangoli::Result<()> {
        panic!("kaboom")
    }
}

/// A page rendered from a template; `?partial` renders only the `rows` block.
async fn tag_page(axum::extract::Query(p): axum::extract::Query<Vec<(String, String)>>) -> rangoli::Result<axum::response::Html<String>> {
    let names: Vec<String> = Tag::objects().order_by(Tag::NAME.asc()).all().await?.into_iter().map(|t| t.name).collect();
    let name = if p.iter().any(|(k, _)| k == "partial") { "tags.html#rows" } else { "tags.html" };
    rangoli::web::render(name, rangoli::web::context! { names, title => "<Tags & more>" })
}

fn post(title: &str, author: &Author, published: bool, rating: Option<f64>) -> Post {
    Post { id: None, title: title.into(), body: format!("Body of {title}"), published, author_id: author.id.unwrap(), rating }
}

async fn reset(db: &orm::Db) {
    for t in [
        "blog_post_tags",
        "blog_tag",
        "blog_post",
        "e2e_note",
        "rangoli_pending_change",
        "rangoli_api_token",
        "rangoli_login_failure",
        "rangoli_session",
        "rangoli_admin_log",
        "rangoli_task",
        "blog_author",
        "rangoli_user",
        "rangoli_migrations",
    ] {
        let sql = format!("DROP TABLE IF EXISTS {}", db.dialect.quote(t));
        sqlx::query(&sql).execute(&db.pool).await.unwrap();
    }
}

/// One long scenario: its future outgrows a test thread's default 2 MB stack in debug builds,
/// so it runs on a thread with room to spare.
#[test]
fn full_stack() {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(scenario()))
        .unwrap()
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e));
}

async fn scenario() {
    let dir = std::env::temp_dir().join(format!("rangoli-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("sqlite://{}/test.sqlite3?mode=rwc", dir.display()));
    std::fs::create_dir_all(&dir).unwrap();
    let migrations = dir.join("migrations");

    // ---- makemigrations + migrate
    let v1 = App::new().admin::<Author>().admin::<Post>();
    let file = migrate::make(&migrations, v1.models(), None).unwrap().expect("first migration");
    assert!(file.file_name().unwrap().to_str().unwrap().ends_with("_create_blog_author.json"));
    assert!(!std::fs::read_to_string(&file).unwrap().contains("rangoli_"), "framework tables come from built-in migrations, not yours");
    assert!(migrate::make(&migrations, v1.models(), None).unwrap().is_none(), "no changes the second time");

    let db = orm::connect(&url).await.unwrap();
    reset(db).await;
    let applied = migrate::run(&migrations).await.unwrap();
    assert_eq!(applied.len(), migrate::builtin().len() + 1, "built-in + app: {applied:?}");
    assert!(migrate::run(&migrations).await.unwrap().is_empty());

    // ---- ORM
    let mut ada = Author { id: None, name: "Ada".into(), email: "ada@example.com".into() };
    ada.save().await.unwrap();
    let mut alan = Author { id: None, name: "Alan".into(), email: "alan@example.com".into() };
    alan.save().await.unwrap();
    assert!(ada.id.is_some() && ada.id != alan.id);

    for p in [
        post("Rust 50% off_sale", &ada, true, Some(4.5)),
        post("Notes on Engines", &ada, false, None),
        post("Computing Machinery", &alan, true, Some(5.0)),
    ] {
        let mut p = p;
        p.save().await.unwrap();
    }

    assert_eq!(Post::objects().count().await.unwrap(), 3);
    let published = Post::objects().filter(Post::PUBLISHED.eq(true)).order_by(Post::TITLE.asc()).all().await.unwrap();
    assert_eq!(published.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(), ["Computing Machinery", "Rust 50% off_sale"]);
    // icontains with LIKE metacharacters in the needle
    assert_eq!(Post::objects().filter(Post::TITLE.contains("50% OFF_")).count().await.unwrap(), 1);
    assert_eq!(Post::objects().filter(Post::TITLE.contains("0%x")).count().await.unwrap(), 0);
    assert_eq!(Post::objects().filter(Post::RATING.is_null()).count().await.unwrap(), 1);
    assert_eq!(Post::objects().filter(Post::RATING.gte(4.6) | Post::TITLE.starts_with("notes")).count().await.unwrap(), 2);
    assert_eq!(Post::objects().exclude(Post::AUTHOR_ID.eq(ada.id.unwrap())).count().await.unwrap(), 1);
    let second = Post::objects().order_by(Post::ID.asc()).offset(1).first().await.unwrap().unwrap();
    assert_eq!(second.title, "Notes on Engines");
    assert!(Post::objects().filter(Post::ID.eq(-1)).exists().await.map(|e| !e).unwrap());

    assert!(matches!(Post::get(999_999).await, Err(Error::NotFound)));
    assert!(matches!(Post::objects().filter(Post::AUTHOR_ID.eq(ada.id.unwrap())).get().await, Err(Error::MultipleObjectsReturned)));

    let mut notes = Post::objects().filter(Post::TITLE.eq("Notes on Engines")).get().await.unwrap();
    notes.rating = Some(3.0);
    notes.save().await.unwrap();
    notes.save().await.unwrap(); // a no-op update must not be mistaken for a missing row
    assert_eq!(Post::get(notes.id.unwrap()).await.unwrap().rating, Some(3.0));

    let n = Post::objects().filter(Post::PUBLISHED.eq(false)).update([Post::PUBLISHED.set(true), Post::RATING.set_null()]).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(Post::objects().filter(Post::PUBLISHED.eq(true)).count().await.unwrap(), 3);

    let authors = Author::in_bulk([ada.id.unwrap(), alan.id.unwrap(), 424242]).await.unwrap();
    assert_eq!(authors.len(), 2);
    assert_eq!(authors[&ada.id.unwrap()], ada);

    // ---- foreign keys: forward, batch, reverse; query counts don't grow with the rows
    let posts = Post::objects().order_by(Post::ID.asc()).all().await.unwrap();
    assert_eq!(posts[0].author().await.unwrap(), ada, "post.author()");
    let (by_id, queries) = orm::count_queries(Post::AUTHOR.load(&posts)).await;
    let by_id = by_id.unwrap();
    assert_eq!(queries, 1, "select_related: one query for {} posts", posts.len());
    assert!(posts.iter().all(|p| by_id[&p.author_id].id == Some(p.author_id)));
    assert_eq!(Post::AUTHOR.reverse(&ada).count().await.unwrap(), 2, "author.post_set");
    let joined_q = || Post::objects().filter(Post::PUBLISHED.eq(true)).order_by(Post::ID.desc()).limit(2);
    let (joined, queries) = orm::count_queries(joined_q().select_related(Post::AUTHOR)).await;
    let joined = joined.unwrap();
    assert_eq!(queries, 1, "select_related is one query (a JOIN)");
    let plain = joined_q().all().await.unwrap();
    assert_eq!(
        joined.iter().map(|(p, _)| p.id).collect::<Vec<_>>(),
        plain.iter().map(|p| p.id).collect::<Vec<_>>(),
        "same rows, same order, same limit"
    );
    for (p, a) in &joined {
        assert_eq!(a.as_ref(), Some(&by_id[&p.author_id]), "each with its author, as Fk::load finds it");
    }
    let everyone = Author::objects().all().await.unwrap();
    let (per_author, queries) = orm::count_queries(Post::AUTHOR.prefetch_reverse(&everyone)).await;
    let per_author = per_author.unwrap();
    assert_eq!(queries, 1, "reverse prefetch: one query for {} authors", everyone.len());
    for a in &everyone {
        let one_by_one: Vec<_> = Post::AUTHOR.reverse(a).order_by(Post::ID.asc()).all().await.unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(per_author[&a.id.unwrap()].iter().map(|p| p.id).collect::<Vec<_>>(), one_by_one, "prefetch matches per-object access");
    }

    // ---- model forms
    let pairs = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
    let errs = ModelForm::<Author>::new().validate(&pairs(&[("name", "Form Author"), ("email", "")])).await.unwrap_err();
    assert_eq!(errs["email"], "This field is required.");
    let fresh_author = ModelForm::<Author>::new().validate(&pairs(&[("name", "Form Author"), ("email", "f@example.com")])).await.unwrap();
    assert_eq!((fresh_author.id, fresh_author.name.as_str()), (None, "Form Author"), "a typed, unsaved model");
    let edited = ModelForm::<Author>::new()
        .fields([&Author::NAME])
        .instance(ada.clone())
        .validate(&pairs(&[("name", "Ada L."), ("email", "attacker@evil.example")]))
        .await
        .unwrap();
    assert_eq!(
        (edited.id, edited.name.as_str(), edited.email.as_str()),
        (ada.id, "Ada L.", "ada@example.com"),
        "fields() limits what a form can change"
    );

    // ---- transactions
    let before = Author::objects().count().await.unwrap();
    let temp = |n: &str| Author { id: None, name: n.into(), email: format!("{n}@example.com") };
    let failed: rangoli::Result<()> = rangoli::atomic(async {
        temp("temp1").save_new().await?;
        rangoli::atomic(async { temp("temp2").save_new().await }).await?; // a released savepoint
        assert_eq!(Author::objects().count().await?, before + 2, "writes are visible inside the transaction");
        Err(Error::NotFound)
    })
    .await;
    assert!(matches!(failed, Err(Error::NotFound)), "{failed:?}");
    assert_eq!(Author::objects().count().await.unwrap(), before, "an Err rolls everything back");
    rangoli::atomic(async { temp("temp3").save_new().await }).await.unwrap();
    assert_eq!(Author::objects().filter(Author::NAME.eq("temp3")).count().await.unwrap(), 1, "an Ok commits");
    Author::objects().filter(Author::NAME.eq("temp3")).delete().await.unwrap();

    // Savepoints: a failed nested block undoes only itself, even after a failed statement
    // (which aborts the whole transaction on Postgres without one).
    rangoli::atomic(async {
        temp("keep").save_new().await?;
        let inner = rangoli::atomic(async {
            temp("gone").save_new().await?;
            Author { id: None, name: "dup".into(), email: "ada@example.com".into() }.save_new().await
        })
        .await;
        assert!(inner.unwrap_err().is_unique_violation());
        temp("after").save_new().await // the transaction is still usable
    })
    .await
    .unwrap();
    let names = |n: &'static str| Author::objects().filter(Author::NAME.eq(n)).count();
    assert_eq!((names("keep").await.unwrap(), names("gone").await.unwrap(), names("after").await.unwrap()), (1, 0, 1));
    Author::objects().filter(Author::NAME.is_in(["keep".to_string(), "after".to_string()])).delete().await.unwrap();

    let dup = Author { id: None, name: "Copy".into(), email: "ada@example.com".into() }.save_new().await;
    assert!(dup.unwrap_err().is_unique_violation());
    let orphan = post("Orphan", &Author { id: Some(424242), ..ada.clone() }, true, None).save_new().await;
    assert!(orphan.unwrap_err().is_foreign_key_violation(), "foreign keys are enforced (SQLite too)");
    assert!(ada.delete().await.unwrap_err().is_foreign_key_violation(), "can't delete an author with posts");

    // ---- schema evolution: drop a column, add a NOT NULL one, widen another
    let v2 = App::new()
        .admin::<Author>()
        .admin::<Tag>()
        .admin_with::<PostV2>(
            ModelAdmin::new()
                .list_display([&PostV2::TITLE, &PostV2::AUTHOR_ID, &PostV2::PUBLISHED, &PostV2::VIEWS, &PostV2::CREATED_AT])
                .search_fields([&PostV2::TITLE])
                .list_filter([&PostV2::PUBLISHED, &PostV2::AUTHOR_ID, &PostV2::CREATED_AT, &PostV2::STATUS])
                .ordering(PostV2::TITLE.asc())
                .readonly_fields([&PostV2::VIEWS])
                .list_per_page(2)
                .action("publish", "Mark selected as published", |selected| async move {
                    selected.update([PostV2::PUBLISHED.set(true)]).await.map(drop)
                })
                .action("explode", "Bump views, then fail", |selected| async move {
                    selected.update([PostV2::VIEWS.set(123_456)]).await?;
                    Err(Error::NotFound)
                }),
        )
        .task::<Greet>()
        .schedule("*/5 * * * *", Greet { name: "Beat".into() })
        .schedule("*/5 * * * *", Greet { name: "Beat2".into() }) // same task and cron, other payload: its own schedule
        .task::<Flaky>()
        .task::<Explodes>()
        .api::<Note>(Api::new().read(Access::Authenticated).write(Access::Public).owned_by(&Note::OWNER_ID))
        .api::<PostV2>(Api::new().read(Access::Public))
        .templates(dir.join("templates"))
        .static_files("/static", dir.join("static"))
        .routes(axum::Router::new().route("/tags", axum::routing::get(tag_page)))
        .api::<Author>(Api::new().read(Access::Authenticated).write(Access::Nobody));
    let file = migrate::make(&migrations, v2.models(), Some("post_views")).unwrap().unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    for op in ["\"alter_column\"", "\"add_column\"", "\"drop_column\"", "\"add_index\"", "\"blog_post_tags\"", "\"blog_post_title_idx\""] {
        assert!(text.contains(op), "{op} missing from {text}");
    }
    assert_eq!(migrate::pending(&migrations).await.unwrap().len(), 1);
    migrate::run(&migrations).await.unwrap();
    let rows = PostV2::objects().order_by(PostV2::ID.asc()).all().await.unwrap();
    assert_eq!(rows.len(), 3, "data survives the migration");
    assert!(rows.iter().all(|p| p.views == 0));
    assert!(rows.iter().all(|p| p.status == Status::Draft && p.meta.is_none()), "existing rows get the model default");
    let made = DateTime::now().start_of_day();
    assert!(rows.iter().all(|p| p.created_at >= made), "existing rows get the time the migration was made, not the epoch");
    let mut ids: Vec<Uuid> = rows.iter().map(|p| p.public_id).collect();
    ids.sort();
    ids.dedup();
    assert!(ids.len() == 3 && !ids.contains(&Uuid::NIL), "a new unique UUID column gives every existing row its own: {ids:?}");
    let mut traces: Vec<Uuid> = rows.iter().map(|p| p.trace_id).collect();
    traces.sort();
    traces.dedup();
    assert!(traces.len() == 3 && !traces.contains(&Uuid::NIL), "so does a non-unique one (found by review): {traces:?}");
    let epoch = DateTime::from_unix(0);
    let mut long = PostV2 {
        id: None,
        title: "x".repeat(300),
        body: String::new(),
        published: false,
        author_id: alan.id.unwrap(),
        views: 7,
        created_at: epoch,
        updated_at: epoch,
        publish_at: DateTime::parse("2026-01-02T03:04"),
        due: Date::parse("1969-07-20"), // before the epoch: stored negative
        price: Decimal::parse("19.99").unwrap(),
        public_id: Uuid::NIL, // `auto` fills it on insert
        trace_id: Uuid::NIL,
        status: Status::Published,
        meta: Some(Json(serde_json::json!({ "source": "e2e", "tags": [1, 2] }))),
    };
    long.save().await.unwrap();
    let today = DateTime::now().start_of_day();
    assert!(long.created_at >= today && long.updated_at >= today, "auto_now_add and auto_now fill in on insert");
    let created = long.created_at;
    long.updated_at = epoch;
    long.created_at = epoch;
    long.save().await.unwrap();
    assert!(long.updated_at >= today, "auto_now refreshes on every save");
    assert_eq!(long.created_at, epoch, "auto_now_add only applies when adding");
    long.created_at = created;
    long.save().await.unwrap();
    let fresh = PostV2::get(long.id.unwrap()).await.unwrap();
    assert_eq!((fresh.created_at, fresh.publish_at.unwrap().to_string()), (created, "2026-01-02T03:04:00Z".to_string()));
    let jan1 = DateTime::parse("2026-01-01").unwrap();
    assert_eq!(PostV2::objects().filter(PostV2::PUBLISH_AT.gte(jan1)).count().await.unwrap(), 1, "datetimes compare in SQL");
    assert_eq!(PostV2::objects().filter(PostV2::PUBLISH_AT.lt(jan1)).count().await.unwrap(), 0);
    assert_eq!(PostV2::objects().filter(PostV2::PUBLISH_AT.is_null()).count().await.unwrap(), 3);
    assert_eq!(fresh.due.unwrap().to_string(), "1969-07-20", "dates round-trip");
    assert_eq!(fresh.price.to_string(), "19.99", "decimals round-trip exactly");
    assert_eq!((fresh.public_id, fresh.public_id.version()), (long.public_id, 7), "auto fills a v7 UUID on insert");
    assert_eq!(PostV2::objects().filter(PostV2::PUBLIC_ID.eq(long.public_id)).get().await.unwrap().id, long.id, "lookup by UUID");
    let chosen = Uuid::new_v4();
    let mut own = PostV2 { id: None, public_id: chosen, title: "own id".into(), ..fresh.clone() };
    own.save().await.unwrap();
    assert_eq!(PostV2::get(own.id.unwrap()).await.unwrap().public_id, chosen, "a UUID set by the caller is kept");
    let mut dup = PostV2 { id: None, title: "dup id".into(), ..own.clone() };
    assert!(dup.save().await.unwrap_err().is_unique_violation(), "the UUID column is unique");
    own.delete().await.unwrap();
    let cents = |s: &str| Decimal::<2>::parse(s).unwrap();
    assert_eq!(PostV2::objects().filter(PostV2::PRICE.eq(cents("19.99"))).count().await.unwrap(), 1, "exact equality in SQL");
    assert_eq!(PostV2::objects().filter(PostV2::PRICE.gt(cents("19.98"))).count().await.unwrap(), 1, "one cent apart compares");
    assert_eq!(PostV2::objects().filter(PostV2::PRICE.eq(cents("0"))).count().await.unwrap(), 3, "existing rows got 0.00");
    assert_eq!(
        PostV2::objects().filter(PostV2::DUE.lt(Date::from_ymd(1970, 1, 1).unwrap())).count().await.unwrap(),
        1,
        "dates compare in SQL"
    );
    assert!(migrate::make(&migrations, v2.models(), None).unwrap().is_none(), "indexes and join tables round-trip");

    // ---- choices and JSON
    assert_eq!(
        (Status::Archived.as_str(), Status::Archived.label(), Status::Draft.to_string()),
        ("old", "Archived for good", "Draft".into())
    );
    assert_eq!(PostV2::objects().filter(PostV2::STATUS.eq(Status::Published)).count().await.unwrap(), 1, "typed choice filter");
    assert_eq!(fresh.meta.as_ref().unwrap()["tags"][1], 2, "JSON round-trips");
    assert_eq!(serde_json::to_string(&Status::Archived).unwrap(), "\"old\"");

    // ---- many-to-many
    let (mut rust, mut web) = (Tag { id: None, name: "rust".into() }, Tag { id: None, name: "web".into() });
    rust.save().await.unwrap();
    web.save().await.unwrap();
    let first = PostV2::objects().order_by(PostV2::ID.asc()).first().await.unwrap().unwrap();
    first.tags().add(&[&rust, &web]).await.unwrap();
    first.tags().add_ids([rust.id.unwrap()]).await.unwrap(); // already linked: skipped
    assert_eq!(first.tags().count().await.unwrap(), 2);
    assert_eq!(first.tags().all().await.unwrap(), vec![rust.clone(), web.clone()]);
    assert_eq!(first.tags().query().filter(Tag::NAME.eq("web")).count().await.unwrap(), 1, "related rows are a queryset");
    assert_eq!(PostV2::objects().filter(PostV2::TAGS.has(rust.id.unwrap())).count().await.unwrap(), 1, "filter across the relation");
    assert_eq!(PostV2::TAGS.reverse(&web).count().await.unwrap(), 1, "reverse direction");
    let all_posts = PostV2::objects().all().await.unwrap();
    let (prefetched, queries) = rangoli::orm::count_queries(PostV2::TAGS.prefetch(&all_posts)).await;
    let prefetched = prefetched.unwrap();
    assert_eq!(queries, 2, "prefetch is two queries however many posts");
    assert_eq!(prefetched.len(), all_posts.len(), "every source gets an entry");
    assert_eq!(prefetched[&first.id.unwrap()], vec![rust.clone(), web.clone()], "prefetch matches per-object access");
    first.tags().set_ids([web.id.unwrap()]).await.unwrap();
    assert_eq!(first.tags().ids().await.unwrap(), vec![web.id.unwrap()]);
    first.tags().remove_ids([web.id.unwrap()]).await.unwrap();
    assert_eq!(first.tags().count().await.unwrap(), 0);
    let unsaved = PostV2 { id: None, ..first.clone() };
    assert!(unsaved.tags().ids().await.is_err(), "unsaved objects have no relations yet");
    let mut doomed = Tag { id: None, name: "doomed".into() };
    doomed.save().await.unwrap();
    first.tags().add(&[&doomed]).await.unwrap();
    doomed.delete().await.unwrap();
    assert_eq!(first.tags().count().await.unwrap(), 0, "deleting a tag removes its links");

    std::fs::create_dir_all(dir.join("templates")).unwrap();
    std::fs::create_dir_all(dir.join("static")).unwrap();
    std::fs::write(
        dir.join("templates/base.html"),
        "<!doctype html><title>{{ title }}</title><main>{% block content %}{% endblock %}</main>",
    )
    .unwrap();
    std::fs::write(
        dir.join("templates/tags.html"),
        r#"{% extends "base.html" %}{% block content %}<ul id="rows">{% block rows %}{% for n in names %}<li>{{ n }}</li>{% endfor %}{% endblock %}</ul>{% endblock %}"#,
    )
    .unwrap();
    std::fs::write(dir.join("static/app.css"), "body { color: red }").unwrap();

    // ---- admin over HTTP
    let app = v2.router();
    let send = |req: Request<Body>| {
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let headers = res.headers().clone();
            let body = String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
            (status, headers, body)
        }
    };
    let get = |uri: &str, cookie: &str| Request::get(uri).header(header::COOKIE, cookie).body(Body::empty()).unwrap();
    let form = |uri: &str, cookie: &str, body: &str| {
        Request::post(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (status, headers, _) = send(get("/admin/", "")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/admin/login?next=%2Fadmin%2F");
    assert!(headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("default-src 'self'"));
    assert_eq!(headers["cross-origin-opener-policy"], "same-origin");

    rangoli::auth::create_user("root", "correct horse", true).await.unwrap();
    let (status, _, body) = send(form("/admin/login", "", "username=root&password=nope")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("correct username and password"));

    let (status, headers, _) = send(form("/admin/login", "", "username=root&password=correct+horse&next=%2Fadmin%2Fblog_author%2F")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/admin/blog_author/");
    let cookie = headers[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string();

    let (status, _, body) = send(get("/admin/", &cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Rangoli administration") && body.contains("Signed in as <strong>root</strong>"));
    assert!(
        body.contains("<caption>Blog</caption>") && body.contains("<caption>Authentication and Authorization</caption>"),
        "models grouped by app"
    );
    assert!(body.contains("Recent actions") && body.contains("Nothing yet."));

    let (_, _, body) = send(get("/admin/blog_post/?q=machinery", &cookie)).await;
    assert!(
        body.contains("Select post v2 to change") && body.contains("Computing Machinery") && !body.contains("Rust 50%"),
        "search filters rows"
    );
    assert!(body.contains("1 result of <a href=") && body.contains(">4 total</a>"), "search shows result counts");
    assert!(body.contains("<td>\n                Alan") || body.contains(">Alan<"), "foreign keys show their label in the list");
    let (_, _, body) = send(get("/admin/blog_post/?o=title&f.published=1", &cookie)).await;
    assert!(body.contains("sorted ascending") && !body.contains("xxxxxxxxxx"), "sort + boolean filter");
    assert!(body.contains("icon-yes.svg"), "booleans render as icons");
    let (_, _, body) = send(get("/admin/rangoli_user/", &cookie)).await;
    assert!(body.contains("root") && !body.contains("$argon2"), "password hashes never render");

    let (status, _, body) = send(form("/admin/blog_author/add", &cookie, "name=&email=x%40example.com")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("Please correct the error below.") && body.contains("This field is required."));
    let (status, _, body) = send(form("/admin/blog_author/add", &cookie, "name=Nul%00Byte&email=n%40example.com")).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && body.contains("Null characters are not allowed."),
        "NUL is refused (Postgres can't store it)"
    );
    let (status, _, _) = send(get("/admin/blog_author/?q=a%00b", &cookie)).await;
    assert_eq!(status, StatusCode::OK, "a NUL in a search term is ignored, not a 500");
    let (status, _, body) = send(form("/admin/blog_author/add", &cookie, "name=Grace&email=ada%40example.com")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("already exists"));
    let (status, headers, _) = send(form("/admin/blog_author/add", &cookie, "name=Grace&email=grace%40example.com&_continue=1")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let landing = headers[header::LOCATION].to_str().unwrap().to_string();
    let grace_url = landing.split('?').next().unwrap().to_string();

    let (_, _, body) = send(get(&landing, &cookie)).await;
    assert!(body.contains("value=\"Grace\""));
    assert!(body.contains("was added successfully. You may edit it again below."), "Django-style success message");
    let (status, headers, _) = send(form(&grace_url, &cookie, "name=Grace+Hopper&email=grace%40example.com")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, body) = send(get(headers[header::LOCATION].to_str().unwrap(), &cookie)).await;
    assert!(body.contains("The author “<a href=") && body.contains("Grace Hopper</a>” was changed successfully."));
    assert_eq!(Author::objects().filter(Author::NAME.eq("Grace Hopper")).count().await.unwrap(), 1);

    let (_, _, body) = send(get(&format!("{grace_url}history"), &cookie)).await;
    assert!(body.contains("Added.") && body.contains("Changed name."), "history records each change: {body}");
    let (_, _, body) = send(get("/admin/", &cookie)).await;
    assert!(body.contains("class=\"changelink\"><a href=") && body.contains("Grace Hopper</a>"), "recent actions");

    let (_, _, body) = send(get(&format!("/admin/blog_post/{}/", long.id.unwrap()), &cookie)).await;
    assert!(body.contains(">Grace Hopper</option>") && body.contains(">Alan</option>"), "foreign keys render as a select");
    assert!(body.contains("type=\"datetime-local\"") && body.contains("value=\"2026-01-02T03:04\""), "datetime input");
    assert!(body.contains("name=\"tags\" id=\"id_tags\" multiple") && body.contains(">rust</option>"), "many-to-many multi-select");
    assert!(!body.contains("name=\"created_at\"") && !body.contains("name=\"updated_at\""), "auto fields are not editable");
    // ModelAdmin settings
    let (_, _, body) = send(get("/admin/blog_post/", &cookie)).await;
    assert!(body.contains("class=\"sorted ascending\"") && body.contains(">Title</a>"), "default ordering");
    assert!(body.contains(">Views</a>") && !body.contains(">Rating</a>"), "list_display picks the columns");
    assert!(body.contains("class=\"this-page\"") && body.contains("4 post v2s"), "list_per_page paginates");
    let (_, _, body) = send(get("/admin/blog_post/?q=Body+of", &cookie)).await;
    assert!(body.contains("0 results of"), "search_fields limits what is searched");
    let (_, _, body) = send(get(&format!("/admin/blog_post/?f.author_id={}", alan.id.unwrap()), &cookie)).await;
    assert!(body.contains("By author") && body.contains("2 results of"), "foreign key filter");
    let post_url = format!("/admin/blog_post/{}/", long.id.unwrap());
    let (_, _, body) = send(get(&post_url, &cookie)).await;
    assert!(body.contains("<div class=\"readonly\">7</div>") && !body.contains("name=\"views\""), "read-only field");
    assert!(body.contains(r#"type="date" class="narrow" value="1969-07-20""#), "a native date picker");
    assert!(body.contains(r#"inputmode="decimal" class="narrow" value="19.99""#), "a decimal input");
    let (status, _, body) =
        send(form(&post_url, &cookie, &format!("title=x&body=b&author_id={}&status=bogus&meta=%7Bnope&price=1.234", alan.id.unwrap())))
            .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("Select a valid choice. bogus is not one of the available choices."), "choices are validated");
    assert!(body.contains("Enter valid JSON"), "JSON is validated");
    assert!(body.contains("Enter a number with at most 2 decimal places."), "decimal places are validated");
    let (_, _, body) = send(get("/admin/blog_post/add", &cookie)).await;
    assert!(body.contains("<option value=\"draft\" selected>Draft</option>"), "new forms start from the default");
    let edit = format!(
        "title=Long&body=b&published=on&author_id={}&views=999&tags={}&tags={}&status=published&meta=%7B%22a%22%3A1%7D&due=1969-07-20&price=19.99",
        alan.id.unwrap(),
        rust.id.unwrap(),
        web.id.unwrap()
    );
    assert_eq!(send(form(&post_url, &cookie, &edit)).await.0, StatusCode::SEE_OTHER);
    let long_now = PostV2::get(long.id.unwrap()).await.unwrap();
    assert_eq!(long_now.views, 7, "posted values for read-only fields are ignored");
    assert_eq!(long_now.tags().ids().await.unwrap(), vec![rust.id.unwrap(), web.id.unwrap()], "admin saves the relation");
    let (_, _, body) = send(get(&format!("{post_url}history"), &cookie)).await;
    assert!(body.contains("Changed title, body, published, publish at, meta and tags."), "relation changes are logged");
    assert_eq!(PostV2::get(long.id.unwrap()).await.unwrap().meta, Some(Json(serde_json::json!({ "a": 1 }))));
    let (_, _, body) = send(get("/admin/blog_post/?f.status=old", &cookie)).await;
    assert!(body.contains("By status") && body.contains("Archived for good") && body.contains("0 results of"), "choice filter");

    // ---- custom admin actions
    let (_, _, body) = send(get("/admin/blog_post/", &cookie)).await;
    assert!(body.contains(r#"<option value="publish">Mark selected as published</option>"#), "custom actions in the menu");
    let drafts: Vec<i64> =
        PostV2::objects().order_by(PostV2::ID.asc()).limit(2).all().await.unwrap().iter().map(|p| p.id.unwrap()).collect();
    PostV2::objects().filter(PostV2::ID.is_in(drafts.clone())).update([PostV2::PUBLISHED.set(false)]).await.unwrap();
    let picked = format!("action=publish&_selected_action={}&_selected_action={}", drafts[0], drafts[1]);
    let (status, headers, _) = send(form("/admin/blog_post/", &cookie, &picked)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, body) = send(get(headers[header::LOCATION].to_str().unwrap(), &cookie)).await;
    assert!(body.contains("Applied “Mark selected as published” to 2 post v2s."), "the admin reports what happened");
    assert!(PostV2::get(drafts[0]).await.unwrap().published && PostV2::get(drafts[1]).await.unwrap().published);
    let (_, _, body) = send(get(&format!("/admin/blog_post/{}/history", drafts[0]), &cookie)).await;
    assert!(body.contains("Ran the “Mark selected as published” action."), "each row's history records the action");
    let views_before = PostV2::get(drafts[0]).await.unwrap().views;
    let (_, headers, _) = send(form("/admin/blog_post/", &cookie, &format!("action=explode&_selected_action={}", drafts[0]))).await;
    let (_, _, body) = send(get(headers[header::LOCATION].to_str().unwrap(), &cookie)).await;
    assert!(body.contains("The “Bump views, then fail” action failed, so nothing was changed."));
    assert_eq!(PostV2::get(drafts[0]).await.unwrap().views, views_before, "a failing action is rolled back");
    let (_, _, body) = send(get("/admin/blog_post/?applied=Your+account+is+locked&n=1", &cookie)).await;
    assert!(!body.contains("account is locked") && !body.contains("Applied"), "a crafted link can't inject a message");
    let (_, headers, _) = send(form("/admin/blog_post/", &cookie, &format!("action=nope&_selected_action={}", drafts[0]))).await;
    assert!(headers[header::LOCATION].to_str().unwrap().ends_with("warn=noaction"), "unknown actions do nothing");

    let (_, _, body) = send(get("/admin/blog_post/?f.created_at=today", &cookie)).await;
    assert!(body.contains("By created at") && body.contains("Past 7 days") && body.contains("4 results of"), "date filter");
    assert!(body.contains(" UTC</td>"), "datetimes display in UTC");

    let (status, _, body) = send(get(&format!("/admin/blog_author/{}/delete", ada.id.unwrap()), &cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("would require deleting these protected related objects") && body.contains("Post v2: Rust 50% off_sale"));
    let (status, _, _) = send(form(&format!("/admin/blog_author/{}/delete", ada.id.unwrap()), &cookie, "")).await;
    assert_eq!(status, StatusCode::CONFLICT, "protected by foreign key");
    let (status, _, body) = send(get(&format!("{grace_url}delete"), &cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Are you sure you want to delete the author “Grace Hopper”?"));
    let (status, _, _) = send(form(&format!("{grace_url}delete"), &cookie, "post=yes")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(Author::objects().count().await.unwrap(), 2);

    // Bulk delete: Django's confirmation step, then the deletion.
    let pick = format!("action=delete_selected&_selected_action={}", long.id.unwrap());
    let (status, _, body) = send(form("/admin/blog_post/", &cookie, &pick)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Are you sure you want to delete the selected post v2s?") && body.contains("Summary"));
    assert_eq!(PostV2::objects().count().await.unwrap(), 4, "nothing deleted before confirming");
    let (status, headers, _) = send(form("/admin/blog_post/", &cookie, &format!("{pick}&post=yes"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, body) = send(get(headers[header::LOCATION].to_str().unwrap(), &cookie)).await;
    assert!(body.contains("Successfully deleted 1 post v2."));
    assert_eq!(PostV2::objects().count().await.unwrap(), 3);
    let (_, headers, _) = send(form("/admin/blog_post/", &cookie, "action=delete_selected")).await;
    assert!(headers[header::LOCATION].to_str().unwrap().ends_with("warn=noitems"));

    // ---- templates and static files
    let (status, _, body) = send(get("/tags", "")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>&lt;Tags &amp; more&gt;</title>"), "templates auto-escape: {body}");
    assert!(body.contains("<li>rust</li><li>web</li>"));
    let (_, _, body) = send(get("/tags?partial=1", "")).await;
    assert_eq!(body, "<li>rust</li><li>web</li>", "page.html#block renders just the partial");
    let (status, headers, body) = send(get("/static/app.css", "")).await;
    assert_eq!(
        (status, headers[header::CONTENT_TYPE].to_str().unwrap(), body.as_str()),
        (StatusCode::OK, "text/css", "body { color: red }")
    );
    assert_eq!(send(get("/static/../templates/base.html", "")).await.0, StatusCode::NOT_FOUND, "no escaping the static dir");
    assert_eq!(send(get("/static/missing.css", "")).await.0, StatusCode::NOT_FOUND);

    // ---- JSON API
    let api = |method: &str, uri: &str, cookie: &str, body: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let json = |body: &str| serde_json::from_str::<serde_json::Value>(body).unwrap();
    let total = PostV2::objects().count().await.unwrap();
    let (status, _, body) = send(get("/api/blog_post/", "")).await;
    assert_eq!(status, StatusCode::OK, "public read");
    let page = json(&body);
    assert_eq!(page["count"], total);
    assert!(page["results"][0]["created_at"].as_str().unwrap().ends_with('Z'), "datetimes are ISO 8601");
    let (_, _, body) = send(get("/api/blog_post/?limit=1&ordering=-title", "")).await;
    let page = json(&body);
    assert_eq!(page["results"].as_array().unwrap().len(), 1);
    assert_eq!(page["next"], "/api/blog_post/?ordering=-title&limit=1&offset=1");
    assert_eq!(page["results"][0]["title"], "Rust 50% off_sale");
    let (_, _, body) = send(get(&format!("/api/blog_post/?author_id={}&published=true", ada.id.unwrap()), "")).await;
    assert_eq!(json(&body)["count"], 2, "typed exact-match filters");
    let (_, _, body) = send(get("/api/blog_post/?search=machinery", "")).await;
    assert_eq!(json(&body)["count"], 1);
    assert_eq!(send(get("/api/blog_post/?nope=1", "")).await.0, StatusCode::BAD_REQUEST);
    let (status, _, body) = send(get("/api/blog_post/999999", "")).await;
    assert_eq!((status, json(&body)["detail"].as_str()), (StatusCode::NOT_FOUND, Some("Not found.")));

    let new_post = format!(
        r#"{{"title": "From the API", "body": "b", "author_id": {}, "views": 1, "publish_at": "2026-05-01T10:00:00Z", "due": "2028-02-29", "price": "0.1"}}"#,
        alan.id.unwrap()
    );
    assert_eq!(send(api("POST", "/api/blog_post/", "", &new_post)).await.0, StatusCode::UNAUTHORIZED, "writes need staff");
    let (status, _, body) = send(api(
        "POST",
        "/api/blog_post/",
        &cookie,
        r#"{"title": "", "views": "many", "bogus": 1, "due": "2027-02-29", "price": "1.005"}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let errs = json(&body);
    assert_eq!(errs["title"][0], "This field may not be blank.");
    assert_eq!(errs["views"][0], "A valid integer is required.");
    assert_eq!(errs["author_id"][0], "This field is required.");
    assert_eq!(errs["bogus"][0], "Unknown field.");
    assert_eq!(errs["due"][0], "Date has wrong format. Use YYYY-MM-DD.", "2027 is not a leap year");
    assert_eq!(errs["price"][0], "A valid number with at most 2 decimal places is required.", "no silent rounding");
    let (status, _, body) = send(api("POST", "/api/blog_post/", &cookie, &new_post)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created = json(&body);
    let new_id = created["id"].as_i64().unwrap();
    assert_eq!(created["publish_at"], "2026-05-01T10:00:00Z");
    assert_eq!(created["due"], "2028-02-29");
    assert_eq!(created["price"], "0.10", "a decimal string in, an exact string out (like DRF)");
    let float = new_post.replace(r#""price": "0.1""#, r#""price": 0.1"#);
    let (status, _, body) = send(api("POST", "/api/blog_post/", &cookie, &float)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a fractional JSON number is a float, so it's refused: {body}");
    let public_id = created["public_id"].as_str().and_then(Uuid::parse).expect("API objects carry their UUID");
    let (_, _, body) = send(get(&format!("/api/blog_post/?public_id={public_id}"), "")).await;
    assert_eq!(json(&body)["count"], 1, "filter by UUID");
    let (status, _, body) = send(get("/api/blog_post/?public_id=not-a-uuid", "")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a malformed UUID filter is a 400: {body}");
    let (_, _, body) = send(get("/api/blog_post/?price=0.1", "")).await;
    assert_eq!(json(&body)["count"], 1, "filter by decimal");
    let (_, _, body) = send(get("/api/blog_post/?due=2028-02-29", "")).await;
    assert_eq!(json(&body)["count"], 1, "filter by date");
    assert_eq!(created["tags"], serde_json::json!([]));
    assert_eq!(created["status"], "draft", "missing fields take the model default");
    let bad = format!(r#"{{"title": "t", "body": "b", "author_id": {}, "views": 0, "status": "nope"}}"#, alan.id.unwrap());
    let (status, _, body) = send(api("POST", "/api/blog_post/", &cookie, &bad)).await;
    assert_eq!((status, json(&body)["status"][0].as_str()), (StatusCode::BAD_REQUEST, Some("\"nope\" is not a valid choice.")));
    let (_, _, body) =
        send(api("PATCH", &format!("/api/blog_post/{}", created["id"]), &cookie, r#"{"meta": {"nested": [true, null]}}"#)).await;
    assert_eq!(json(&body)["meta"]["nested"][0], true, "JSON fields are real JSON in the API");
    assert!(created["created_at"].as_str().is_some(), "auto_now_add is filled in");
    let (status, _, body) = send(api("PATCH", &format!("/api/blog_post/{new_id}"), &cookie, r#"{"views": 42}"#)).await;
    assert_eq!((status, json(&body)["views"].as_i64(), json(&body)["title"].as_str()), (StatusCode::OK, Some(42), Some("From the API")));
    let tag_body = format!(r#"{{"tags": [{}, {}]}}"#, web.id.unwrap(), rust.id.unwrap());
    let (_, _, body) = send(api("PATCH", &format!("/api/blog_post/{new_id}"), &cookie, &tag_body)).await;
    assert_eq!(json(&body)["tags"], serde_json::json!([rust.id.unwrap(), web.id.unwrap()]), "relations are writable");
    let (_, _, body) = send(api("PATCH", &format!("/api/blog_post/{new_id}"), &cookie, r#"{"views": 43}"#)).await;
    assert_eq!(json(&body)["tags"].as_array().unwrap().len(), 2, "PATCH leaves relations it doesn't mention");
    let (_, _, body) = send(get("/api/blog_post/?search=From+the+API", "")).await;
    assert_eq!(json(&body)["results"][0]["tags"].as_array().unwrap().len(), 2, "lists include relations");
    let (status, _, body) = send(api("PATCH", &format!("/api/blog_post/{new_id}"), &cookie, r#"{"tags": "rust"}"#)).await;
    assert_eq!((status, json(&body)["tags"][0].as_str()), (StatusCode::BAD_REQUEST, Some("Expected a list of ids.")));
    assert_eq!(
        send(api("PUT", &format!("/api/blog_post/{new_id}"), &cookie, r#"{"views": 1}"#)).await.0,
        StatusCode::BAD_REQUEST,
        "PUT needs every field"
    );
    let (status, _, body) =
        send(api("POST", "/api/blog_post/", &cookie, r#"{"title": "a\u0000b", "body": "b", "author_id": 1, "views": 0}"#)).await;
    assert_eq!((status, json(&body)["title"][0].as_str()), (StatusCode::BAD_REQUEST, Some("Null characters are not allowed.")));
    assert_eq!(send(get("/api/blog_post/?title=a%00b", "")).await.0, StatusCode::BAD_REQUEST, "NUL in an exact filter is a 400");
    let orphan = r#"{"title": "x", "body": "b", "author_id": 424242, "views": 0, "price": "1"}"#;
    let (status, _, body) = send(api("POST", "/api/blog_post/", &cookie, orphan)).await;
    assert!(status == StatusCode::BAD_REQUEST && body.contains("non_field_errors"), "foreign key errors are 400s: {body}");
    assert_eq!(send(api("DELETE", &format!("/api/blog_post/{new_id}"), &cookie, "")).await.0, StatusCode::NO_CONTENT);
    assert_eq!(send(api("DELETE", &format!("/api/blog_post/{new_id}"), &cookie, "")).await.0, StatusCode::NOT_FOUND);

    assert_eq!(send(get("/api/blog_author/", "")).await.0, StatusCode::UNAUTHORIZED, "authenticated-only read");
    let (status, _, body) = send(get("/api/blog_author/", &cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("password"));
    assert_eq!(send(api("POST", "/api/blog_author/", &cookie, "{}")).await.0, StatusCode::METHOD_NOT_ALLOWED, "write(Access::Nobody)");
    assert_eq!(send(get("/api/rangoli_user/", &cookie)).await.0, StatusCode::NOT_FOUND, "only exposed models are served");

    // ---- API tokens: what a script or mobile app uses (no cookie, no browser headers)
    use rangoli::auth::{self, ApiToken};
    let bearer = |method: &str, uri: &str, token: &str, body: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let root = auth::User::objects().filter(auth::User::USERNAME.eq("root")).get().await.unwrap();
    let (row, token) = auth::create_token(&root, "deploy bot", None).await.unwrap();
    assert!(token.starts_with("rgl_") && token.len() == 44 && row.prefix == token[..12]);
    let stored = serde_json::to_string(&ApiToken::objects().all().await.unwrap().iter().map(|t| t.values()).collect::<Vec<_>>()).unwrap();
    assert!(!stored.contains(&token[4..]), "the token itself is never stored, only its hash");
    let mut by_token = new_post.replace("From the API", "By token");
    by_token = by_token.replace(r#""price": "0.1""#, r#""price": "2.50""#);
    let (status, _, body) = send(bearer("POST", "/api/blog_post/", &token, &by_token)).await;
    assert_eq!(status, StatusCode::CREATED, "a staff token can write without a session: {body}");
    let made = json(&body)["id"].as_i64().unwrap();
    assert_eq!(send(bearer("GET", "/api/blog_author/", &token, "")).await.0, StatusCode::OK, "and read authenticated-only models");
    let (status, headers, body) = send(bearer("GET", "/api/blog_post/", "rgl_0000000000000000000000000000000000000000", "")).await;
    assert_eq!((status, json(&body)["detail"].as_str()), (StatusCode::UNAUTHORIZED, Some("Invalid token.")), "a wrong token is a 401");
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
    assert_eq!(
        send(bearer("GET", "/api/blog_post/", "rgl_garbage", "")).await.0,
        StatusCode::UNAUTHORIZED,
        "even on public endpoints, like DRF"
    );
    assert_eq!(
        send(bearer("GET", "/api/blog_post/", "eyJhbGciOi.app-own-jwt", "")).await.0,
        StatusCode::OK,
        "other bearer schemes pass through to the app"
    );
    let used = ApiToken::get(row.id.unwrap()).await.unwrap().last_used_at;
    assert!(used.is_some(), "last_used_at is recorded");
    let (_, queries) = orm::count_queries(auth::token_user(&token)).await;
    assert_eq!(queries, 2, "within a minute of the last use, checking a token writes nothing (token + user lookups)");
    let (_, short) = auth::create_token(&root, "expired", Some(std::time::Duration::ZERO)).await.unwrap();
    assert_eq!(send(bearer("GET", "/api/blog_post/", &short, "")).await.0, StatusCode::UNAUTHORIZED, "expired tokens stop working");
    let (_, other) = auth::create_token(&root, "to revoke", None).await.unwrap();
    assert_eq!(send(bearer("GET", "/api/blog_author/", &other, "")).await.0, StatusCode::OK);
    ApiToken::objects().filter(ApiToken::NAME.eq("to revoke")).delete().await.unwrap();
    assert_eq!(send(bearer("GET", "/api/blog_author/", &other, "")).await.0, StatusCode::UNAUTHORIZED, "deleting the row revokes it");
    let mut plain = auth::create_user("plain", "correct horse", false).await.unwrap();
    let (_, weak) = auth::create_token(&plain, "weak", None).await.unwrap();
    assert_eq!(
        send(bearer("DELETE", &format!("/api/blog_post/{made}"), &weak, "")).await.0,
        StatusCode::FORBIDDEN,
        "a token has its user's rights"
    );
    plain.is_active = false;
    plain.save().await.unwrap();
    assert_eq!(
        send(bearer("GET", "/api/blog_author/", &weak, "")).await.0,
        StatusCode::UNAUTHORIZED,
        "deactivating the user disables their tokens"
    );
    assert_eq!(send(bearer("DELETE", &format!("/api/blog_post/{made}"), &token, "")).await.0, StatusCode::NO_CONTENT);

    // ---- login lockout: counted in the database, so every instance shares it
    use rangoli::auth::LoginFailure;
    auth::create_user("victim", "correct horse", false).await.unwrap();
    for _ in 0..5 {
        assert!(auth::authenticate("victim", "wrong").await.unwrap().is_none());
    }
    assert!(
        matches!(auth::authenticate("victim", "correct horse").await, Err(Error::Locked)),
        "locked after 5, even with the right password"
    );
    let fails = LoginFailure::objects().filter(LoginFailure::USERNAME.eq("victim")).get().await.unwrap();
    assert_eq!(fails.failures, 6, "the count is a database row (5 failures, then the refused attempt)");
    let (status, _, body) = send(form("/admin/login", "", "username=victim&password=correct+horse")).await;
    assert!(status != StatusCode::SEE_OTHER && body.contains("too many failed logins"), "the login page says so");
    LoginFailure::objects().filter(LoginFailure::USERNAME.eq("victim")).update([LoginFailure::LOCKED_UNTIL.set(0)]).await.unwrap();
    assert!(auth::authenticate("victim", "correct horse").await.unwrap().is_some(), "the lock lifts when its window ends");
    assert_eq!(LoginFailure::objects().filter(LoginFailure::USERNAME.eq("victim")).count().await.unwrap(), 0, "and success clears it");
    // A burst of 50 parallel guesses, as from many instances: each attempt is counted before its
    // password is checked, so at most 5 ever reach the check. (Counting after the check let all 50 through.)
    auth::create_user("burst", "correct horse", false).await.unwrap();
    let racers: Vec<_> = (0..50).map(|_| tokio::spawn(async { auth::authenticate("burst", "wrong").await })).collect();
    let mut checked = 0;
    for r in racers {
        match r.await.unwrap() {
            Ok(None) => checked += 1,
            Err(Error::Locked) => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(checked <= 5, "only 5 of 50 parallel guesses reach the password check, got {checked}");
    let fails = LoginFailure::objects().filter(LoginFailure::USERNAME.eq("burst")).get().await.unwrap();
    assert_eq!(fails.failures, 50, "no lost updates: every attempt is counted");
    assert!(matches!(auth::authenticate("burst", "correct horse").await, Err(Error::Locked)), "the right password is refused too");
    for _ in 0..6 {
        let _ = auth::authenticate("nobody-by-this-name", "x").await;
    }
    assert!(matches!(auth::authenticate("nobody-by-this-name", "x").await, Err(Error::Locked)), "unknown usernames lock like real ones");
    let long = "x".repeat(400); // longer than any username column (found by the fuzzer: a 500 on Postgres and MySQL)
    assert!(auth::authenticate(&long, "x").await.unwrap().is_none(), "an over-long username is a plain failure");

    // ---- per-object permissions: owned_by
    let alice = auth::create_user("alice", "correct horse", false).await.unwrap();
    let bob = auth::create_user("bob", "correct horse", false).await.unwrap();
    let (_, at) = auth::create_token(&alice, "a", None).await.unwrap();
    let (_, bt) = auth::create_token(&bob, "b", None).await.unwrap();
    let (aid, bid) = (alice.id.unwrap(), bob.id.unwrap());
    let (status, _, body) = send(bearer("POST", "/api/e2e_note/", &at, &format!(r#"{{"title": "a1", "owner_id": {bid}}}"#))).await;
    assert_eq!((status, json(&body)["owner_id"].as_i64()), (StatusCode::CREATED, Some(aid)), "a forged owner is overwritten");
    let a1 = json(&body)["id"].as_i64().unwrap();
    let (_, _, body) = send(bearer("POST", "/api/e2e_note/", &bt, r#"{"title": "b1"}"#)).await;
    let b1 = json(&body)["id"].as_i64().unwrap();
    assert_eq!(json(&body)["owner_id"], bid, "the owner is filled in");
    let (_, _, body) = send(bearer("GET", "/api/e2e_note/", &at, "")).await;
    assert_eq!((json(&body)["count"].as_i64(), json(&body)["results"][0]["id"].as_i64()), (Some(1), Some(a1)), "lists show only your rows");
    let (_, _, body) = send(bearer("GET", &format!("/api/e2e_note/?owner_id={bid}"), &at, "")).await;
    assert_eq!(json(&body)["count"], 0, "filters can't widen the scope");
    for (method, body) in [("GET", ""), ("PATCH", r#"{"title": "mine now"}"#), ("PUT", r#"{"title": "mine now"}"#), ("DELETE", "")] {
        let status = send(bearer(method, &format!("/api/e2e_note/{b1}"), &at, body)).await.0;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} on someone else's row is a 404, like a row that doesn't exist");
    }
    let still = Note::get(b1).await.unwrap();
    assert_eq!((still.title.as_str(), still.owner_id), ("b1", bid), "and nothing changed");
    let (_, _, body) = send(bearer("PATCH", &format!("/api/e2e_note/{a1}"), &at, &format!(r#"{{"owner_id": {bid}}}"#))).await;
    assert_eq!(json(&body)["owner_id"], aid, "you can't give a row away to someone else");
    assert_eq!(send(get("/api/e2e_note/", "")).await.0, StatusCode::UNAUTHORIZED);
    let forged = format!(r#"{{"title": "forged", "owner_id": {aid}}}"#);
    assert_eq!(
        send(api("POST", "/api/e2e_note/", "", &forged)).await.0,
        StatusCode::UNAUTHORIZED,
        "anonymous can't create owned rows, even with write(Public)"
    );
    let wrong = std::panic::catch_unwind(|| Api::<Post>::new().owned_by(&Post::AUTHOR_ID));
    assert!(wrong.is_err(), "owned_by on a column that isn't a user foreign key stops the app at startup");
    let (_, _, body) = send(bearer("GET", "/api/e2e_note/", &token, "")).await;
    assert_eq!(json(&body)["count"], 2, "staff see every row");
    let (_, _, body) = send(bearer("PATCH", &format!("/api/e2e_note/{b1}"), &token, &format!(r#"{{"owner_id": {aid}}}"#))).await;
    assert_eq!(json(&body)["owner_id"], aid, "staff can reassign");
    assert_eq!(send(bearer("DELETE", &format!("/api/e2e_note/{b1}"), &at, "")).await.0, StatusCode::NO_CONTENT, "it's alice's now");

    // ---- MCP: AI agents use the same API, as a token's user, through tools
    let rpc = |method: &str, params: serde_json::Value| {
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()
    };
    let (status, _, body) = send(bearer(
        "POST",
        "/mcp",
        &token,
        &rpc(
            "initialize",
            serde_json::json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "1" } }),
        ),
    ))
    .await;
    assert_eq!((status, json(&body)["result"]["protocolVersion"].as_str()), (StatusCode::OK, Some("2025-06-18")), "{body}");
    assert_eq!(json(&body)["result"]["serverInfo"]["name"], "rangoli");
    let note = serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
    assert_eq!(send(bearer("POST", "/mcp", &token, &note)).await.0, StatusCode::ACCEPTED, "notifications get 202, no body");
    let names = |body: &str| -> Vec<String> {
        json(body)["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    let (_, _, body) = send(api("POST", "/mcp", "", &rpc("tools/list", serde_json::json!({})))).await;
    let anon = names(&body);
    assert!(
        anon.contains(&"blog_post_list".to_string()) && !anon.contains(&"blog_post_create".to_string()),
        "anonymous agents get the public read tools only: {anon:?}"
    );
    assert!(!anon.iter().any(|n| n.starts_with("e2e_note")), "and nothing they can't use");
    let (_, _, body) = send(bearer("POST", "/mcp", &token, &rpc("tools/list", serde_json::json!({})))).await;
    let staff_tools = names(&body);
    assert!(
        ["blog_post_create", "blog_post_update", "blog_post_delete", "e2e_note_list"].iter().all(|t| staff_tools.contains(&t.to_string())),
        "{staff_tools:?}"
    );
    let create_tool = json(&body)["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "blog_post_create").unwrap().clone();
    assert_eq!(create_tool["inputSchema"]["properties"]["price"]["format"], "decimal", "tool schemas come from the model");
    assert_eq!(create_tool["inputSchema"]["properties"]["public_id"], serde_json::Value::Null, "auto fields aren't inputs");
    let call = |name: &str, args: serde_json::Value| rpc("tools/call", serde_json::json!({ "name": name, "arguments": args }));
    let args =
        serde_json::json!({ "title": "Written by an agent", "body": "b", "author_id": alan.id.unwrap(), "views": 0, "price": "4.20" });
    let (_, _, body) = send(bearer("POST", "/mcp", &token, &call("blog_post_create", args))).await;
    let result = &json(&body)["result"];
    assert_eq!(result["isError"], false, "{body}");
    let agent_post = result["structuredContent"]["id"].as_i64().unwrap();
    assert_eq!(result["structuredContent"]["price"], "4.20");
    let (_, _, body) =
        send(bearer("POST", "/mcp", &token, &call("blog_post_update", serde_json::json!({ "id": agent_post, "views": 9 })))).await;
    assert_eq!(json(&body)["result"]["structuredContent"]["views"], 9);
    let (_, _, body) =
        send(bearer("POST", "/mcp", &token, &call("blog_post_list", serde_json::json!({ "search": "Written by an agent" })))).await;
    assert_eq!(json(&body)["result"]["structuredContent"]["count"], 1);
    let (_, _, body) = send(bearer("POST", "/mcp", &token, &call("blog_post_create", serde_json::json!({ "title": "" })))).await;
    let bad = &json(&body)["result"];
    assert!(
        bad["isError"] == true && bad["structuredContent"]["error"]["title"][0] == "This field may not be blank.",
        "validation errors reach the agent: {body}"
    );
    let (_, _, history) = send(get(&format!("/admin/blog_post/{agent_post}/history"), &cookie)).await;
    assert!(history.contains("Added via MCP.") && history.contains("Changed via MCP."), "every agent change is in the admin History");
    let (_, _, body) = send(bearer("POST", "/mcp", &token, &call("blog_post_delete", serde_json::json!({ "id": agent_post })))).await;
    assert_eq!(json(&body)["result"]["structuredContent"]["deleted"], true);
    let deleted = orm::count_queries(async {
        rangoli::admin::LogEntry::objects()
            .filter(rangoli::admin::LogEntry::OBJECT_ID.eq(agent_post) & rangoli::admin::LogEntry::ACTION.eq(3))
            .count()
            .await
            .unwrap()
    })
    .await
    .0;
    assert_eq!(deleted, 1, "deletes are logged too");
    // owned_by holds for agents: alice's agent sees and changes only alice's notes.
    let (_, _, body) = send(bearer("POST", "/mcp", &at, &call("e2e_note_list", serde_json::json!({})))).await;
    let mine = &json(&body)["result"]["structuredContent"];
    assert!(mine["results"].as_array().unwrap().iter().all(|n| n["owner_id"] == aid), "{mine}");
    let (_, _, body) = send(bearer("POST", "/mcp", &bt, &call("e2e_note_delete", serde_json::json!({ "id": a1 })))).await;
    assert_eq!(
        (json(&body)["result"]["isError"].as_bool(), json(&body)["result"]["structuredContent"]["status"].as_i64()),
        (Some(true), Some(404)),
        "bob's agent can't touch alice's note"
    );
    let (_, _, body) = send(bearer("POST", "/mcp", &token, &call("nope_list", serde_json::json!({})))).await;
    assert_eq!(json(&body)["error"]["code"], -32602, "unknown tools are a JSON-RPC error");
    let (_, _, body) = send(bearer("POST", "/mcp", &token, &rpc("bogus/method", serde_json::json!({})))).await;
    assert_eq!(json(&body)["error"]["code"], -32601);

    // ---- the admin's "Connect an AI agent" page: a token shown once, with client setup
    let (status, _, page) = send(get("/admin/agents/", &cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, home) = send(get("/admin/", &cookie)).await;
    assert!(home.contains("/admin/agents/"), "the dashboard links to it");
    let (status, _, page2) = send(form("/admin/agents/", &cookie, "name=Claude+on+my+laptop&days=30&allow_writes=on")).await;
    assert_eq!(status, StatusCode::OK);
    let shown: String = page2.split("rgl_").nth(1).map(|t| format!("rgl_{}", &t[..40])).expect("token on the page");
    assert!(
        page2.contains("claude mcp add --transport http") && page2.contains("/mcp") && page2.contains("mcpServers"),
        "setup for Claude Code and Cursor"
    );
    assert!(!page.contains(&shown), "a token appears only on the page that created it");
    let (_, _, body) = send(bearer("POST", "/mcp", &shown, &rpc("tools/list", serde_json::json!({})))).await;
    assert!(names(&body).contains(&"blog_post_create".to_string()), "and it works for an agent at once");
    assert_eq!(send(form("/admin/agents/", &cookie, "name=")).await.1.get(header::LOCATION), None, "a name is required");
    let (_, _, again) = send(get("/admin/agents/", &cookie)).await;
    assert!(again.contains("Claude on my laptop") && !again.contains(&shown), "listed afterwards by name, never by token");
    assert_eq!(send(get("/admin/agents/", "")).await.0, StatusCode::SEE_OTHER, "staff only");

    // ---- guardrails: agents are read-only unless allowed, scoped to tables, kept out of the admin
    let token_on = |page: &str| page.split("rgl_").nth(1).map(|t| format!("rgl_{}", &t[..40])).expect("token on the page");
    let (_, _, page3) = send(form("/admin/agents/", &cookie, "name=Careful+agent&days=7")).await;
    let careful = token_on(&page3);
    assert!(page3.contains("change nothing"), "the page says what the agent can do");
    let (_, _, body) = send(bearer("POST", "/mcp", &careful, &rpc("tools/list", serde_json::json!({})))).await;
    let ro_tools = names(&body);
    assert!(
        ro_tools.contains(&"blog_post_list".to_string()) && !ro_tools.iter().any(|t| t.ends_with("_create") || t.ends_with("_delete")),
        "read-only by default: {ro_tools:?}"
    );
    let blocked = serde_json::json!({ "title": "nope", "body": "b", "author_id": alan.id.unwrap(), "views": 0, "price": "1" }).to_string();
    let (status, _, body) = send(bearer("POST", "/api/blog_post/", &careful, &blocked)).await;
    assert_eq!(
        (status, json(&body)["detail"].as_str()),
        (StatusCode::FORBIDDEN, Some("This token is read-only.")),
        "enforced by the API, not just hidden"
    );
    let (_, _, body) = send(bearer("POST", "/mcp", &careful, &call("blog_post_create", serde_json::from_str(&blocked).unwrap()))).await;
    assert_eq!(json(&body)["result"]["structuredContent"]["status"], 403, "and over MCP even when the agent calls a tool it wasn't shown");
    let root_user = auth::User::objects().filter(auth::User::USERNAME.eq("root")).get().await.unwrap();
    let notes_only = auth::TokenScope { tables: Some(vec!["e2e_note".into()]), ..Default::default() };
    let (_, narrow) = auth::create_scoped_token(&root_user, "notes bot", None, &notes_only).await.unwrap();
    assert_eq!(
        send(bearer("GET", "/api/blog_post/", &narrow, "")).await.0,
        StatusCode::NOT_FOUND,
        "other tables look like they don't exist"
    );
    assert_eq!(send(bearer("GET", "/api/e2e_note/", &narrow, "")).await.0, StatusCode::OK);
    let (_, _, body) = send(bearer("POST", "/mcp", &narrow, &rpc("tools/list", serde_json::json!({})))).await;
    assert!(names(&body).iter().all(|t| t.starts_with("e2e_note_")), "only its tables' tools: {:?}", names(&body));
    let (status, _, body) = send(bearer("GET", "/admin/", &token, "")).await;
    assert_eq!(
        (status, json(&body)["detail"].as_str()),
        (StatusCode::FORBIDDEN, Some("API tokens can't open the admin.")),
        "even a staff token"
    );

    // ---- private fields never leave the server through the API or MCP (django-mcp-server #66/#72 leaked theirs)
    let (_, _, body) = send(bearer("POST", "/api/e2e_note/", &at, r#"{"title": "has a secret"}"#)).await;
    let sid = json(&body)["id"].as_i64().unwrap();
    Note::objects().filter(Note::ID.eq(sid)).update([Note::SECRET.set("hunter2".to_string())]).await.unwrap();
    let (_, _, body) = send(bearer("GET", &format!("/api/e2e_note/{sid}"), &at, "")).await;
    assert!(json(&body).get("secret").is_none() && !body.contains("hunter2"), "not in responses: {body}");
    let (_, _, body) = send(bearer("GET", "/api/e2e_note/", &at, "")).await;
    assert!(!body.contains("hunter2"), "not in lists");
    for probe in ["?secret=hunter2", "?secret__startswith=h", "?ordering=secret", "?ordering=-secret"] {
        assert_eq!(
            send(bearer("GET", &format!("/api/e2e_note/{probe}"), &at, "")).await.0,
            StatusCode::BAD_REQUEST,
            "{probe} can't probe it"
        );
    }
    let (_, _, body) = send(bearer("GET", "/api/e2e_note/?search=hunter2", &at, "")).await;
    assert_eq!(json(&body)["count"], 0, "search doesn't look inside it");
    let (status, _, body) = send(bearer("PATCH", &format!("/api/e2e_note/{sid}"), &at, r#"{"secret": "changed"}"#)).await;
    assert_eq!((status, json(&body)["secret"][0].as_str()), (StatusCode::BAD_REQUEST, Some("Unknown field.")), "nor written");
    assert_eq!(Note::get(sid).await.unwrap().secret.as_deref(), Some("hunter2"));
    let (_, _, body) = send(bearer("POST", "/mcp", &at, &call("e2e_note_get", serde_json::json!({ "id": sid })))).await;
    assert!(!body.contains("hunter2"), "agents don't see it either");
    let (_, _, body) = send(bearer("POST", "/mcp", &at, &rpc("tools/list", serde_json::json!({})))).await;
    assert!(!body.contains("\"secret\""), "or know it exists");
    let (_, _, schema) = send(get("/api/schema.json", "")).await;
    assert!(!schema.contains("\"secret\""), "nor does the OpenAPI document");

    // ---- approvals: an "ask me first" agent's writes wait for a person
    let (_, _, page4) = send(form("/admin/agents/", &cookie, "name=Cautious+writer&days=7&allow_writes=on&ask_first=on")).await;
    let asker = token_on(&page4);
    assert!(page4.contains("waits for your approval"), "the page explains it");
    let wanted = serde_json::json!({ "title": "Needs a human", "body": "b", "author_id": alan.id.unwrap(), "views": 0, "price": "1" });
    let (_, _, body) = send(bearer("POST", "/mcp", &asker, &call("blog_post_create", wanted.clone()))).await;
    let held = &json(&body)["result"]["structuredContent"];
    assert_eq!(held["pending"], true, "{body}");
    let approval = held["approval_id"].as_i64().unwrap();
    let titled = |t: &'static str| PostV2::objects().filter(PostV2::TITLE.eq(t)).count();
    assert_eq!(titled("Needs a human").await.unwrap(), 0, "nothing is written yet");
    let (_, _, body) = send(bearer("POST", "/mcp", &asker, &call("approval_status", serde_json::json!({ "approval_id": approval })))).await;
    assert_eq!(json(&body)["result"]["structuredContent"]["status"], "pending");
    let (_, _, body) = send(bearer("POST", "/mcp", &at, &call("approval_status", serde_json::json!({ "approval_id": approval })))).await;
    assert_eq!(json(&body)["result"]["structuredContent"]["error"], "No such approval for you.", "other users can't see it");
    let (status, _, body) = send(bearer("POST", "/api/blog_post/", &asker, r#"{"title": ""}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "invalid requests fail at once, they aren't queued: {body}");
    let (_, _, queue_page) = send(get("/admin/rangoli_pending_change/", &cookie)).await;
    assert!(
        queue_page.contains("Add a blog post") && queue_page.contains("Cautious writer") && queue_page.contains("Approve selected changes")
    );
    let (_, headers, _) =
        send(form("/admin/rangoli_pending_change/", &cookie, &format!("action=approve&_selected_action={approval}"))).await;
    let (_, _, done) = send(get(headers[header::LOCATION].to_str().unwrap(), &cookie)).await;
    assert!(done.contains("Applied “Approve selected changes” to 1 pending change."), "the admin confirms");
    assert_eq!(titled("Needs a human").await.unwrap(), 1, "approved: now it's written");
    let made = PostV2::objects().filter(PostV2::TITLE.eq("Needs a human")).get().await.unwrap().id.unwrap();
    let (_, _, history) = send(get(&format!("/admin/blog_post/{made}/history"), &cookie)).await;
    assert!(history.contains("Added via MCP, approved by root."), "History names who approved it");
    let (_, _, body) = send(bearer("POST", "/mcp", &asker, &call("approval_status", serde_json::json!({ "approval_id": approval })))).await;
    let st = &json(&body)["result"]["structuredContent"];
    assert!(st["status"] == "approved" && st["result"]["title"] == "Needs a human", "the agent can see the outcome: {st}");
    let (status, _, body) = send(bearer("DELETE", &format!("/api/blog_post/{made}"), &asker, "")).await;
    assert_eq!((status, json(&body)["pending"].as_bool()), (StatusCode::ACCEPTED, Some(true)), "deletes wait too, over the API as well");
    let delete_id = json(&body)["approval_id"].as_i64().unwrap();
    assert_eq!(
        send(bearer("DELETE", "/api/blog_post/999999", &asker, "")).await.0,
        StatusCode::NOT_FOUND,
        "a delete of nothing isn't queued"
    );
    send(form("/admin/rangoli_pending_change/", &cookie, &format!("action=reject&_selected_action={delete_id}"))).await;
    assert_eq!(titled("Needs a human").await.unwrap(), 1, "rejected: the post survives");
    let decided = rangoli::approvals::PendingChange::get(delete_id).await.unwrap();
    assert_eq!(decided.status, rangoli::approvals::ApprovalStatus::Rejected);
    send(form("/admin/rangoli_pending_change/", &cookie, &format!("action=approve&_selected_action={delete_id}"))).await;
    assert_eq!(titled("Needs a human").await.unwrap(), 1, "a decided change can't be approved later");

    // ---- rate limits: a runaway client or agent gets 429s, not the database
    // The limit is per calendar minute: don't straddle a minute boundary, or the count resets mid-test.
    let into_minute = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() % 60;
    if into_minute > 54 {
        tokio::time::sleep(std::time::Duration::from_secs(61 - into_minute)).await;
    }
    let limited = App::new()
        .api::<Note>(Api::new().read(Access::Authenticated).write(Access::Authenticated).owned_by(&Note::OWNER_ID))
        .rate_limit(2)
        .mcp_rate_limit(Some(2))
        .router();
    let send_to = |req: Request<Body>| {
        let app = limited.clone();
        async move {
            let res = app.oneshot(req).await.unwrap();
            (res.status(), res.headers().clone())
        }
    };
    for _ in 0..2 {
        assert_eq!(send_to(bearer("GET", "/api/e2e_note/", &at, "")).await.0, StatusCode::OK);
    }
    let (status, headers) = send_to(bearer("GET", "/api/e2e_note/", &at, "")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "the 3rd in a minute is refused");
    let wait: i64 = headers[header::RETRY_AFTER].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&wait), "with Retry-After: {wait}");
    assert_eq!(send_to(bearer("GET", "/api/e2e_note/", &bt, "")).await.0, StatusCode::OK, "other users have their own count");
    for _ in 0..2 {
        assert_eq!(send_to(bearer("POST", "/mcp", &at, &rpc("tools/list", serde_json::json!({})))).await.0, StatusCode::OK);
    }
    assert_eq!(
        send_to(bearer("POST", "/mcp", &at, &rpc("tools/list", serde_json::json!({})))).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "MCP too"
    );

    // ---- CORS: listed browser apps may call the API, everyone else is still blocked
    let cors_app = |origins: Vec<&str>| {
        App::new()
            .api::<Note>(Api::new().read(Access::Public).write(Access::Authenticated).owned_by(&Note::OWNER_ID))
            .cors(origins)
            .router()
    };
    let spa = cors_app(vec!["https://app.example.com"]);
    let browser = |method: &str, origin: &str, extra: &[(&str, &str)]| {
        let mut b =
            Request::builder().method(method).uri("/api/e2e_note/").header(header::ORIGIN, origin).header("sec-fetch-site", "cross-site");
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        b.header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(if method == "POST" { r#"{"title": "from the SPA"}"# } else { "" }))
            .unwrap()
    };
    let run = |app: axum::Router, req: Request<Body>| async move { app.oneshot(req).await.unwrap() };
    let pre = run(
        spa.clone(),
        browser(
            "OPTIONS",
            "https://app.example.com",
            &[("access-control-request-method", "POST"), ("access-control-request-headers", "authorization, content-type")],
        ),
    )
    .await;
    assert_eq!(pre.status(), StatusCode::NO_CONTENT, "preflight answered");
    assert_eq!(pre.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "https://app.example.com");
    assert!(pre.headers()[header::ACCESS_CONTROL_ALLOW_METHODS].to_str().unwrap().contains("POST"));
    let auth = format!("Bearer {at}");
    let write = run(spa.clone(), browser("POST", "https://app.example.com", &[("authorization", &auth)])).await;
    assert_eq!(write.status(), StatusCode::CREATED, "a listed origin may write, cross-site");
    assert_eq!(write.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "https://app.example.com");
    let evil = run(spa.clone(), browser("POST", "https://evil.example", &[("authorization", &auth)])).await;
    assert_eq!(evil.status(), StatusCode::FORBIDDEN, "an unlisted origin is still blocked");
    assert!(evil.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    let evil_pre = run(spa.clone(), browser("OPTIONS", "https://evil.example", &[("access-control-request-method", "POST")])).await;
    assert!(evil_pre.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none(), "and gets no preflight approval");
    let open = cors_app(vec!["*"]);
    let read = run(open.clone(), browser("GET", "https://anyone.example", &[])).await;
    assert_eq!(
        (read.status(), read.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN].to_str().unwrap()),
        (StatusCode::OK, "*"),
        "* allows reads"
    );
    assert_eq!(
        run(open, browser("POST", "https://anyone.example", &[("authorization", &auth)])).await.status(),
        StatusCode::FORBIDDEN,
        "but never writes"
    );

    // Tokens in the admin: listed and revocable, never created there (the plaintext couldn't be shown).
    let (status, _, body) = send(get("/admin/rangoli_api_token/", &cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("deploy bot") && body.contains(&row.prefix), "tokens are listed by name and prefix");
    assert!(!body.contains("/admin/rangoli_api_token/add"), "no Add button or sidebar link");
    assert_eq!(send(get("/admin/rangoli_api_token/add", &cookie)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        send(form("/admin/rangoli_api_token/add", &cookie, "name=x")).await.0,
        StatusCode::FORBIDDEN,
        "no_add() is enforced, not just hidden"
    );
    let (_, _, index) = send(get("/admin/", &cookie)).await;
    assert!(index.contains("/admin/blog_author/add") && !index.contains("/admin/rangoli_api_token/add"));
    let token_url = format!("/admin/rangoli_api_token/{}/", row.id.unwrap());
    let (_, _, page) = send(get(&token_url, &cookie)).await;
    assert!(!page.contains("_addanother") && !page.contains(r#"name="key_hash""#), "hash is read-only, no add-another");
    let (status, _, _) = send(form(&token_url, &cookie, "name=deploy+bot&expires_at=2001-01-01T00%3A00&key_hash=forged")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(ApiToken::get(row.id.unwrap()).await.unwrap().key_hash, row.key_hash, "posted read-only fields are ignored");
    assert_eq!(
        send(bearer("GET", "/api/blog_author/", &token, "")).await.0,
        StatusCode::UNAUTHORIZED,
        "an expiry set in the admin revokes it"
    );

    let (_, _, body) = send(get("/api/schema.json", "")).await;
    let schema = json(&body);
    assert_eq!(schema["openapi"], "3.0.3");
    assert!(schema["paths"]["/api/blog_post/{id}"]["patch"].is_object());
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["publish_at"]["format"], "date-time");
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["due"]["format"], "date");
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["price"]["format"], "decimal");
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["created_at"]["readOnly"], true);
    let uuid_schema = &schema["components"]["schemas"]["PostV2"]["properties"]["public_id"];
    assert_eq!((uuid_schema["format"].as_str(), uuid_schema["readOnly"].as_bool()), (Some("uuid"), Some(true)));
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["tags"]["type"], "array");
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["status"]["enum"], serde_json::json!(["draft", "published", "old"]));
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["status"]["default"], "draft");

    // ---- background tasks
    let greet = Greet { name: "Ada".into() }.enqueue().await.unwrap();
    let later = Greet { name: "Later".into() }.enqueue_in(std::time::Duration::from_secs(3600)).await.unwrap();
    assert_eq!(tasks::run_due(10).await.unwrap(), 1, "only due runs run");
    assert_eq!(*GREETED.lock().unwrap(), vec!["Ada".to_string()]);
    let done = TaskRecord::get(greet).await.unwrap();
    assert_eq!((done.status, done.attempts, done.finished_at.is_some()), (TaskStatus::Done, 1, true));
    assert_eq!(TaskRecord::get(later).await.unwrap().status, TaskStatus::Queued);

    let flaky = Flaky.enqueue().await.unwrap();
    tasks::run_due(10).await.unwrap();
    let after_fail = TaskRecord::get(flaky).await.unwrap();
    assert_eq!(
        (after_fail.status, after_fail.attempts, after_fail.last_error.as_deref()),
        (TaskStatus::Queued, 1, Some("task error: boom"))
    );
    assert!(after_fail.run_at.unix() >= DateTime::now().unix() + 9, "retries back off");
    let in_two_hours = DateTime::from_unix(DateTime::now().unix() + 7200);
    tasks::run_due_at(in_two_hours, 10).await.unwrap();
    let recovered = TaskRecord::get(flaky).await.unwrap();
    assert_eq!((recovered.status, recovered.attempts, recovered.last_error), (TaskStatus::Done, 2, None), "a retry succeeds");
    assert!(GREETED.lock().unwrap().contains(&"Later".to_string()), "the delayed run ran once due");

    let boom = Explodes.enqueue().await.unwrap();
    tasks::run_due(10).await.unwrap();
    let failed = TaskRecord::get(boom).await.unwrap();
    assert_eq!(failed.status, TaskStatus::Failed, "MAX_ATTEMPTS = 1");
    assert!(failed.last_error.unwrap().contains("panicked: kaboom"), "panics are caught and recorded");

    let unknown = tasks::enqueue_raw("not_registered", "{}".into(), DateTime::now(), 3).await.unwrap();
    tasks::run_due(10).await.unwrap();
    assert_eq!(TaskRecord::get(unknown).await.unwrap().status, TaskStatus::Failed, "an unknown task fails instead of retrying");

    // A worker that died mid-run leaves the record `running`; after the lease it is retried.
    let stuck = Greet { name: "Stuck".into() }.enqueue().await.unwrap();
    let hour_ago = DateTime::from_unix(DateTime::now().unix() - 3600);
    TaskRecord::objects()
        .filter(TaskRecord::ID.eq(stuck))
        .update([TaskRecord::STATUS.set(TaskStatus::Running), TaskRecord::STARTED_AT.set(hour_ago)])
        .await
        .unwrap();
    tasks::run_due(10).await.unwrap();
    assert_eq!(TaskRecord::get(stuck).await.unwrap().status, TaskStatus::Done, "stale runs are recovered");

    let (_, _, body) = send(get("/admin/rangoli_task/?f.status=failed", &cookie)).await;
    assert!(
        body.contains("Select task record to change") && body.contains("2 results of") && body.contains("By status"),
        "tasks in the admin"
    );

    // ---- schedules: 20 workers, each with its own memory (as if separate servers), see one tick
    let tick = DateTime::parse("2030-01-01T00:05").unwrap();
    let racers: Vec<_> =
        (0..20).map(|_| tokio::spawn(async move { tasks::queue_schedules(tick, &mut Default::default()).await.unwrap() })).collect();
    let mut queued = 0;
    for r in racers {
        queued += r.await.unwrap();
    }
    assert_eq!(queued, 2, "twenty racing workers queue each schedule's tick exactly once (two schedules)");
    let ticks = TaskRecord::objects().filter(TaskRecord::DEDUP.is_not_null()).count().await.unwrap();
    assert_eq!(ticks, 2);
    let later = DateTime::from_unix(tick.unix() + 3 * 60);
    assert_eq!(
        tasks::queue_schedules(later, &mut Default::default()).await.unwrap(),
        0,
        "a worker starting minutes later doesn't requeue it"
    );
    tasks::run_due_at(later, 10).await.unwrap();
    assert_eq!(GREETED.lock().unwrap().iter().filter(|n| *n == "Beat").count(), 1, "and it runs once");
    assert_eq!(GREETED.lock().unwrap().iter().filter(|n| *n == "Beat2").count(), 1, "a second payload on the same cron runs too");
    let next = DateTime::from_unix(tick.unix() + 5 * 60);
    assert_eq!(tasks::queue_schedules(next, &mut Default::default()).await.unwrap(), 2, "the next tick is queued (both schedules)");
    let stale = DateTime::from_unix(tick.unix() + 5 * 60 + 11 * 60 + 30);
    let missed = TaskRecord::objects().filter(TaskRecord::DEDUP.is_not_null()).count().await.unwrap();
    tasks::queue_schedules(stale, &mut Default::default()).await.unwrap();
    assert_eq!(
        TaskRecord::objects().filter(TaskRecord::DEDUP.is_not_null()).count().await.unwrap(),
        missed + 2,
        "only the latest tick catches up"
    );

    // Password change, then log in with the new password.
    let (status, _, body) =
        send(form("/admin/password_change/", &cookie, "old_password=wrong&new_password1=n3w-pass-123&new_password2=n3w-pass-123")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("Your old password was entered incorrectly."));
    let (_, _, body) =
        send(form("/admin/password_change/", &cookie, "old_password=correct+horse&new_password1=n3w-pass-123&new_password2=n3w-pass-123"))
            .await;
    assert!(body.contains("Your password was changed."));

    let mut evil = form("/admin/blog_author/add", &cookie, "name=Mallory&email=m%40evil.example");
    evil.headers_mut().insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(send(evil).await.0, StatusCode::FORBIDDEN, "cross-site POSTs are rejected");

    let (status, headers, body) = send(form("/admin/logout", &cookie, "")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Thanks for spending some quality time"));
    assert!(headers[header::SET_COOKIE].to_str().unwrap().contains("Max-Age=0"));
    assert_eq!(send(get("/admin/", &cookie)).await.0, StatusCode::SEE_OTHER, "session is gone after logout");
    let (status, _, _) = send(form("/admin/login", "", "username=root&password=n3w-pass-123")).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "new password works");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `save()` on a clone that should insert, for asserting on the error.
trait SaveNew {
    async fn save_new(self) -> rangoli::Result<()>;
}
impl<M: orm::Model> SaveNew for M {
    async fn save_new(mut self) -> rangoli::Result<()> {
        self.save().await
    }
}

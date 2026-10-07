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
use rangoli::{migrate, App, Choices, DateTime, Error, Json, Model};
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

#[tokio::test]
async fn full_stack() {
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
    assert!(migrate::make(&migrations, v1.models(), None).unwrap().is_none(), "no changes the second time");

    let db = orm::connect(&url).await.unwrap();
    reset(db).await;
    let applied = migrate::run(&migrations).await.unwrap();
    assert_eq!(applied.len(), 4, "three built-in + app: {applied:?}");
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
        rangoli::atomic(async { temp("temp2").save_new().await }).await?; // nested joins the outer block
        assert_eq!(Author::objects().count().await?, before + 2, "writes are visible inside the transaction");
        Err(Error::NotFound)
    })
    .await;
    assert!(matches!(failed, Err(Error::NotFound)));
    assert_eq!(Author::objects().count().await.unwrap(), before, "an Err rolls everything back");
    rangoli::atomic(async { temp("temp3").save_new().await }).await.unwrap();
    assert_eq!(Author::objects().filter(Author::NAME.eq("temp3")).count().await.unwrap(), 1, "an Ok commits");
    Author::objects().filter(Author::NAME.eq("temp3")).delete().await.unwrap();

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
                .list_per_page(2),
        )
        .task::<Greet>()
        .task::<Flaky>()
        .task::<Explodes>()
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
    let (status, _, body) =
        send(form(&post_url, &cookie, &format!("title=x&body=b&author_id={}&status=bogus&meta=%7Bnope", alan.id.unwrap()))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("Select a valid choice. bogus is not one of the available choices."), "choices are validated");
    assert!(body.contains("Enter valid JSON"), "JSON is validated");
    let (_, _, body) = send(get("/admin/blog_post/add", &cookie)).await;
    assert!(body.contains("<option value=\"draft\" selected>Draft</option>"), "new forms start from the default");
    let edit = format!(
        "title=Long&body=b&published=on&author_id={}&views=999&tags={}&tags={}&status=published&meta=%7B%22a%22%3A1%7D",
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
        r#"{{"title": "From the API", "body": "b", "author_id": {}, "views": 1, "publish_at": "2026-05-01T10:00:00Z"}}"#,
        alan.id.unwrap()
    );
    assert_eq!(send(api("POST", "/api/blog_post/", "", &new_post)).await.0, StatusCode::UNAUTHORIZED, "writes need staff");
    let (status, _, body) = send(api("POST", "/api/blog_post/", &cookie, r#"{"title": "", "views": "many", "bogus": 1}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let errs = json(&body);
    assert_eq!(errs["title"][0], "This field may not be blank.");
    assert_eq!(errs["views"][0], "A valid integer is required.");
    assert_eq!(errs["author_id"][0], "This field is required.");
    assert_eq!(errs["bogus"][0], "Unknown field.");
    let (status, _, body) = send(api("POST", "/api/blog_post/", &cookie, &new_post)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created = json(&body);
    let new_id = created["id"].as_i64().unwrap();
    assert_eq!(created["publish_at"], "2026-05-01T10:00:00Z");
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
    let orphan = r#"{"title": "x", "body": "b", "author_id": 424242, "views": 0}"#;
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

    let (_, _, body) = send(get("/api/schema.json", "")).await;
    let schema = json(&body);
    assert_eq!(schema["openapi"], "3.0.3");
    assert!(schema["paths"]["/api/blog_post/{id}"]["patch"].is_object());
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["publish_at"]["format"], "date-time");
    assert_eq!(schema["components"]["schemas"]["PostV2"]["properties"]["created_at"]["readOnly"], true);
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

//! End to end: migrations, ORM, schema evolution and the admin over HTTP.
//! Runs against SQLite by default; CI also points `RANGOLI_TEST_DATABASE_URL`
//! at Postgres and MySQL. One test function because the pool is process-global.

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use rangoli::orm;
use rangoli::{migrate, App, Error, Model};
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

/// The same table after a schema change: `rating` dropped, `views` added, `title` widened.
#[derive(Model, Clone, Debug)]
#[model(table = "blog_post", display = "title")]
struct PostV2 {
    id: Option<i64>,
    #[field(max_length = 300)]
    title: String,
    #[field(text)]
    body: String,
    published: bool,
    #[field(fk = Author)]
    author_id: i64,
    views: i64,
}

fn post(title: &str, author: &Author, published: bool, rating: Option<f64>) -> Post {
    Post { id: None, title: title.into(), body: format!("Body of {title}"), published, author_id: author.id.unwrap(), rating }
}

async fn reset(db: &orm::Db) {
    for t in ["blog_post", "rangoli_session", "blog_author", "rangoli_user", "rangoli_migrations"] {
        let sql = format!("DROP TABLE IF EXISTS {}", db.dialect.quote(t));
        sqlx::query(&sql).execute(&db.pool).await.unwrap();
    }
}

#[tokio::test]
async fn full_stack() {
    let dir = std::env::temp_dir().join(format!("rangoli-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let url = std::env::var("RANGOLI_TEST_DATABASE_URL").unwrap_or_else(|_| format!("sqlite://{}/test.sqlite3?mode=rwc", dir.display()));
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
    assert_eq!(applied.len(), 2, "builtin + app: {applied:?}");
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

    let dup = Author { id: None, name: "Copy".into(), email: "ada@example.com".into() }.save_new().await;
    assert!(dup.unwrap_err().is_unique_violation());
    let orphan = post("Orphan", &Author { id: Some(424242), ..ada.clone() }, true, None).save_new().await;
    assert!(orphan.unwrap_err().is_foreign_key_violation(), "foreign keys are enforced (SQLite too)");
    assert!(ada.delete().await.unwrap_err().is_foreign_key_violation(), "can't delete an author with posts");

    // ---- schema evolution: drop a column, add a NOT NULL one, widen another
    let v2 = App::new().admin::<Author>().admin::<PostV2>();
    let file = migrate::make(&migrations, v2.models(), Some("post_views")).unwrap().unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    for op in ["\"alter_column\"", "\"add_column\"", "\"drop_column\""] {
        assert!(text.contains(op), "{op} missing from {text}");
    }
    assert_eq!(migrate::pending(&migrations).await.unwrap().len(), 1);
    migrate::run(&migrations).await.unwrap();
    let rows = PostV2::objects().order_by(PostV2::ID.asc()).all().await.unwrap();
    assert_eq!(rows.len(), 3, "data survives the migration");
    assert!(rows.iter().all(|p| p.views == 0));
    let mut long =
        PostV2 { id: None, title: "x".repeat(300), body: String::new(), published: false, author_id: alan.id.unwrap(), views: 7 };
    long.save().await.unwrap();

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
    assert!(body.contains("Authors") && body.contains("PostV2s") && body.contains("Users"));

    let (_, _, body) = send(get("/admin/blog_post/?q=machinery", &cookie)).await;
    assert!(body.contains("Computing Machinery") && !body.contains("Rust 50%"), "search filters rows");
    assert!(body.contains("<td>Alan</td>"), "foreign keys show their label in the list");
    let (_, _, body) = send(get("/admin/blog_post/?o=title&f.published=1", &cookie)).await;
    assert!(body.contains("Title ▲") && !body.contains("xxxxxxxxxx"), "sort + boolean filter");
    let (_, _, body) = send(get("/admin/rangoli_user/", &cookie)).await;
    assert!(body.contains("root") && !body.contains("$argon2"), "password hashes never render");

    let (status, _, body) = send(form("/admin/blog_author/add", &cookie, "name=&email=x%40example.com")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("This field is required."));
    let (status, _, body) = send(form("/admin/blog_author/add", &cookie, "name=Grace&email=ada%40example.com")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("already exists"));
    let (status, headers, _) = send(form("/admin/blog_author/add", &cookie, "name=Grace&email=grace%40example.com&_continue=1")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let grace_url = headers[header::LOCATION].to_str().unwrap().to_string();

    let (_, _, body) = send(get(&grace_url, &cookie)).await;
    assert!(body.contains("value=\"Grace\""));
    let (status, _, _) = send(form(&grace_url, &cookie, "name=Grace+Hopper&email=grace%40example.com")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(Author::objects().filter(Author::NAME.eq("Grace Hopper")).count().await.unwrap(), 1);

    let (_, _, body) = send(get(&format!("/admin/blog_post/{}/", long.id.unwrap()), &cookie)).await;
    assert!(body.contains(">Grace Hopper</option>") && body.contains(">Alan</option>"), "foreign keys render as a select");

    let (status, _, _) = send(form(&format!("/admin/blog_author/{}/delete", ada.id.unwrap()), &cookie, "")).await;
    assert_eq!(status, StatusCode::CONFLICT, "protected by foreign key");
    let (status, _, _) = send(form(&format!("{grace_url}delete"), &cookie, "")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(Author::objects().count().await.unwrap(), 2);

    let mut evil = form("/admin/blog_author/add", &cookie, "name=Mallory&email=m%40evil.example");
    evil.headers_mut().insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(send(evil).await.0, StatusCode::FORBIDDEN, "cross-site POSTs are rejected");

    let (status, headers, _) = send(form("/admin/logout", &cookie, "")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers[header::SET_COOKIE].to_str().unwrap().contains("Max-Age=0"));
    assert_eq!(send(get("/admin/", &cookie)).await.0, StatusCode::SEE_OTHER, "session is gone after logout");

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

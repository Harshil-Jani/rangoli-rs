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
    for t in ["blog_post", "rangoli_session", "rangoli_admin_log", "blog_author", "rangoli_user", "rangoli_migrations"] {
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
    assert_eq!(applied.len(), 3, "two built-in + app: {applied:?}");
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
    assert!(body.contains("Rangoli administration") && body.contains("Signed in as <strong>root</strong>"));
    assert!(
        body.contains("<caption>Blog</caption>") && body.contains("<caption>Authentication and Authorization</caption>"),
        "models grouped by app"
    );
    assert!(body.contains("Recent actions") && body.contains("Nothing yet."));

    let (_, _, body) = send(get("/admin/blog_post/?q=machinery", &cookie)).await;
    assert!(
        body.contains("Select postv2 to change") && body.contains("Computing Machinery") && !body.contains("Rust 50%"),
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

    let (status, _, body) = send(get(&format!("/admin/blog_author/{}/delete", ada.id.unwrap()), &cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("would require deleting these protected related objects") && body.contains("PostV2: Rust 50% off_sale"));
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
    assert!(body.contains("Are you sure you want to delete the selected postv2s?") && body.contains("Summary"));
    assert_eq!(PostV2::objects().count().await.unwrap(), 4, "nothing deleted before confirming");
    let (status, headers, _) = send(form("/admin/blog_post/", &cookie, &format!("{pick}&post=yes"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, body) = send(get(headers[header::LOCATION].to_str().unwrap(), &cookie)).await;
    assert!(body.contains("Successfully deleted 1 postv2."));
    assert_eq!(PostV2::objects().count().await.unwrap(), 3);
    let (_, headers, _) = send(form("/admin/blog_post/", &cookie, "action=delete_selected")).await;
    assert!(headers[header::LOCATION].to_str().unwrap().ends_with("warn=noitems"));

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

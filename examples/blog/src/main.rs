use rangoli::axum::extract::{Path, Query};
use rangoli::axum::response::Html;
use rangoli::axum::{routing::get, Json, Router};
use rangoli::prelude::*;
use serde::Serialize;

mod models;
use models::*;

#[derive(Serialize)]
struct PostOut {
    #[serde(flatten)]
    post: Post,
    author: Option<String>,
}

/// Published posts with their authors in two queries total, never N+1.
async fn posts() -> rangoli::Result<Json<Vec<PostOut>>> {
    let posts = Post::objects().filter(Post::PUBLISHED.eq(true)).order_by(Post::ID.desc()).limit(50).all().await?;
    let authors = Author::in_bulk(posts.iter().map(|p| p.author_id)).await?;
    Ok(Json(posts.into_iter().map(|p| PostOut { author: authors.get(&p.author_id).map(|a| a.name.clone()), post: p }).collect()))
}

/// The public homepage. `?partial=1` renders only the `posts` block, which the
/// search box swaps in with htmx (Django 6.0 template partials).
async fn home(Query(p): Query<Vec<(String, String)>>) -> rangoli::Result<Html<String>> {
    let q = p.iter().find(|(k, _)| k == "q").map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let cards = latest_cards(&q).await?;
    let page = if p.iter().any(|(k, _)| k == "partial") { "home.html#posts" } else { "home.html" };
    render(page, context! { posts => cards, q })
}

/// A missing id becomes a 404 automatically.
async fn post(Path(id): Path<i64>) -> rangoli::Result<Json<Post>> {
    Ok(Json(Post::get(id).await?))
}

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    App::new()
        .admin::<Author>()
        .admin::<Tag>()
        .admin_with::<Post>(
            ModelAdmin::new()
                .list_display([&Post::TITLE, &Post::AUTHOR_ID, &Post::PUBLISHED, &Post::RATING, &Post::CREATED_AT])
                .search_fields([&Post::TITLE, &Post::BODY])
                .list_filter([&Post::PUBLISHED, &Post::AUTHOR_ID, &Post::CREATED_AT])
                .ordering(Post::CREATED_AT.desc()),
        )
        .api::<Post>(Api::new().read(Access::Public)) // /api/blog_post/ and /api/schema.json
        .static_files("/static", "static")
        .routes(Router::new().route("/", get(home)).route("/posts.json", get(posts)).route("/posts/{id}", get(post)))
        .run()
        .await
}

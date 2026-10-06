use rangoli::axum::extract::{Path, Query};
use rangoli::axum::response::Html;
use rangoli::axum::{routing::get, Json, Router};
use rangoli::prelude::*;
use serde::Serialize;

#[derive(Model, Serialize, Clone, Debug)]
#[model(table = "blog_author", display = "name")]
pub struct Author {
    pub id: Option<i64>,
    #[field(max_length = 100)]
    pub name: String,
    #[field(max_length = 254, unique)]
    pub email: String,
}

#[derive(Model, Serialize, Clone, Debug)]
#[model(table = "blog_tag", display = "name")]
pub struct Tag {
    pub id: Option<i64>,
    #[field(max_length = 50, unique)]
    pub name: String,
}

#[derive(Model, Serialize, Clone, Debug)]
#[model(table = "blog_post", display = "title", m2m(tags = Tag))]
pub struct Post {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub title: String,
    #[field(text)]
    pub body: String,
    #[field(index)]
    pub published: bool,
    #[field(fk = Author)]
    pub author_id: i64,
    pub rating: Option<f64>,
    #[field(auto_now_add)]
    pub created_at: DateTime,
    #[field(auto_now)]
    pub updated_at: DateTime,
}

#[derive(Serialize)]
struct PostOut {
    #[serde(flatten)]
    post: Post,
    author: Option<String>,
}

/// Published posts with their authors in two queries total, never N+1.
async fn posts() -> rangoli::Result<Json<Vec<PostOut>>> {
    let posts = Post::objects().filter(Post::PUBLISHED.eq(true)).order_by(Post::ID.desc()).all().await?;
    let authors = Author::in_bulk(posts.iter().map(|p| p.author_id)).await?;
    Ok(Json(posts.into_iter().map(|p| PostOut { author: authors.get(&p.author_id).map(|a| a.name.clone()), post: p }).collect()))
}

#[derive(Serialize)]
struct Card {
    title: String,
    body: String,
    author: String,
    tags: Vec<String>,
}

/// The public homepage. `?partial=1` renders only the `posts` block, which the
/// search box swaps in with htmx (Django 6.0 template partials).
async fn home(Query(p): Query<Vec<(String, String)>>) -> rangoli::Result<Html<String>> {
    let q = p.iter().find(|(k, _)| k == "q").map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let mut posts = Post::objects().filter(Post::PUBLISHED.eq(true)).order_by(Post::CREATED_AT.desc());
    if !q.is_empty() {
        posts = posts.filter(Post::TITLE.contains(&q) | Post::BODY.contains(&q));
    }
    let posts = posts.all().await?;
    // Three queries in total, however many posts: posts, their authors, their tags.
    let authors = Author::in_bulk(posts.iter().map(|p| p.author_id)).await?;
    let mut tags = Post::TAGS.prefetch(&posts).await?;
    let cards: Vec<Card> = posts
        .into_iter()
        .map(|p| Card {
            tags: tags.remove(&p.id.unwrap()).unwrap_or_default().into_iter().map(|t| t.name).collect(),
            author: authors.get(&p.author_id).map(|a| a.name.clone()).unwrap_or_default(),
            title: p.title,
            body: p.body,
        })
        .collect();
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

use rangoli::axum::{extract::Path, routing::get, Json, Router};
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
#[model(table = "blog_post", display = "title")]
pub struct Post {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub title: String,
    #[field(text)]
    pub body: String,
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

/// A missing id becomes a 404 automatically.
async fn post(Path(id): Path<i64>) -> rangoli::Result<Json<Post>> {
    Ok(Json(Post::get(id).await?))
}

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    App::new()
        .admin::<Author>()
        .admin_with::<Post>(
            ModelAdmin::new()
                .list_display([&Post::TITLE, &Post::AUTHOR_ID, &Post::PUBLISHED, &Post::RATING, &Post::CREATED_AT])
                .search_fields([&Post::TITLE, &Post::BODY])
                .list_filter([&Post::PUBLISHED, &Post::AUTHOR_ID, &Post::CREATED_AT])
                .ordering(Post::CREATED_AT.desc()),
        )
        .api::<Post>(Api::new().read(Access::Public)) // /api/blog_post/ and /api/schema.json
        .routes(Router::new().route("/", get(posts)).route("/posts/{id}", get(post)))
        .run()
        .await
}

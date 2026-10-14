//! The seven endpoints of django-bolt's cross-framework benchmark
//! (github.com/FarhanAliRaza/python-api-frameworks-benchmark), in Rangoli, over the same
//! PostgreSQL data. Same paths, same query shapes, same response fields.

use rangoli::axum::extract::{Path, Query};
use rangoli::axum::http::{header, StatusCode};
use rangoli::axum::response::{IntoResponse, Response};
use rangoli::axum::routing::{get, post};
use rangoli::axum::{Json, Router};
use rangoli::jwt::JwtAuth;
use rangoli::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

const JWT_SECRET: &str = "benchmark-jwt-secret-not-for-production";

#[derive(Model, Clone, Debug, Serialize)]
#[model(table = "rb_user")]
struct BenchUser {
    id: Option<i64>,
    #[field(max_length = 50, unique)]
    username: String,
    #[field(max_length = 254, unique)]
    email: String,
    #[field(max_length = 50)]
    first_name: String,
    #[field(max_length = 50)]
    last_name: String,
    is_active: bool,
}

#[derive(Model, Clone, Debug, Serialize)]
#[model(table = "rb_author")]
struct Author {
    id: Option<i64>,
    #[field(max_length = 100)]
    name: String,
    #[field(max_length = 254, unique)]
    email: String,
    #[field(text)]
    bio: String,
    created_at: DateTime,
}

#[derive(Model, Clone, Debug, Serialize)]
#[model(table = "rb_tag")]
struct Tag {
    id: Option<i64>,
    #[field(max_length = 50, unique)]
    name: String,
    #[field(max_length = 50, unique)]
    slug: String,
}

#[derive(Model, Clone, Debug)]
#[model(table = "rb_article", m2m(tags = Tag), index(created_at))]
struct Article {
    id: Option<i64>,
    #[field(max_length = 200)]
    title: String,
    #[field(text)]
    content: String,
    published: bool,
    #[field(fk = Author, cascade)]
    author_id: i64,
    created_at: DateTime,
    updated_at: DateTime,
}

#[derive(Model, Clone, Debug, Serialize)]
#[model(table = "rb_comment", index(article_id))]
struct Comment {
    id: Option<i64>,
    #[field(fk = Article, cascade)]
    article_id: i64,
    #[field(max_length = 100)]
    author_name: String,
    #[field(text)]
    content: String,
    created_at: DateTime,
}

#[derive(Serialize)]
struct ArticleOut {
    id: Option<i64>,
    title: String,
    content: String,
    published: bool,
    author: Option<Author>,
    tags: Vec<Tag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comments: Option<Vec<Comment>>,
    created_at: DateTime,
    updated_at: DateTime,
}

impl ArticleOut {
    fn new(a: Article, author: Option<Author>, tags: Vec<Tag>, comments: Option<Vec<Comment>>) -> Self {
        let Article { id, title, content, published, created_at, updated_at, .. } = a;
        ArticleOut { id, title, content, published, author, tags, comments, created_at, updated_at }
    }
}

#[derive(Serialize)]
struct Page {
    items: Vec<ArticleOut>,
    total: i64,
    page: i64,
    page_size: i64,
    pages: i64,
}

// Parsed once, serialized on every request, as the Python frameworks do with their dicts.
static JSON_1K: LazyLock<serde_json::Value> = LazyLock::new(|| serde_json::from_str(include_str!("../data/1K.json")).unwrap());
static JSON_10K: LazyLock<serde_json::Value> = LazyLock::new(|| serde_json::from_str(include_str!("../data/10K.json")).unwrap());

async fn json_1k() -> Json<&'static serde_json::Value> {
    Json(&JSON_1K)
}

async fn json_10k() -> Json<&'static serde_json::Value> {
    Json(&JSON_10K)
}

async fn db() -> rangoli::Result<Json<Vec<BenchUser>>> {
    Ok(Json(BenchUser::objects().limit(10).all().await?))
}

#[derive(Deserialize)]
struct PageQuery {
    page: Option<i64>,
    page_size: Option<i64>,
}

async fn articles_page(q: PageQuery) -> rangoli::Result<Json<Page>> {
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(20).clamp(1, 100);
    let total = Article::objects().count().await?;
    let rows = Article::objects()
        .order_by(Article::CREATED_AT.desc())
        .order_by(Article::ID.desc()) // seconds tie where Django's microseconds don't; ids follow insertion
        .offset(((page - 1) * page_size) as u64)
        .limit(page_size as u64)
        .select_related(Article::AUTHOR)
        .await?;
    let articles: Vec<Article> = rows.iter().map(|(a, _)| a.clone()).collect();
    let mut tags = Article::TAGS.prefetch(&articles).await?;
    let items = rows.into_iter().map(|(a, author)| {
        let t = tags.remove(&a.id.unwrap_or_default()).unwrap_or_default();
        ArticleOut::new(a, author, t, None)
    });
    let pages = if total == 0 { 0 } else { (total + page_size - 1) / page_size };
    Ok(Json(Page { items: items.collect(), total, page, page_size, pages }))
}

async fn articles(Query(q): Query<PageQuery>) -> rangoli::Result<Json<Page>> {
    articles_page(q).await
}

async fn article(Path(id): Path<i64>) -> rangoli::Result<Json<ArticleOut>> {
    let mut rows = Article::objects().filter(Article::ID.eq(id)).select_related(Article::AUTHOR).await?;
    let (a, author) = rows.pop().ok_or(rangoli::Error::NotFound)?;
    let one = [a];
    let (tags, comments) = tokio::try_join!(Article::TAGS.prefetch(&one), Comment::ARTICLE.prefetch_reverse(&one))?;
    let [a] = one;
    let id = a.id.unwrap_or_default();
    let mut comments = comments.get(&id).cloned().unwrap_or_default();
    comments.sort_by_key(|c| std::cmp::Reverse((c.created_at, c.id))); // Django's Meta.ordering = -created_at
    Ok(Json(ArticleOut::new(a, author, tags.get(&id).cloned().unwrap_or_default(), Some(comments))))
}

#[derive(Serialize)]
struct Me {
    id: Option<i64>,
    username: String,
    email: String,
    first_name: String,
    last_name: String,
    is_active: bool,
}

/// The JWT is checked and the user loaded by the framework before this runs.
async fn me(user: CurrentUser) -> Response {
    match user.0 {
        Some(u) => Json(Me {
            email: format!("{}@example.com", u.username),
            first_name: "Bench".into(),
            last_name: "User".into(),
            id: u.id,
            username: u.username,
            is_active: u.is_active,
        })
        .into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn auth_articles(user: CurrentUser, Query(q): Query<PageQuery>) -> Response {
    if user.0.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    articles_page(q).await.into_response()
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}

async fn login(Json(l): Json<Login>) -> rangoli::Result<Response> {
    let Some(user) = rangoli::auth::authenticate(&l.username, &l.password).await? else {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    };
    let token = JwtAuth::hs256(JWT_SECRET).issue(&user, std::time::Duration::from_secs(900)).unwrap_or_default();
    let cookie = format!("access_token={token}; HttpOnly; Path=/; SameSite=Lax");
    Ok(([(header::SET_COOKIE, cookie)], Json(HashMap::from([("ok", true)]))).into_response())
}

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    App::new()
        .model::<BenchUser>()
        .model::<Author>()
        .model::<Tag>()
        .model::<Article>()
        .model::<Comment>()
        .jwt(JwtAuth::hs256(JWT_SECRET).cookie("access_token"))
        .routes(
            Router::new()
                .route("/json-1k", get(json_1k))
                .route("/json-10k", get(json_10k))
                .route("/db", get(db))
                .route("/articles", get(articles))
                .route("/articles/{id}", get(article))
                .route("/auth/login", post(login))
                .route("/auth/me", get(me))
                .route("/auth/articles", get(auth_articles)),
        )
        .run()
        .await
}

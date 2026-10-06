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
pub struct Card {
    pub title: String,
    pub body: String,
    pub author: String,
    pub tags: Vec<String>,
}

/// The latest 50 published posts matching `q`, with author names and tags.
/// Four queries however many posts: posts, their authors, the tag links, the tags.
pub async fn latest_cards(q: &str) -> rangoli::Result<Vec<Card>> {
    let mut posts = Post::objects().filter(Post::PUBLISHED.eq(true)).order_by(Post::CREATED_AT.desc());
    if !q.is_empty() {
        posts = posts.filter(Post::TITLE.contains(q) | Post::BODY.contains(q));
    }
    let posts = posts.limit(50).all().await?;
    let authors = Author::in_bulk(posts.iter().map(|p| p.author_id)).await?;
    let mut tags = Post::TAGS.prefetch(&posts).await?;
    Ok(posts
        .into_iter()
        .map(|p| Card {
            tags: tags.remove(&p.id.unwrap()).unwrap_or_default().into_iter().map(|t| t.name).collect(),
            author: authors.get(&p.author_id).map(|a| a.name.clone()).unwrap_or_default(),
            title: p.title,
            body: p.body,
        })
        .collect())
}

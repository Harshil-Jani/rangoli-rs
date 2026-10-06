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

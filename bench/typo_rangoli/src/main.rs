use rangoli::prelude::*;

#[derive(Model)]
#[model(table = "blog_post")]
pub struct Post {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub title: String,
}

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    let n = Post::objects().filter(Post::TITEL.eq("Hello")).count().await?;
    println!("{n}");
    Ok(())
}

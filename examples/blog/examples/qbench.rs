//! Where does a request's time go? Times the ORM pieces the homepage uses.
#[path = "../src/models.rs"]
mod models;
use models::*;
use rangoli::prelude::*;
use std::time::Instant;

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    rangoli::orm::connect(&std::env::var("RANGOLI_DATABASE_URL").unwrap()).await?;
    let n = 500;
    let t = Instant::now();
    for _ in 0..n {
        Post::objects().filter(Post::PUBLISHED.eq(true)).order_by(Post::ID.desc()).limit(50).all().await?;
    }
    println!("50 posts query      {:>8.1} us", t.elapsed().as_micros() as f64 / n as f64);
    let posts = Post::objects().filter(Post::PUBLISHED.eq(true)).order_by(Post::ID.desc()).limit(50).all().await?;
    let t = Instant::now();
    for _ in 0..n {
        Author::in_bulk(posts.iter().map(|p| p.author_id)).await?;
    }
    println!("authors in_bulk     {:>8.1} us", t.elapsed().as_micros() as f64 / n as f64);
    let t = Instant::now();
    for _ in 0..n {
        Post::TAGS.prefetch(&posts).await?;
    }
    println!("tags prefetch       {:>8.1} us", t.elapsed().as_micros() as f64 / n as f64);
    let t = Instant::now();
    for _ in 0..n {
        Post::objects().filter(Post::ID.eq(1)).count().await?;
    }
    println!("trivial count       {:>8.1} us", t.elapsed().as_micros() as f64 / n as f64);
    Ok(())
}

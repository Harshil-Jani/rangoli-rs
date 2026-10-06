//! How many queries does the homepage cost? (Django's assertNumQueries, in Rust.)
#[path = "../src/models.rs"]
#[allow(dead_code)]
mod models;

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    rangoli::orm::connect(&std::env::var("RANGOLI_DATABASE_URL").expect("RANGOLI_DATABASE_URL")).await?;
    let (cards, queries) = rangoli::orm::count_queries(models::latest_cards("")).await;
    println!("{{\"posts\": {}, \"queries\": {queries}}}", cards?.len());
    Ok(())
}

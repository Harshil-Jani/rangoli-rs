//! A JSON API on Postgres with nothing else compiled in: no admin pages, no templates,
//! no WebSockets, no SQLite or MySQL drivers.
use rangoli::prelude::*;

#[derive(Model, Clone, Debug)]
#[model(table = "note")]
pub struct Note {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub title: String,
    #[field(auto_now_add)]
    pub created_at: DateTime,
}

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    App::new().api::<Note>(Api::new().read(Access::Public).write(Access::Authenticated)).run().await
}

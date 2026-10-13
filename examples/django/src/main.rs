//! Your Django database, ready for AI agents, in one command:
//!
//! ```sh
//! RANGOLI_DATABASE_URL=postgres://user:pw@host/your_django_db cargo run -p rangoli-django
//! ```
//!
//! Open /admin/, sign in with your Django staff account, and click "Connect an AI agent".
//! Read-only unless RANGOLI_DJANGO_WRITES=1. See `rangoli::django` for what it reads and skips.

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    rangoli::django::app().await?.run().await
}

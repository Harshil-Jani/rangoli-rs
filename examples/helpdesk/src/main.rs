//! A support desk with an AI agent on it, safely.
//!
//! Customers file tickets through the API (each sees only their own). Staff work them in the
//! admin, with replies edited inline. An AI agent connects from the admin ("Connect an AI
//! agent") to triage and draft replies: it can't read the internal notes (a private field), it
//! only touches tickets and replies, and with "ask me first" every reply it writes waits for a
//! person under Pending changes, where it can be approved, rejected, or later undone.
//!
//! ```sh
//! cargo run -p helpdesk -- migrate
//! cargo run -p helpdesk -- createsuperuser admin
//! RANGOLI_DEBUG=1 cargo run -p helpdesk      # http://127.0.0.1:8000/admin/
//! ```

use rangoli::auth::User;
use rangoli::prelude::*;

#[derive(Choices, Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Open,
    Waiting,
    Closed,
}

#[derive(Model, Clone, Debug)]
#[model(table = "desk_ticket", display = "subject", index(status, opened_at))]
pub struct Ticket {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub subject: String,
    #[field(text)]
    pub body: String,
    #[field(choices, default = "open")]
    pub status: Status,
    /// The customer who filed it: they see only their own tickets through the API.
    #[field(fk = User, cascade)]
    pub customer_id: i64,
    /// Staff-only context. Private: never in the API, never shown to an agent.
    #[field(text, private)]
    pub internal_note: Option<String>,
    #[field(auto_now_add, index)]
    pub opened_at: DateTime,
}

#[derive(Model, Clone, Debug)]
#[model(table = "desk_reply", display = "body")]
pub struct Reply {
    pub id: Option<i64>,
    #[field(fk = Ticket, cascade)]
    pub ticket_id: i64,
    #[field(text)]
    pub body: String,
    #[field(auto_now_add)]
    pub sent_at: DateTime,
}

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    App::new()
        .embed_migrations(rangoli::migrations!())
        .admin_with::<Ticket>(
            ModelAdmin::new()
                .list_display([&Ticket::SUBJECT, &Ticket::STATUS, &Ticket::CUSTOMER_ID, &Ticket::OPENED_AT])
                .search_fields([&Ticket::SUBJECT, &Ticket::BODY])
                .list_filter([&Ticket::STATUS, &Ticket::OPENED_AT])
                .ordering(Ticket::OPENED_AT.desc())
                .inline(Reply::TICKET_ID)
                .action("close", "Close selected tickets", |selected| async move {
                    selected.update([Ticket::STATUS.set(Status::Closed)]).await.map(drop)
                }),
        )
        .admin::<Reply>()
        // Customers: their own tickets only. Agents and staff: everything their token allows.
        .api::<Ticket>(Api::new().read(Access::Authenticated).write(Access::Authenticated).owned_by(&Ticket::CUSTOMER_ID))
        .api::<Reply>(Api::new().read(Access::Staff).write(Access::Staff))
        // A slash command in Claude or Cursor: "/triage waiting".
        .mcp_prompt("triage", "Rank tickets by urgency", "List support tickets with status {status}, read each, and rank them by urgency with one line of reasoning each. Don't change anything.")
        // `cargo run -p helpdesk -- close_stale 14`: close tickets nobody touched in N days.
        .command("close_stale", "close open tickets older than N days (default 30)", |args| async move {
            let days: i64 = args.first().and_then(|d| d.parse().ok()).unwrap_or(30);
            let cutoff = DateTime::from_unix(DateTime::now().unix() - days * 86_400);
            let n = Ticket::objects()
                .filter(Ticket::STATUS.eq(Status::Open))
                .filter(Ticket::OPENED_AT.lt(cutoff))
                .update([Ticket::STATUS.set(Status::Closed)])
                .await?;
            println!("Closed {n} ticket(s) opened before {cutoff}.");
            Ok(())
        })
        .run()
        .await
}

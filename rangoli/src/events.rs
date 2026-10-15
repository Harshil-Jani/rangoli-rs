//! The table MySQL and SQLite instances share realtime messages through (Postgres uses
//! LISTEN/NOTIFY instead). Rows live about a minute.

/// One message for other instances: its realtime group, the text, and which instance sent it.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_event")]
pub struct Event {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub channel: String,
    #[field(text)]
    pub message: String,
    #[field(max_length = 32)]
    pub origin: String,
    /// Unix seconds, for pruning.
    pub at: i64,
}

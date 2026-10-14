//! A cache that empties itself when the data it was built from changes, on every instance.
//!
//! ```ignore
//! let top = cache::cached("home:top-posts", &["blog_post"], Duration::from_secs(300), || async {
//!     Post::objects().order_by(Post::VIEWS.desc()).limit(10).all().await
//! }).await?;
//! ```
//!
//! Each value is tagged with the tables it reads. Any write to one of them through Rangoli (the
//! ORM, the admin, the API, MCP) drops the value on this instance and, over Postgres
//! `LISTEN/NOTIFY` (the `realtime` feature), on every other instance: no Redis, and no
//! hand-written invalidation (Django's cache has neither). Inside a transaction the drop is
//! repeated after commit, and a value computed while its tables changed is not stored, so a slow
//! reader can't put old rows back.
//!
//! ponytail: values live in each process's memory, capped at 10,000 entries (past that the
//! cache starts over). Writes made outside Rangoli (raw SQL, another app) aren't seen.

use crate::Result;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 10_000;
#[cfg(feature = "realtime")]
const GROUP: &str = "rangoli:cache";

struct Entry {
    json: String,
    tables: Vec<String>,
    expires: Instant,
}

#[derive(Default)]
struct Store {
    entries: HashMap<String, Entry>,
    /// Bumped on every change to a table; a value computed across a bump isn't stored.
    generation: HashMap<String, u64>,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(Default::default);

fn store() -> std::sync::MutexGuard<'static, Store> {
    STORE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn generations(tables: &[&str]) -> Vec<u64> {
    let s = store();
    tables.iter().map(|t| s.generation.get(*t).copied().unwrap_or(0)).collect()
}

/// The cached value for `key`, or `compute`'s result, kept for `ttl` until one of `tables`
/// changes. Values round-trip through JSON, so any serde type works.
pub async fn cached<T, F, Fut>(key: &str, tables: &[&str], ttl: Duration, compute: F) -> Result<T>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    listen();
    {
        let s = store();
        if let Some(e) = s.entries.get(key).filter(|e| e.expires > Instant::now()) {
            if let Ok(v) = serde_json::from_str(&e.json) {
                return Ok(v);
            }
        }
    }
    let before = generations(tables);
    let value = compute().await?;
    if generations(tables) == before {
        if let Ok(json) = serde_json::to_string(&value) {
            let mut s = store();
            if s.entries.len() >= MAX_ENTRIES {
                s.entries.clear();
            }
            let entry = Entry { json, tables: tables.iter().map(|t| t.to_string()).collect(), expires: Instant::now() + ttl };
            s.entries.insert(key.to_string(), entry);
        }
    }
    Ok(value)
}

/// Forget one key, on this instance.
pub fn delete(key: &str) {
    store().entries.remove(key);
}

/// Drop everything built from `table` on this instance (what a write does for you).
pub fn clear_table(table: &str) {
    let mut s = store();
    *s.generation.entry(table.to_string()).or_default() += 1;
    s.entries.retain(|_, e| !e.tables.iter().any(|t| t == table));
}

tokio::task_local! {
    /// Tables written in the current `atomic` block, cleared again once it commits.
    static PENDING: Mutex<HashSet<String>>;
}

/// Called by every ORM write. Clears now (this instance), and tells other instances now or,
/// inside a transaction, after it commits (before then they'd re-read the old rows).
pub(crate) fn changed(table: &str) {
    clear_table(table);
    if PENDING.try_with(|p| p.lock().unwrap_or_else(PoisonError::into_inner).insert(table.to_string())).is_err() {
        broadcast(table);
    }
}

/// Run `body` (an outermost transaction) collecting written tables; `after_commit` runs the drops.
pub(crate) async fn track<T>(body: impl Future<Output = T>) -> (T, Vec<String>) {
    PENDING
        .scope(Mutex::new(HashSet::new()), async {
            let out = body.await;
            let tables = PENDING.with(|p| p.lock().unwrap_or_else(PoisonError::into_inner).drain().collect());
            (out, tables)
        })
        .await
}

pub(crate) fn after_commit(tables: Vec<String>) {
    for t in tables {
        clear_table(&t);
        broadcast(&t);
    }
}

fn broadcast(table: &str) {
    #[cfg(feature = "realtime")]
    if !table.is_empty() {
        crate::realtime::publish_to_others(GROUP, table); // this instance has already cleared
    }
    #[cfg(not(feature = "realtime"))]
    let _ = table;
}

/// Start listening for other instances' changes (once, on first use).
fn listen() {
    #[cfg(feature = "realtime")]
    {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            let mut rx = crate::realtime::subscribe(GROUP);
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(table) => clear_table(&table),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => store().entries.clear(), // missed some: start over
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        });
    }
}

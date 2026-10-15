//! Durable change hooks (Django's signals, reliably): `App::on_change::<Order>(|c| async move { ... })`.
//!
//! Each change made through the admin, the API, MCP or a CSV import is queued as a task in the
//! same transaction as the change: a rolled-back change never fires its hooks, a committed one
//! always does (even across a restart), and the hook runs on the worker with retries, so a slow
//! or failing hook never slows down or breaks the request. Delivery is at least once: make a
//! hook safe to run twice.

use crate::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

/// What happened to which row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Added,
    Changed,
    Deleted,
}

/// One change, as the hook receives it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Change {
    pub table: String,
    pub id: i64,
    pub action: Action,
    /// The row's display name, as History shows it.
    pub object: String,
    /// History's message, e.g. "Changed title and status via MCP."
    pub message: String,
    /// Who made the change.
    pub user_id: i64,
}

type HookFn = Arc<dyn Fn(Change) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

static HOOKS: LazyLock<RwLock<Vec<(&'static str, HookFn)>>> = LazyLock::new(Default::default);

pub(crate) fn register<F, Fut>(table: &'static str, f: F)
where
    F: Fn(Change) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    HOOKS.write().unwrap_or_else(PoisonError::into_inner).push((table, Arc::new(move |c| Box::pin(f(c)))));
}

/// Queue a run of every hook on this entry's table (inside the change's transaction).
pub(crate) async fn queue(entry: &crate::admin::LogEntry) -> Result<()> {
    let wanted: Vec<usize> = HOOKS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .enumerate()
        .filter(|(_, (t, _))| *t == entry.table_name)
        .map(|(i, _)| i)
        .collect();
    for hook in wanted {
        let change = Change {
            table: entry.table_name.clone(),
            id: entry.object_id,
            action: match entry.action {
                1 => Action::Added,
                2 => Action::Changed,
                _ => Action::Deleted,
            },
            object: entry.object_repr.clone(),
            message: entry.message.clone(),
            user_id: entry.user_id,
        };
        crate::tasks::Task::enqueue(&Run { hook, change }).await?;
    }
    Ok(())
}

/// One hook's run for one change: its own task, so one failing hook retries alone.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Run {
    hook: usize,
    change: Change,
}

impl crate::tasks::Task for Run {
    const NAME: &'static str = "rangoli.on_change";
    const MAX_ATTEMPTS: i64 = 5;

    async fn run(self) -> Result<()> {
        let hook = HOOKS
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(self.hook)
            .filter(|(t, _)| *t == self.change.table)
            .map(|(_, f)| f.clone());
        match hook {
            Some(f) => f(self.change).await,
            None => Ok(()), // registered differently in this process: nothing to run
        }
    }
}

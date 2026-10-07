//! Background tasks, queued in your own database and run by a worker in the same binary.
//!
//! Django 6.0 added a tasks *interface* but no production worker, so it still needs
//! Celery plus Redis or RabbitMQ. Here `runserver` runs tasks in-process (set
//! `RANGOLI_WORKERS=0` to turn that off) and `cargo run -- worker` runs a dedicated worker.
//!
//! ```ignore
//! #[derive(Serialize, Deserialize)]
//! struct SendWelcome { user_id: i64 }
//!
//! impl Task for SendWelcome {
//!     const NAME: &'static str = "send_welcome";
//!     async fn run(self) -> rangoli::Result<()> { /* send it */ Ok(()) }
//! }
//!
//! App::new().task::<SendWelcome>();
//! SendWelcome { user_id: 7 }.enqueue().await?;
//! ```
//!
//! Failures retry with exponential backoff (10s, 20s, 40s, ... capped at an hour) up to
//! `MAX_ATTEMPTS`; panics are caught and recorded; tasks left running by a crashed worker
//! are picked up again after a lease of ten minutes.

use crate::orm::Model;
use crate::{DateTime, Error, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

const LEASE_SECS: i64 = 10 * 60;
const MAX_BACKOFF_SECS: i64 = 3600;

#[derive(rangoli_macros::Choices, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Done,
    Failed,
}

/// One queued run of a task, as stored in `rangoli_task`.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_task", display = "name", index(status, run_at))]
pub struct TaskRecord {
    pub id: Option<i64>,
    #[field(max_length = 100)]
    pub name: String,
    #[field(text)]
    pub payload: String,
    #[field(choices, max_length = 16)]
    pub status: TaskStatus,
    pub attempts: i64,
    pub max_attempts: i64,
    pub run_at: DateTime,
    pub started_at: Option<DateTime>,
    pub finished_at: Option<DateTime>,
    #[field(text)]
    pub last_error: Option<String>,
    #[field(auto_now_add)]
    pub created_at: DateTime,
}

/// A unit of background work. Its fields are the payload, stored as JSON.
pub trait Task: Serialize + DeserializeOwned + Send + 'static {
    /// Stable name stored with each queued run; renaming it orphans queued runs.
    const NAME: &'static str;
    /// Attempts before a run is marked failed.
    const MAX_ATTEMPTS: i64 = 3;

    fn run(self) -> impl Future<Output = Result<()>> + Send;

    /// Queue a run now. Returns the task record's id.
    fn enqueue(&self) -> impl Future<Output = Result<i64>> + Send {
        self.enqueue_in(Duration::ZERO)
    }

    /// Queue a run after `delay`.
    fn enqueue_in(&self, delay: Duration) -> impl Future<Output = Result<i64>> + Send {
        let payload = serde_json::to_string(self).map_err(|e| Error::Task(format!("cannot serialize {}: {e}", Self::NAME)));
        let run_at = DateTime::from_unix(DateTime::now().unix() + delay.as_secs() as i64);
        async move { enqueue_raw(Self::NAME, payload?, run_at, Self::MAX_ATTEMPTS).await }
    }
}

type BoxFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type Handler = fn(String) -> BoxFuture;

static REGISTRY: LazyLock<RwLock<HashMap<&'static str, Handler>>> = LazyLock::new(Default::default);

fn handler<T: Task>(payload: String) -> BoxFuture {
    Box::pin(async move {
        let task: T = serde_json::from_str(&payload).map_err(|e| Error::Task(format!("bad payload for {}: {e}", T::NAME)))?;
        task.run().await
    })
}

/// Make `T` runnable by workers in this process. `App::task::<T>()` calls this.
pub fn register<T: Task>() {
    REGISTRY.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(T::NAME, handler::<T>);
}

/// Queue a run by name, for code that doesn't have the task type at hand.
pub async fn enqueue_raw(name: &str, payload: String, run_at: DateTime, max_attempts: i64) -> Result<i64> {
    let mut record = TaskRecord {
        id: None,
        name: name.into(),
        payload,
        status: TaskStatus::Queued,
        attempts: 0,
        max_attempts: max_attempts.max(1),
        run_at,
        started_at: None,
        finished_at: None,
        last_error: None,
        created_at: DateTime::now(),
    };
    record.save().await?;
    Ok(record.id.unwrap())
}

/// Run everything that is due, up to `limit` runs. Returns how many ran.
pub async fn run_due(limit: usize) -> Result<usize> {
    run_due_at(DateTime::now(), limit).await
}

/// `run_due` as if the clock read `now` (lets tests skip ahead past backoffs).
pub async fn run_due_at(now: DateTime, limit: usize) -> Result<usize> {
    requeue_stale(now).await?;
    let mut ran = 0;
    while ran < limit {
        let Some(record) = claim(now).await? else { break };
        finish(record, now).await?;
        ran += 1;
    }
    Ok(ran)
}

/// Runs whose worker died mid-task go back in the queue once their lease expires.
async fn requeue_stale(now: DateTime) -> Result<()> {
    let expired = DateTime::from_unix(now.unix() - LEASE_SECS);
    TaskRecord::objects()
        .filter(TaskRecord::STATUS.eq(TaskStatus::Running) & TaskRecord::STARTED_AT.lt(expired))
        .update([TaskRecord::STATUS.set(TaskStatus::Queued)])
        .await?;
    Ok(())
}

/// Take the next due run. The status check in the UPDATE makes this a compare-and-set:
/// when two workers race for one row, exactly one update succeeds.
async fn claim(now: DateTime) -> Result<Option<TaskRecord>> {
    loop {
        let next = TaskRecord::objects()
            .filter(TaskRecord::STATUS.eq(TaskStatus::Queued) & TaskRecord::RUN_AT.lte(now))
            .order_by(TaskRecord::RUN_AT.asc())
            .order_by(TaskRecord::ID.asc())
            .first()
            .await?;
        let Some(mut record) = next else { return Ok(None) };
        let won = TaskRecord::objects()
            .filter(TaskRecord::ID.eq(record.id.unwrap()) & TaskRecord::STATUS.eq(TaskStatus::Queued))
            .update([
                TaskRecord::STATUS.set(TaskStatus::Running),
                TaskRecord::STARTED_AT.set(now),
                TaskRecord::ATTEMPTS.set(record.attempts + 1),
            ])
            .await?
            == 1;
        if won {
            record.status = TaskStatus::Running;
            record.started_at = Some(now);
            record.attempts += 1;
            return Ok(Some(record));
        }
    }
}

/// Run a claimed record and store the outcome. Panics are caught by running on a separate task.
async fn finish(mut record: TaskRecord, now: DateTime) -> Result<()> {
    let handler = REGISTRY.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(record.name.as_str()).copied();
    let outcome = match handler {
        None => Err(format!("no task registered named `{}` in this process", record.name)),
        Some(run) => match tokio::spawn(run(record.payload.clone())).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(join) => Err(match join.try_into_panic() {
                Ok(p) => format!(
                    "panicked: {}",
                    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default()
                ),
                Err(_) => "cancelled".into(),
            }),
        },
    };
    match outcome {
        Ok(()) => {
            record.status = TaskStatus::Done;
            record.finished_at = Some(now);
            record.last_error = None;
        }
        // An unknown task name won't fix itself by retrying in this process.
        Err(e) if record.attempts < record.max_attempts && handler.is_some() => {
            let backoff = (10i64 << (record.attempts - 1).clamp(0, 20)).min(MAX_BACKOFF_SECS);
            record.status = TaskStatus::Queued;
            record.run_at = DateTime::from_unix(now.unix() + backoff);
            record.last_error = Some(e);
        }
        Err(e) => {
            record.status = TaskStatus::Failed;
            record.finished_at = Some(now);
            record.last_error = Some(e);
        }
    }
    record.save().await
}

/// Run tasks forever on `concurrency` loops, polling every second when idle.
pub async fn work(concurrency: usize) {
    let loops: Vec<_> = (0..concurrency.max(1))
        .map(|_| {
            tokio::spawn(async {
                loop {
                    match run_due(1).await {
                        Ok(0) => tokio::time::sleep(Duration::from_secs(1)).await,
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("rangoli worker: {e}");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            })
        })
        .collect();
    for l in loops {
        let _ = l.await;
    }
}

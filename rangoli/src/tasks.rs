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
//! Recurring work (Celery beat's job) needs no extra process either:
//! `App::new().schedule("0 3 * * *", Cleanup { days: 30 })`. Each tick is queued as an
//! ordinary run under a unique key, so however many workers or servers are up, the database
//! lets exactly one of them queue it, and the queue lets exactly one of them run it.
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
    /// Set for scheduled ticks (`name|cron@unix`) and `enqueue_unique` runs (`once:name|key`,
    /// cleared when the run ends); unique, so each is queued once.
    #[field(max_length = 200, unique)]
    pub dedup: Option<String>,
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

    /// Queue a run unless one with the same `key` is still waiting or running: `Ok(None)` then.
    /// For "reindex product 7" or "send today's digest", where a double click, a retried
    /// request or two servers must not queue the work twice. Once the run finishes (or fails
    /// for good) the key is free again. The database's unique index decides, so it holds
    /// across any number of processes.
    fn enqueue_unique(&self, key: &str) -> impl Future<Output = Result<Option<i64>>> + Send {
        let payload = serde_json::to_string(self).map_err(|e| Error::Task(format!("cannot serialize {}: {e}", Self::NAME)));
        let dedup = format!("{ONCE}{}|{key}", Self::NAME);
        async move {
            match insert(Self::NAME, payload?, DateTime::now(), Self::MAX_ATTEMPTS, Some(dedup)).await {
                Ok(id) => Ok(Some(id)),
                Err(e) if e.is_unique_violation() => Ok(None),
                Err(e) => Err(e),
            }
        }
    }
}

/// Prefix of `enqueue_unique` keys, which are released when the run ends (schedule tick keys stay).
const ONCE: &str = "once:";

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
    insert(name, payload, run_at, max_attempts, None).await
}

async fn insert(name: &str, payload: String, run_at: DateTime, max_attempts: i64, dedup: Option<String>) -> Result<i64> {
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
        dedup,
    };
    record.save().await?;
    Ok(record.id.unwrap())
}

// ---------------------------------------------------------------- schedules

/// A UTC cron expression: `minute hour day-of-month month day-of-week`, with `*`, `a-b`,
/// `*/n`, `a-b/n` and lists, or `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`.
/// Like Vixie cron, when both day fields are restricted a day matching either one fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    dom_any: bool,
    dow_any: bool,
}

impl Cron {
    pub fn parse(expr: &str) -> std::result::Result<Self, String> {
        let expr = match expr.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            e => e,
        };
        let f: Vec<&str> = expr.split_whitespace().collect();
        let [mi, h, dom, mo, dow] = f[..] else {
            return Err(format!("`{expr}`: expected 5 fields (minute hour day-of-month month day-of-week)"));
        };
        let mut dow_bits = cron_field(dow, 0, 7).map_err(|e| format!("day-of-week: {e}"))?;
        if dow_bits & (1 << 7) != 0 {
            dow_bits = (dow_bits | 1) & !(1 << 7); // 7 is Sunday too
        }
        Ok(Cron {
            minute: cron_field(mi, 0, 59).map_err(|e| format!("minute: {e}"))?,
            hour: cron_field(h, 0, 23).map_err(|e| format!("hour: {e}"))?,
            dom: cron_field(dom, 1, 31).map_err(|e| format!("day-of-month: {e}"))?,
            month: cron_field(mo, 1, 12).map_err(|e| format!("month: {e}"))?,
            dow: dow_bits,
            dom_any: dom.starts_with('*'),
            dow_any: dow.starts_with('*'),
        })
    }

    /// Does the minute containing `t` fire?
    pub fn matches(&self, t: DateTime) -> bool {
        let (_, mo, d, h, mi, _) = t.parts();
        let weekday = (t.unix().div_euclid(86_400) + 4).rem_euclid(7); // 1970-01-01 was a Thursday
        let (dom_ok, dow_ok) = (self.dom & (1 << d) != 0, self.dow & (1 << weekday) != 0);
        let day_ok = if self.dom_any || self.dow_any { dom_ok && dow_ok } else { dom_ok || dow_ok };
        self.minute & (1 << mi) != 0 && self.hour & (1 << h) != 0 && self.month & (1 << mo) != 0 && day_ok
    }

    /// The first firing minute strictly after `t`, within the next ~5 years.
    pub fn next_after(&self, t: DateTime) -> Option<DateTime> {
        let start = t.unix().div_euclid(60) + 1;
        // ponytail: minute-by-minute scan, ~2.6M steps worst case (a Feb 29 schedule); fine for startup and tests.
        (start..start + 5 * 366 * 1440).map(|m| DateTime::from_unix(m * 60)).find(|&m| self.matches(m))
    }
}

fn cron_field(s: &str, lo: u32, hi: u32) -> std::result::Result<u64, String> {
    let mut bits = 0u64;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().ok().filter(|&n| n > 0).ok_or(format!("bad step in `{part}`"))?),
            None => (part, 1),
        };
        let num = |x: &str| x.parse::<u32>().ok().filter(|n| (lo..=hi).contains(n)).ok_or(format!("`{x}` is not in {lo}-{hi}"));
        let (a, b) = match range {
            "*" => (lo, hi),
            r => match r.split_once('-') {
                Some((a, b)) => (num(a)?, num(b)?),
                None if part.contains('/') => (num(r)?, hi),
                None => (num(r)?, num(r)?),
            },
        };
        if a > b {
            return Err(format!("`{range}` runs backwards"));
        }
        for v in (a..=b).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

/// How long after a missed tick (every worker down) it still runs, once, on startup.
const CATCH_UP_SECS: i64 = 10 * 60;

struct Schedule {
    name: &'static str,
    cron_text: String,
    cron: Cron,
    payload: String,
    max_attempts: i64,
}

static SCHEDULES: LazyLock<RwLock<Vec<Schedule>>> = LazyLock::new(Default::default);

/// Run `task` on `cron` (UTC). `App::schedule` calls this; errors on a bad expression.
pub fn schedule<T: Task>(cron: &str, task: &T) -> Result<()> {
    let parsed = Cron::parse(cron).map_err(|e| Error::Config(format!("schedule for {}: {e}", T::NAME)))?;
    let payload = serde_json::to_string(task).map_err(|e| Error::Task(format!("cannot serialize {}: {e}", T::NAME)))?;
    register::<T>();
    let s = Schedule { name: T::NAME, cron_text: cron.trim().into(), cron: parsed, payload, max_attempts: T::MAX_ATTEMPTS };
    SCHEDULES.write().unwrap_or_else(std::sync::PoisonError::into_inner).push(s);
    Ok(())
}

/// The dedup key of one schedule's tick: `name|<hash of cron and payload>@unix`. The payload
/// is part of it, so `Cleanup { days: 30 }` and `Cleanup { days: 7 }` on the same cron are two
/// schedules; the hash keeps the key a fixed length whatever the cron text or payload.
fn tick_key(s: &Schedule, tick: DateTime) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{}\n{}", s.cron_text, s.payload).as_bytes());
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{}|{short}@{}", s.name, tick.unix())
}

/// Queue the latest due tick of every schedule (if it is under ten minutes old). Returns how
/// many this call queued: across all processes each tick is queued exactly once, because a
/// second insert of the same `dedup` key is a unique violation. `seen` remembers ticks this
/// process already handled, so a worker polling every second writes once per tick, not per poll.
pub async fn queue_schedules(now: DateTime, seen: &mut std::collections::HashSet<String>) -> Result<usize> {
    let due: Vec<(String, &'static str, String, i64, DateTime)> = {
        let all = SCHEDULES.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        let minute = now.unix().div_euclid(60) * 60;
        all.iter()
            .filter_map(|s| {
                let tick = (0..=CATCH_UP_SECS / 60).map(|k| DateTime::from_unix(minute - k * 60)).find(|&t| s.cron.matches(t))?;
                Some((tick_key(s, tick), s.name, s.payload.clone(), s.max_attempts, tick))
            })
            .collect()
    };
    let mut queued = 0;
    for (key, name, payload, max_attempts, tick) in due {
        if seen.contains(&key) {
            continue;
        }
        let mut record = TaskRecord {
            id: None,
            name: name.into(),
            payload,
            status: TaskStatus::Queued,
            attempts: 0,
            max_attempts,
            run_at: tick,
            started_at: None,
            finished_at: None,
            last_error: None,
            created_at: DateTime::now(),
            dedup: Some(key.clone()),
        };
        match record.save().await {
            Ok(()) => queued += 1,
            Err(e) if e.is_unique_violation() => {} // another worker queued this tick first
            // One schedule's trouble mustn't stop the others; the next poll tries it again.
            Err(e) => {
                eprintln!("rangoli scheduler: {name}: {e}");
                continue;
            }
        }
        seen.insert(key);
    }
    Ok(queued)
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
    if record.status != TaskStatus::Queued && record.dedup.as_deref().is_some_and(|d| d.starts_with(ONCE)) {
        record.dedup = None; // free the key for the next enqueue_unique
    }
    record.save().await
}

/// Queue failed runs again from scratch (the admin's "Retry now" action).
pub async fn retry(runs: crate::orm::QuerySet<TaskRecord>) -> Result<()> {
    runs.filter(TaskRecord::STATUS.eq(TaskStatus::Failed))
        .update([
            TaskRecord::STATUS.set(TaskStatus::Queued),
            TaskRecord::ATTEMPTS.set(0),
            TaskRecord::RUN_AT.set(DateTime::now()),
            TaskRecord::FINISHED_AT.set_null(),
        ])
        .await?;
    Ok(())
}

/// Run tasks forever on `concurrency` loops, polling every second when idle.
/// Each loop also queues scheduled ticks; the unique `dedup` key keeps that idempotent.
pub async fn work(concurrency: usize) {
    let loops: Vec<_> = (0..concurrency.max(1))
        .map(|_| {
            tokio::spawn(async {
                let mut seen = std::collections::HashSet::new();
                loop {
                    if let Err(e) = queue_schedules(DateTime::now(), &mut seen).await {
                        eprintln!("rangoli scheduler: {e}");
                    }
                    if seen.len() > 10_000 {
                        seen.clear(); // the database still dedups; this only saves writes
                    }
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

#[cfg(test)]
mod tests {
    use super::Cron;
    use crate::DateTime;

    fn at(s: &str) -> DateTime {
        DateTime::parse(s).unwrap()
    }

    #[test]
    fn cron_matches_like_vixie() {
        let every15 = Cron::parse("*/15 * * * *").unwrap();
        assert!(every15.matches(at("2026-10-13T10:45")) && !every15.matches(at("2026-10-13T10:46")));
        let weekdays_3am = Cron::parse("0 3 * * 1-5").unwrap();
        assert!(weekdays_3am.matches(at("2026-10-13T03:00")), "a Tuesday");
        assert!(!weekdays_3am.matches(at("2026-10-11T03:00")), "a Sunday");
        // Both day fields restricted: either one fires (the 13th, or any Friday).
        let either = Cron::parse("0 0 13 * 5").unwrap();
        assert!(either.matches(at("2026-10-13T00:00")) && either.matches(at("2026-10-16T00:00")));
        assert!(!either.matches(at("2026-10-14T00:00")));
        assert_eq!(Cron::parse("0 0 * * 7"), Cron::parse("0 0 * * 0"), "7 is Sunday");
        assert_eq!(Cron::parse("@daily"), Cron::parse("0 0 * * *"));
        let list = Cron::parse("5,10-12,50-59/5 * * * *").unwrap();
        let fired: Vec<u32> = (0..60).filter(|m| list.matches(DateTime::from_unix(i64::from(*m) * 60))).collect();
        assert_eq!(fired, [5, 10, 11, 12, 50, 55]);
    }

    #[test]
    fn next_after_and_rejects() {
        let leap = Cron::parse("0 12 29 2 *").unwrap();
        assert_eq!(leap.next_after(at("2026-10-13T00:00")).unwrap().to_string(), "2028-02-29T12:00:00Z");
        assert_eq!(Cron::parse("@monthly").unwrap().next_after(at("2026-12-15T09:00")).unwrap().to_string(), "2027-01-01T00:00:00Z");
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
            "* * * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?}");
        }
    }
}

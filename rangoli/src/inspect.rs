//! What each request did to the database: every SQL statement with its time, N+1 patterns
//! flagged. On with `RANGOLI_DEBUG=1` or `App::inspect_queries()`. Staff see it in the admin
//! (**Requests**) and staff AI agents get an `inspect_requests` MCP tool, so you can ask
//! "why is /orders slow?" and the agent sees the query that ran 50 times.
//!
//! Captured where the ORM runs SQL, so raw ORM queries, transactions and the admin are all
//! included (silk misses raw SQL, #80). The last 200 requests, in memory, per process.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Instant;

const KEEP: usize = 200;
/// The same statement this many times in one request is flagged as a likely N+1.
pub const REPEAT_WARNING: usize = 5;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Statement {
    pub sql: String,
    pub ms: f64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RequestLog {
    pub at: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ms: f64,
    pub statements: Vec<Statement>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Repeat {
    pub sql: String,
    pub times: usize,
    pub ms: f64,
}

impl RequestLog {
    pub fn db_ms(&self) -> f64 {
        self.statements.iter().map(|s| s.ms).sum()
    }

    /// Statements run [`REPEAT_WARNING`] or more times: the usual sign of a loop doing one query per row.
    pub fn repeats(&self) -> Vec<Repeat> {
        let mut by_sql: HashMap<&str, (usize, f64)> = HashMap::new();
        for s in &self.statements {
            let e = by_sql.entry(&s.sql).or_default();
            e.0 += 1;
            e.1 += s.ms;
        }
        let mut out: Vec<Repeat> = by_sql
            .into_iter()
            .filter(|(_, (n, _))| *n >= REPEAT_WARNING)
            .map(|(sql, (times, ms))| Repeat { sql: sql.to_string(), times, ms })
            .collect();
        out.sort_by(|a, b| b.times.cmp(&a.times));
        out
    }
}

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LOG: LazyLock<Mutex<VecDeque<RequestLog>>> = LazyLock::new(Default::default);

tokio::task_local! {
    static CURRENT: Mutex<Vec<Statement>>;
}

pub(crate) fn enable() {
    ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Called by the ORM after each statement.
pub(crate) fn record(sql: &str, started: Instant) {
    let _ = CURRENT.try_with(|c| {
        c.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Statement { sql: sql.to_string(), ms: started.elapsed().as_secs_f64() * 1000.0 })
    });
}

/// The recorded requests, newest first.
pub fn recent() -> Vec<RequestLog> {
    LOG.lock().unwrap_or_else(PoisonError::into_inner).iter().rev().cloned().collect()
}

pub(crate) async fn middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if !enabled() || path.starts_with("/admin/static") || path.starts_with("/admin/requests") {
        return next.run(req).await;
    }
    let (method, started) = (req.method().to_string(), Instant::now());
    let (res, statements) = CURRENT
        .scope(Mutex::new(vec![]), async {
            let res = next.run(req).await;
            let statements = CURRENT.with(|c| std::mem::take(&mut *c.lock().unwrap_or_else(PoisonError::into_inner)));
            (res, statements)
        })
        .await;
    let entry = RequestLog {
        at: crate::DateTime::now().to_string(),
        method,
        path,
        status: res.status().as_u16(),
        ms: started.elapsed().as_secs_f64() * 1000.0,
        statements,
    };
    let mut log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
    if log.len() >= KEEP {
        log.pop_front();
    }
    log.push_back(entry);
    res
}

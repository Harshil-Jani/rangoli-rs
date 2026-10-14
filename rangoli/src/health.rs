//! `GET /-/health` for load balancers, Kubernetes probes and uptime monitors.
//!
//! - `200 {"status": "ok"}` when the database answers.
//! - `200 {"status": "warn"}` when it answers but due background jobs have waited over five
//!   minutes (workers down or swamped). The app still serves, so a probe shouldn't restart it.
//! - `503 {"status": "down"}` when the database doesn't answer within two seconds.
//!
//! No version, hostnames or error text: the endpoint is public.

use crate::orm::Model;
use crate::tasks::{TaskRecord, TaskStatus};
use crate::DateTime;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use std::time::{Duration, Instant};

const DB_TIMEOUT: Duration = Duration::from_secs(2);
const BACKLOG_WARN_SECS: i64 = 300;

pub(crate) async fn health() -> Response {
    let started = Instant::now();
    let ping = tokio::time::timeout(DB_TIMEOUT, sqlx::query("SELECT 1").execute(&crate::orm::db().pool)).await;
    let ms = started.elapsed().as_millis() as u64;
    if !matches!(ping, Ok(Ok(_))) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "down", "database": { "ok": false } }))).into_response();
    }
    let now = DateTime::now();
    let due = || TaskRecord::objects().filter(TaskRecord::STATUS.eq(TaskStatus::Queued) & TaskRecord::RUN_AT.lte(now));
    let (waiting, oldest) = match (due().count().await, due().order_by(TaskRecord::RUN_AT.asc()).first().await) {
        (Ok(n), Ok(first)) => (n, first.map_or(0, |t| now.unix() - t.run_at.unix())),
        _ => (0, 0), // no task table yet (not migrated): nothing is waiting
    };
    let status = if oldest > BACKLOG_WARN_SECS { "warn" } else { "ok" };
    Json(json!({
        "status": status,
        "database": { "ok": true, "ms": ms },
        "tasks": { "waiting": waiting, "oldest_wait_secs": oldest },
    }))
    .into_response()
}

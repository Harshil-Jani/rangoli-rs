//! Request rate limits for the JSON API and the MCP server: a fixed one-minute window per
//! caller (the signed-in user, else the client's IP). Over the limit is a 429 with
//! `Retry-After`. The MCP spec says servers must rate limit tool calls; a runaway agent loop
//! should hit a wall, not the database.
//!
//! ponytail: counts live in this process, so N instances allow N times the limit. That stops
//! runaway loops; a shared limit for abuse across instances belongs in the database or a proxy.

use crate::auth::CurrentUser;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

pub(crate) struct Limiter {
    per_minute: u32,
    hits: Mutex<HashMap<String, (i64, u32)>>,
}

impl Limiter {
    pub(crate) fn new(per_minute: u32) -> Arc<Self> {
        Arc::new(Limiter { per_minute, hits: Mutex::new(HashMap::new()) })
    }

    /// Count one request for `key` at `now` (Unix seconds); `Err(seconds to wait)` over the limit.
    fn hit(&self, key: &str, now: i64) -> Result<(), i64> {
        let window = now.div_euclid(60);
        let mut hits = self.hits.lock().unwrap_or_else(PoisonError::into_inner);
        if hits.len() > 100_000 {
            hits.retain(|_, (w, _)| *w == window); // forget callers from past windows
        }
        let entry = hits.entry(key.to_string()).or_insert((window, 0));
        if entry.0 != window {
            *entry = (window, 0);
        }
        entry.1 += 1;
        if entry.1 > self.per_minute {
            Err(60 - now.rem_euclid(60))
        } else {
            Ok(())
        }
    }
}

pub(crate) async fn middleware(State(limiter): State<Arc<Limiter>>, req: Request, next: Next) -> Response {
    let user = req.extensions().get::<CurrentUser>().and_then(|u| u.0.as_ref()).and_then(|u| u.id);
    let key = match (user, req.extensions().get::<ConnectInfo<SocketAddr>>()) {
        (Some(id), _) => format!("user:{id}"),
        (None, Some(ConnectInfo(addr))) => format!("ip:{}", addr.ip()),
        (None, None) => "anonymous".into(), // in-process calls and tests, with no socket
    };
    match limiter.hit(&key, crate::auth::now()) {
        Ok(()) => next.run(req).await,
        Err(wait) => {
            let body = axum::Json(serde_json::json!({ "detail": format!("Too many requests. Try again in {wait} seconds.") }));
            (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, wait.to_string())], body).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixed_window_per_key() {
        let l = super::Limiter::new(3);
        let t = 1_800_000_010; // 10 s into a minute
        assert!((0..3).all(|_| l.hit("a", t).is_ok()));
        assert_eq!(l.hit("a", t + 5), Err(45), "the 4th in the minute waits for the next window");
        assert!(l.hit("b", t).is_ok(), "callers are counted apart");
        assert!(l.hit("a", t + 50).is_ok(), "a new minute starts fresh");
    }
}

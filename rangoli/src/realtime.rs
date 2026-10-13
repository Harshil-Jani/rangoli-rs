//! Real-time: WebSockets and Server-Sent Events with publish/subscribe groups.
//!
//! What Django needs Channels, a Redis channel layer and an ASGI server for. Rangoli is
//! async throughout, so a WebSocket is just another handler:
//!
//! ```ignore
//! async fn chat(ws: WebSocketUpgrade, headers: HeaderMap, Path(room): Path<String>) -> Response {
//!     // Everything a client sends goes to everyone in the room (return None to drop it).
//!     realtime::serve(ws, &headers, format!("chat:{room}"), |text| async move { Some(text) })
//! }
//!
//! realtime::publish("chat:lobby", "deploy finished"); // from any view or background task
//! ```
//!
//! On Postgres, `runserver` and `worker` also join every other instance through
//! LISTEN/NOTIFY, so a publish from any process (a web server, a background worker) reaches
//! subscribers on all of them, with no Redis. Delivery across instances is at most once,
//! like Channels' Redis layer: a message sent while an instance's listener is reconnecting
//! is not replayed. On MySQL and SQLite groups live in one process.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::broadcast;

/// Messages a slow subscriber may fall behind by before it skips ahead.
const BUFFER: usize = 256;

static GROUPS: LazyLock<Mutex<HashMap<String, broadcast::Sender<String>>>> = LazyLock::new(Default::default);

fn sender(group: &str) -> broadcast::Sender<String> {
    let mut groups = GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
    // Drop groups nobody listens to any more, so the map can't grow without bound.
    groups.retain(|name, tx| name == group || tx.receiver_count() > 0);
    groups.entry(group.to_string()).or_insert_with(|| broadcast::channel(BUFFER).0).clone()
}

/// Send `message` to everyone subscribed to `group`, in this process and (on Postgres) every
/// other instance. Returns how many local subscribers received it.
pub fn publish(group: &str, message: impl Into<String>) -> usize {
    let message = message.into();
    #[cfg(feature = "postgres")]
    cluster::forward(group, &message);
    sender(group).send(message).unwrap_or(0)
}

#[cfg(feature = "postgres")]
pub use cluster::join_cluster;

#[cfg(feature = "postgres")]
mod cluster {
    use std::sync::OnceLock;
    use tokio::sync::mpsc;

    pub(super) const CHANNEL: &str = "rangoli_realtime";
    /// Postgres rejects NOTIFY payloads of 8000 bytes or more.
    pub(super) const MAX_PAYLOAD: usize = 7900;
    /// Publishes waiting for their NOTIFY; beyond this (database down) they stay local.
    const QUEUE: usize = 4096;

    static OUTBOX: OnceLock<mpsc::Sender<String>> = OnceLock::new();
    pub(super) static INSTANCE: OnceLock<String> = OnceLock::new();

    #[derive(serde::Serialize, serde::Deserialize)]
    pub(super) struct Envelope {
        pub(super) from: String,
        pub(super) group: String,
        pub(super) message: String,
    }

    pub(super) fn forward(group: &str, message: &str) {
        let (Some(outbox), Some(from)) = (OUTBOX.get(), INSTANCE.get()) else { return };
        let env = Envelope { from: from.clone(), group: group.into(), message: message.into() };
        let Ok(payload) = serde_json::to_string(&env) else { return };
        if payload.len() > MAX_PAYLOAD {
            eprintln!(
                "rangoli realtime: a {}-byte message to `{group}` is too big for NOTIFY; delivered on this instance only",
                payload.len()
            );
        } else if outbox.try_send(payload).is_err() {
            eprintln!("rangoli realtime: NOTIFY queue full (is the database down?); delivered on this instance only");
        }
    }

    /// Fan publishes out to, and in from, every instance on this Postgres database.
    /// `runserver` and `worker` call it; calling it again does nothing.
    pub async fn join_cluster(database_url: &str) -> crate::Result<()> {
        if INSTANCE.get().is_some() {
            return Ok(());
        }
        let mut listener = sqlx::postgres::PgListener::connect(database_url).await?;
        listener.listen(CHANNEL).await?;
        let me = crate::auth::random_hex(8)?;
        if INSTANCE.set(me.clone()).is_err() {
            return Ok(());
        }
        // In: deliver what other instances published. PgListener reconnects by itself.
        tokio::spawn(async move {
            loop {
                match listener.recv().await {
                    Ok(n) => match serde_json::from_str::<Envelope>(n.payload()) {
                        Ok(env) if env.from != me => {
                            let _ = super::sender(&env.group).send(env.message);
                        }
                        _ => {}
                    },
                    Err(e) => {
                        eprintln!("rangoli realtime: listener: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        });
        // Out: one sender, so messages leave in the order they were published.
        let (tx, mut rx) = mpsc::channel::<String>(QUEUE);
        let _ = OUTBOX.set(tx);
        tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                let params = vec![
                    (crate::orm::Value::Text(CHANNEL.into()), crate::orm::FieldType::Text),
                    (crate::orm::Value::Text(payload), crate::orm::FieldType::Text),
                ];
                if let Err(e) = crate::orm::execute("SELECT pg_notify($1, $2)", &params).await {
                    eprintln!("rangoli realtime: NOTIFY failed: {e}");
                }
            }
        });
        Ok(())
    }
}

/// Subscribe to `group` yourself (for custom handlers).
pub fn subscribe(group: &str) -> broadcast::Receiver<String> {
    sender(group).subscribe()
}

/// Number of live subscribers in `group`.
pub fn subscribers(group: &str) -> usize {
    GROUPS.lock().unwrap_or_else(PoisonError::into_inner).get(group).map_or(0, broadcast::Sender::receiver_count)
}

/// A WebSocket handshake is a GET, so the usual CSRF checks don't run. A page on another
/// site could open a socket with the user's cookies (cross-site WebSocket hijacking), so
/// browsers' `Origin` must match the host. Clients that send no Origin (not browsers) pass.
fn same_origin(headers: &HeaderMap) -> bool {
    let get = |name| headers.get(name).and_then(|v| v.to_str().ok());
    match (get(header::ORIGIN), get(header::HOST)) {
        (Some(origin), Some(host)) => origin.split_once("://").map(|(_, h)| h) == Some(host),
        (Some(_), None) => false,
        (None, _) => true,
    }
}

/// Upgrade to a WebSocket in `group`: group messages go to the client; each text message
/// from the client goes through `on_message`, and `Some(text)` is published to the group.
pub fn serve<F, Fut>(ws: WebSocketUpgrade, headers: &HeaderMap, group: impl Into<String>, on_message: F) -> Response
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<String>> + Send + 'static,
{
    if !same_origin(headers) {
        return (StatusCode::FORBIDDEN, "Cross-origin WebSocket refused").into_response();
    }
    let group = group.into();
    ws.max_message_size(64 * 1024).on_upgrade(move |socket| run(socket, group, on_message))
}

async fn run<F, Fut>(mut socket: WebSocket, group: String, on_message: F)
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<String>> + Send + 'static,
{
    let mut rx = subscribe(&group);
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Some(out) = on_message(text.to_string()).await {
                        publish(&group, out);
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {} // binary, ping and pong frames need no answer from us
            },
            outgoing = rx.recv() => match outgoing {
                Ok(text) => if socket.send(Message::Text(text.into())).await.is_err() { break },
                // Too slow to keep up: skip what was missed rather than stall everyone else.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = ping.tick() => if socket.send(Message::Ping(Vec::new().into())).await.is_err() { break },
        }
    }
}

/// Server-Sent Events for `group`: a one-way stream browsers reconnect to on their own.
pub fn events(group: &str) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = subscribe(group);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(text) => return Some((Ok(Event::default().data(text)), rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_must_match_host() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, v.parse().unwrap());
            }
            m
        };
        assert!(same_origin(&h(&[("origin", "https://app.example"), ("host", "app.example")])));
        assert!(!same_origin(&h(&[("origin", "https://evil.example"), ("host", "app.example")])));
        assert!(same_origin(&h(&[("host", "app.example")])), "non-browser clients send no Origin");
    }

    #[tokio::test]
    async fn publish_reaches_subscribers_and_idle_groups_are_dropped() {
        assert_eq!(publish("t:none", "nobody"), 0);
        let mut a = subscribe("t:room");
        let mut b = subscribe("t:room");
        assert_eq!((publish("t:room", "hi"), subscribers("t:room")), (2, 2));
        assert_eq!((a.recv().await.unwrap(), b.recv().await.unwrap()), ("hi".into(), "hi".into()));
        drop((a, b));
        publish("t:other", "x");
        assert!(!GROUPS.lock().unwrap().contains_key("t:room"), "empty groups are cleaned up");
    }
}

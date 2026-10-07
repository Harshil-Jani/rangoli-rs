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
//! Groups live in this process. ponytail: one instance; fan out through Postgres
//! LISTEN/NOTIFY when an app runs several.

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

/// Send `message` to everyone subscribed to `group`. Returns how many received it.
pub fn publish(group: &str, message: impl Into<String>) -> usize {
    sender(group).send(message.into()).unwrap_or(0)
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

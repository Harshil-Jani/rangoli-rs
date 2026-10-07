//! WebSockets and Server-Sent Events against a real running server.
#![cfg(feature = "realtime")]

use axum::extract::{ws::WebSocketUpgrade, Path};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use rangoli::{realtime, App};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

/// A chat room that shouts: every message is upper-cased; empty ones are dropped.
async fn chat(ws: WebSocketUpgrade, headers: HeaderMap, Path(room): Path<String>) -> axum::response::Response {
    realtime::serve(ws, &headers, format!("chat:{room}"), |text| async move { (!text.is_empty()).then(|| text.to_uppercase()) })
}

async fn next_text<S>(ws: &mut S) -> String
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("no message within 5s").unwrap().unwrap() {
            Message::Text(t) => return t.to_string(),
            _ => continue,
        }
    }
}

async fn until(f: impl Fn() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never became true");
}

#[tokio::test]
async fn websockets_and_server_sent_events() {
    let app = App::new().routes(Router::new().route("/ws/{room}", get(chat)).route("/events", get(|| async { realtime::events("news") })));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app.router()).await.unwrap() });

    let (mut a, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/lobby")).await.unwrap();
    let (mut b, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/lobby")).await.unwrap();
    let (mut other, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/kitchen")).await.unwrap();
    until(|| realtime::subscribers("chat:lobby") == 2 && realtime::subscribers("chat:kitchen") == 1).await;

    a.send(Message::Text("hello".into())).await.unwrap();
    assert_eq!(next_text(&mut b).await, "HELLO", "a room hears each other, through the handler");
    assert_eq!(next_text(&mut a).await, "HELLO", "including the sender");

    a.send(Message::Text("".into())).await.unwrap(); // dropped by the handler
    assert_eq!(realtime::publish("chat:lobby", "deploy finished"), 2, "server code reaches every socket in the room");
    assert_eq!(next_text(&mut b).await, "deploy finished", "the dropped message never arrived");

    realtime::publish("chat:kitchen", "kitchen only");
    assert_eq!(next_text(&mut other).await, "kitchen only", "rooms are separate");

    drop(b);
    until(|| realtime::subscribers("chat:lobby") == 1).await;

    // Cross-site WebSocket hijacking: a page on another site can't open a socket with our cookies.
    let mut req = format!("ws://{addr}/ws/lobby").into_client_request().unwrap();
    req.headers_mut().insert("origin", "https://evil.example".parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err().to_string();
    assert!(err.contains("403"), "{err}");
    let mut req = format!("ws://{addr}/ws/lobby").into_client_request().unwrap();
    req.headers_mut().insert("origin", format!("http://{addr}").parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_ok(), "same-origin browsers are welcome");

    // Server-Sent Events.
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    tcp.write_all(b"GET /events HTTP/1.1\r\nhost: localhost\r\naccept: text/event-stream\r\n\r\n").await.unwrap();
    until(|| realtime::subscribers("news") == 1).await;
    realtime::publish("news", "tick");
    let mut seen = String::new();
    let mut buf = [0u8; 1024];
    while !seen.contains("data: tick") {
        let n = tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut buf)).await.expect("no event within 5s").unwrap();
        assert!(n > 0, "stream closed early: {seen}");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(seen.contains("text/event-stream"));
}

//! `rangoli::ai`: OpenAI-compatible chat and embeddings, against a local stand-in server.
#![cfg(feature = "webhooks")]

use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[tokio::test]
async fn openai_compatible_chat_and_embeddings() {
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(|headers: axum::http::HeaderMap, Json(b): Json<Value>| async move {
                let auth = headers.get("authorization").map(|v| v.to_str().unwrap().to_string());
                Json(json!({ "choices": [{ "message": { "content": format!("{} {} {:?}", b["model"], b["messages"][0]["content"], auth) } }] }))
            }),
        )
        .route("/v1/embeddings", post(|| async { Json(json!({ "data": [{ "embedding": [0.5, -1.0] }] })) }))
        .route("/bad/chat/completions", post(|| async { (axum::http::StatusCode::UNAUTHORIZED, Json(json!({ "error": { "message": "bad key" } }))) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base: &'static str = Box::leak(format!("http://{}/v1", listener.local_addr().unwrap()).into_boxed_str());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let chat = rangoli::ai::openai_compatible(base, "llama", None);
    assert_eq!(chat("hi".into()).await.unwrap(), "\"llama\" \"hi\" None");
    std::env::set_var("AI_TEST_KEY", "sk-test");
    let keyed = rangoli::ai::openai_compatible(base, "m", Some("AI_TEST_KEY"));
    assert!(keyed("x".into()).await.unwrap().contains("Bearer sk-test"), "the key goes in the header");
    assert!(rangoli::ai::openai_compatible(base, "m", Some("AI_TEST_MISSING"))("x".into())
        .await
        .unwrap_err()
        .to_string()
        .contains("not set"));
    assert_eq!(rangoli::ai::embeddings(base, "e", None)("text".into()).await.unwrap(), vec![0.5, -1.0]);
    let bad: &'static str = Box::leak(base.replace("/v1", "/bad").into_boxed_str());
    let err = rangoli::ai::openai_compatible(bad, "m", None)("x".into()).await.unwrap_err().to_string();
    assert!(err.contains("401") && err.contains("bad key"), "{err}");
}

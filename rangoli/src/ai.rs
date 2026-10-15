//! Language models and embeddings in one line, for `App::llm` and `App::embed`:
//!
//! ```ignore
//! App::new()
//!     .llm(ai::anthropic("claude-haiku-5-5"))                       // ANTHROPIC_API_KEY
//!     .embed(ai::openai_embeddings("text-embedding-3-small"))       // OPENAI_API_KEY
//! // or any OpenAI-compatible server: OpenAI, OpenRouter, Groq, a local Ollama
//!     .llm(ai::openai_compatible("http://localhost:11434/v1", "llama3.2", None))
//! ```
//!
//! Keys come from the environment when each call is made, never from code. Calls go over the
//! HTTP client webhooks use (the `webhooks` feature), with a 60 second limit.

use crate::{Error, Result};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;

type Reply<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

fn key(env: &str) -> Result<String> {
    std::env::var(env).ok().filter(|k| !k.is_empty()).ok_or_else(|| Error::Config(format!("{env} is not set")))
}

async fn call(url: &str, body: Value, headers: &[(&str, &str)]) -> Result<Value> {
    let (status, text) = crate::webhooks::post(url, &body.to_string(), headers, 60).await?;
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    if !(200..300).contains(&status) {
        let detail = json["error"]["message"].as_str().map_or_else(|| json.to_string(), str::to_owned);
        return Err(Error::Task(format!("{url} answered {status}: {}", detail.chars().take(300).collect::<String>())));
    }
    Ok(json)
}

/// Anthropic's Messages API, key in `ANTHROPIC_API_KEY`.
pub fn anthropic(model: &'static str) -> impl Fn(String) -> Reply<String> + Send + Sync + 'static {
    move |prompt| {
        Box::pin(async move {
            let key = key("ANTHROPIC_API_KEY")?;
            let body = json!({ "model": model, "max_tokens": 2048, "messages": [{ "role": "user", "content": prompt }] });
            let r =
                call("https://api.anthropic.com/v1/messages", body, &[("x-api-key", &key), ("anthropic-version", "2023-06-01")]).await?;
            text(&r["content"][0]["text"])
        })
    }
}

/// Any server with OpenAI's `/chat/completions`: `base` like `https://api.openai.com/v1`, and the
/// environment variable holding its key (`None` for a local server that needs none).
pub fn openai_compatible(
    base: &'static str,
    model: &'static str,
    key_env: Option<&'static str>,
) -> impl Fn(String) -> Reply<String> + Send + Sync + 'static {
    move |prompt| {
        Box::pin(async move {
            let auth = key_env.map(key).transpose()?.map(|k| format!("Bearer {k}"));
            let headers: Vec<(&str, &str)> = auth.as_deref().map(|a| ("authorization", a)).into_iter().collect();
            let body = json!({ "model": model, "messages": [{ "role": "user", "content": prompt }] });
            let r = call(&format!("{}/chat/completions", base.trim_end_matches('/')), body, &headers).await?;
            text(&r["choices"][0]["message"]["content"])
        })
    }
}

/// OpenAI, key in `OPENAI_API_KEY`.
pub fn openai(model: &'static str) -> impl Fn(String) -> Reply<String> + Send + Sync + 'static {
    openai_compatible("https://api.openai.com/v1", model, Some("OPENAI_API_KEY"))
}

/// Embeddings from any server with OpenAI's `/embeddings` (`base`, key variable as above).
pub fn embeddings(
    base: &'static str,
    model: &'static str,
    key_env: Option<&'static str>,
) -> impl Fn(String) -> Reply<Vec<f32>> + Send + Sync + 'static {
    move |input| {
        Box::pin(async move {
            let auth = key_env.map(key).transpose()?.map(|k| format!("Bearer {k}"));
            let headers: Vec<(&str, &str)> = auth.as_deref().map(|a| ("authorization", a)).into_iter().collect();
            let r =
                call(&format!("{}/embeddings", base.trim_end_matches('/')), json!({ "model": model, "input": input }), &headers).await?;
            let v = r["data"][0]["embedding"].as_array().ok_or_else(|| Error::Task("no embedding in the reply".into()))?;
            Ok(v.iter().filter_map(Value::as_f64).map(|x| x as f32).collect())
        })
    }
}

/// OpenAI's embeddings, key in `OPENAI_API_KEY`.
pub fn openai_embeddings(model: &'static str) -> impl Fn(String) -> Reply<Vec<f32>> + Send + Sync + 'static {
    embeddings("https://api.openai.com/v1", model, Some("OPENAI_API_KEY"))
}

fn text(v: &Value) -> Result<String> {
    v.as_str().map(str::to_owned).ok_or_else(|| Error::Task("the model's reply had no text".into()))
}

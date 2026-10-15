//! Outgoing webhooks, managed in the admin: every add, change or delete made through the admin,
//! the JSON API or an AI agent (MCP) is POSTed as JSON to the URLs you list, for Slack, Zapier,
//! n8n or your own service. Each delivery is signed and retried by the task queue.
//!
//! The body: `{"event": "added|changed|deleted", "table", "id", "object", "message", "user", "at"}`.
//! The row's data isn't included (fetch it from the API if you need it), so a webhook can't leak
//! private or encrypted fields. `X-Rangoli-Signature: sha256=<hex>` is an HMAC-SHA256 of the body
//! with the webhook's secret; check it before trusting a request.

use crate::orm::Model;
use crate::{DateTime, Error, Result};

/// A URL to notify, as stored in `rangoli_webhook`.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_webhook", display = "name")]
pub struct Webhook {
    pub id: Option<i64>,
    #[field(max_length = 100)]
    pub name: String,
    /// `https://...` (or `http://` for a service on your own network).
    #[field(max_length = 500)]
    pub url: String,
    /// Only changes to this table; empty for every table.
    #[field(max_length = 100)]
    pub table_name: String,
    /// Which events, comma-separated: `added,changed,deleted`.
    #[field(max_length = 50, default = "added,changed,deleted")]
    pub events: String,
    /// Signs each delivery (HMAC-SHA256). Keep it secret; receivers use it to verify.
    #[field(max_length = 100, private)]
    pub secret: String,
    #[field(default = true)]
    pub active: bool,
}

/// One delivery attempt's job, run by the task queue (retries with backoff on failure).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Delivery {
    pub webhook: i64,
    pub event: String,
    pub body: String,
}

fn event_name(action: i64) -> &'static str {
    match action {
        1 => "added",
        2 => "changed",
        _ => "deleted",
    }
}

/// Queue deliveries for one History entry to every matching active webhook. Called where the
/// entry is written, inside the change's transaction: a rolled-back change sends nothing.
pub(crate) async fn fire(entry: &crate::admin::LogEntry) -> Result<()> {
    crate::hooks::queue(entry).await?;
    if !cfg!(feature = "webhooks") {
        return Ok(());
    }
    let event = event_name(entry.action);
    let hooks = Webhook::objects()
        .filter(Webhook::ACTIVE.eq(true) & (Webhook::TABLE_NAME.eq(entry.table_name.as_str()) | Webhook::TABLE_NAME.eq("")))
        .all()
        .await?;
    let user = crate::auth::User::get(entry.user_id).await.map(|u| u.username).unwrap_or_default();
    for h in hooks.into_iter().filter(|h| h.events.split(',').any(|e| e.trim() == event)) {
        let body = serde_json::json!({
            "event": event,
            "table": entry.table_name,
            "id": entry.object_id,
            "object": entry.object_repr,
            "message": entry.message,
            "user": user,
            "at": DateTime::from_unix(entry.at).to_string(),
        })
        .to_string();
        let job = Delivery { webhook: h.id.unwrap_or_default(), event: event.into(), body };
        crate::tasks::Task::enqueue(&job).await?;
    }
    Ok(())
}

/// `sha256=<hex>` of `body` under `secret`.
pub fn signature(secret: &str, body: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = ring::hmac::sign(&key, body.as_bytes());
    format!("sha256={}", tag.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>())
}

impl crate::tasks::Task for Delivery {
    const NAME: &'static str = "rangoli.webhook";
    const MAX_ATTEMPTS: i64 = 5;

    async fn run(self) -> Result<()> {
        let Some(hook) = Webhook::objects().filter(Webhook::ID.eq(self.webhook)).first().await? else { return Ok(()) };
        if !hook.active {
            return Ok(()); // turned off since it was queued
        }
        let sig = signature(&hook.secret, &self.body);
        let status = post(&hook.url, &self.body, &[("x-rangoli-event", &self.event), ("x-rangoli-signature", &sig)]).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(Error::Task(format!("webhook {} answered {status}", hook.name)))
        }
    }
}

/// POST a JSON body and return the status. A minimal HTTP/1.1 client (http and https) over the
/// hyper and rustls already in the tree, with a 10 second limit.
#[cfg(feature = "webhooks")]
async fn post(url: &str, body: &str, headers: &[(&str, &str)]) -> Result<u16> {
    use http_body_util::Full;
    use hyper_util::rt::TokioIo;
    let bad = |m: &str| Error::Task(format!("webhook url {url}: {m}"));
    let (tls, rest) = match url.split_once("://") {
        Some(("https", r)) => (true, r),
        Some(("http", r)) => (false, r),
        _ => return Err(bad("must start with http:// or https://")),
    };
    let (authority, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(a, p)| (a, format!("/{p}")));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| bad("bad port"))?),
        None => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    let mut req = hyper::Request::post(&path)
        .header("host", authority)
        .header("content-type", "application/json")
        .header("user-agent", concat!("rangoli/", env!("CARGO_PKG_VERSION")));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Full::new(bytes::Bytes::from(body.to_string()))).map_err(|e| bad(&e.to_string()))?;
    let send = async {
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await.map_err(|e| bad(&e.to_string()))?;
        let status = if tls {
            let mut roots = tokio_rustls::rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
            let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| bad(&e.to_string()))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| bad(&e.to_string()))?;
            let stream =
                tokio_rustls::TlsConnector::from(std::sync::Arc::new(config)).connect(name, tcp).await.map_err(|e| bad(&e.to_string()))?;
            exchange(TokioIo::new(stream), req).await?
        } else {
            exchange(TokioIo::new(tcp), req).await?
        };
        Ok::<u16, Error>(status)
    };
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), send).await.map_err(|_| bad("timed out"))??;
    Ok(status)
}

#[cfg(feature = "webhooks")]
async fn exchange<T>(io: T, req: hyper::Request<http_body_util::Full<bytes::Bytes>>) -> Result<u16>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.map_err(|e| Error::Task(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let res = sender.send_request(req).await.map_err(|e| Error::Task(e.to_string()))?;
    Ok(res.status().as_u16())
}

#[cfg(not(feature = "webhooks"))]
async fn post(_: &str, _: &str, _: &[(&str, &str)]) -> Result<u16> {
    Err(Error::Task("webhooks need the `webhooks` feature".into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn signs_like_github_does() {
        // HMAC-SHA256("It's a Secret to Everybody", "Hello, World!"), GitHub's documented example.
        assert_eq!(
            super::signature("It's a Secret to Everybody", "Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
    }
}

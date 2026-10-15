//! Sending email: `mail::send(Mail::new(to, subject, text))`, queued and retried by the task
//! queue, delivered over SMTP.
//!
//! - `RANGOLI_SMTP_URL`: `smtp://user:pass@host:587` (STARTTLS, required), `smtps://...:465`
//!   (TLS from the start), or `smtp://localhost:1025` (plain, allowed only to this machine).
//! - `RANGOLI_EMAIL_FROM`: the sender, e.g. `Acme <no-reply@acme.com>`.
//!
//! Without `RANGOLI_SMTP_URL`, mail is printed to the console instead (Django's console backend),
//! so development needs no mail server. Addresses and subjects with line breaks are refused, so
//! user input can't add headers.

use crate::{Error, Result};

/// One email. `html` is optional; the text part is always sent.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Mail {
    pub to: Vec<String>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
}

impl Mail {
    pub fn new(to: &str, subject: &str, text: &str) -> Mail {
        Mail { to: vec![to.to_string()], subject: subject.into(), text: text.into(), html: None }
    }
    pub fn html(mut self, html: &str) -> Mail {
        self.html = Some(html.into());
        self
    }
}

/// Queue `mail` for delivery (retried with backoff if the server is down). Returns at once.
pub async fn send(mail: Mail) -> Result<()> {
    check(&mail)?;
    crate::tasks::Task::enqueue(&mail).await.map(drop)
}

fn check(mail: &Mail) -> Result<()> {
    let bad = |s: &str| s.contains(['\r', '\n']);
    if mail.to.is_empty() || mail.to.iter().any(|t| bad(t) || !t.contains('@')) || bad(&mail.subject) {
        return Err(Error::Task("a mail needs valid recipients and a one-line subject".into()));
    }
    Ok(())
}

impl crate::tasks::Task for Mail {
    const NAME: &'static str = "rangoli.mail";
    const MAX_ATTEMPTS: i64 = 6;

    async fn run(self) -> Result<()> {
        deliver(&self).await
    }
}

fn from_address() -> String {
    std::env::var("RANGOLI_EMAIL_FROM")
        .ok()
        .filter(|f| !f.is_empty() && !f.contains(['\r', '\n']))
        .unwrap_or_else(|| "webmaster@localhost".into())
}

/// `Name <a@b>` -> `a@b`.
fn bare(addr: &str) -> &str {
    addr.rsplit_once('<').map_or(addr, |(_, a)| a.trim_end_matches('>')).trim()
}

/// RFC 2047 for non-ASCII subjects.
fn header_text(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", crate::django::base64(s.as_bytes()))
    }
}

fn wrapped_base64(s: &str) -> String {
    let b = crate::django::base64(s.as_bytes());
    b.as_bytes().chunks(76).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("\r\n")
}

/// The message as sent: headers and a text (or text + HTML) body, base64-encoded.
pub(crate) fn render(mail: &Mail, from: &str, now: i64) -> String {
    let id = crate::auth::random_hex(12).unwrap_or_default();
    let domain = bare(from).rsplit_once('@').map_or("localhost", |(_, d)| d);
    let date = crate::DateTime::from_unix(now).rfc2822();
    let mut out = format!(
        "From: {from}\r\nTo: {}\r\nSubject: {}\r\nDate: {date}\r\nMessage-ID: <{id}@{domain}>\r\nMIME-Version: 1.0\r\n",
        mail.to.join(", "),
        header_text(&mail.subject)
    );
    let part = |ty: &str, body: &str| {
        format!("Content-Type: {ty}; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n", wrapped_base64(body))
    };
    match &mail.html {
        None => out.push_str(&part("text/plain", &mail.text)),
        Some(html) => {
            let b = format!("rangoli-{id}");
            out.push_str(&format!("Content-Type: multipart/alternative; boundary=\"{b}\"\r\n\r\n"));
            out.push_str(&format!("--{b}\r\n{}--{b}\r\n{}--{b}--\r\n", part("text/plain", &mail.text), part("text/html", html)));
        }
    }
    out
}

/// Deliver now: over SMTP, or to the console when no server is configured.
pub async fn deliver(mail: &Mail) -> Result<()> {
    check(mail)?;
    let from = from_address();
    let msg = render(mail, &from, crate::auth::now());
    match std::env::var("RANGOLI_SMTP_URL").ok().filter(|u| !u.is_empty()) {
        None => {
            println!("---------- email (set RANGOLI_SMTP_URL to send it) ----------\n{msg}\n----------");
            Ok(())
        }
        Some(url) => smtp::send(&url, bare(&from), &mail.to.iter().map(|t| bare(t).to_string()).collect::<Vec<_>>(), &msg).await,
    }
}

#[cfg(feature = "mail")]
mod smtp {
    use crate::{Error, Result};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
    use tokio_rustls::rustls;

    trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
    impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

    fn fail(m: impl std::fmt::Display) -> Error {
        Error::Task(format!("smtp: {m}"))
    }

    struct Conn {
        io: BufReader<Box<dyn Io>>,
    }

    impl Conn {
        /// Read one reply (possibly multi-line) and check its code.
        async fn expect(&mut self, ok: &[u16]) -> Result<String> {
            let mut all = String::new();
            loop {
                let mut line = String::new();
                if self.io.read_line(&mut line).await.map_err(fail)? == 0 {
                    return Err(fail("the server closed the connection"));
                }
                all.push_str(&line);
                if line.len() < 4 || line.as_bytes()[3] != b'-' {
                    let code: u16 = line.get(..3).and_then(|c| c.parse().ok()).unwrap_or(0);
                    return if ok.contains(&code) { Ok(all) } else { Err(fail(all.trim())) };
                }
            }
        }

        async fn cmd(&mut self, line: &str, ok: &[u16]) -> Result<String> {
            self.io.get_mut().write_all(format!("{line}\r\n").as_bytes()).await.map_err(fail)?;
            self.expect(ok).await
        }
    }

    fn tls(host: &str) -> Result<(tokio_rustls::TlsConnector, rustls::pki_types::ServerName<'static>)> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(fail)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(fail)?;
        Ok((tokio_rustls::TlsConnector::from(Arc::new(config)), name))
    }

    pub(super) async fn send(url: &str, from: &str, to: &[String], msg: &str) -> Result<()> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| fail("RANGOLI_SMTP_URL must start with smtp:// or smtps://"))?;
        let (auth, hostport) =
            rest.trim_end_matches('/').rsplit_once('@').map_or((None, rest.trim_end_matches('/')), |(a, h)| (Some(a), h));
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| fail("bad port"))?),
            None => (hostport.to_string(), if scheme == "smtps" { 465 } else { 587 }),
        };
        let local = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1");
        let tcp = tokio::time::timeout(std::time::Duration::from_secs(15), tokio::net::TcpStream::connect((host.as_str(), port)))
            .await
            .map_err(|_| fail("connect timed out"))?
            .map_err(fail)?;
        let io: Box<dyn Io> = if scheme == "smtps" {
            let (c, name) = tls(&host)?;
            Box::new(c.connect(name, tcp).await.map_err(fail)?)
        } else {
            Box::new(tcp)
        };
        let mut c = Conn { io: BufReader::new(io) };
        c.expect(&[220]).await?;
        let ehlo = c.cmd("EHLO rangoli", &[250]).await?;
        if scheme == "smtp" && ehlo.contains("STARTTLS") {
            c.cmd("STARTTLS", &[220]).await?;
            let plain = c.io.into_inner();
            let (conn, name) = tls(&host)?;
            // ponytail: STARTTLS upgrades a boxed stream by re-wrapping it; a TLS failure aborts the send (retried).
            let upgraded: Box<dyn Io> = Box::new(conn.connect(name, plain).await.map_err(fail)?);
            c = Conn { io: BufReader::new(upgraded) };
            c.cmd("EHLO rangoli", &[250]).await?;
        } else if scheme == "smtp" && !local {
            return Err(fail(format!("{host} doesn't offer STARTTLS; refusing to send credentials and mail in the clear")));
        }
        if let Some(auth) = auth {
            let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
            let decode = |s: &str| {
                let mut out = vec![];
                let mut bytes = s.bytes();
                while let Some(b) = bytes.next() {
                    if b == b'%' {
                        let h: String = bytes.by_ref().take(2).map(char::from).collect();
                        out.push(u8::from_str_radix(&h, 16).unwrap_or(b'?'));
                    } else {
                        out.push(b);
                    }
                }
                String::from_utf8_lossy(&out).into_owned()
            };
            let token = crate::django::base64(format!("\0{}\0{}", decode(user), decode(pass)).as_bytes());
            c.cmd(&format!("AUTH PLAIN {token}"), &[235]).await?;
        }
        c.cmd(&format!("MAIL FROM:<{from}>"), &[250]).await?;
        for t in to {
            c.cmd(&format!("RCPT TO:<{t}>"), &[250, 251]).await?;
        }
        c.cmd("DATA", &[354]).await?;
        // Dot-stuffing: a line that starts with "." gets another one.
        let body: String =
            msg.split("\r\n").map(|l| if l.starts_with('.') { format!(".{l}") } else { l.to_string() }).collect::<Vec<_>>().join("\r\n");
        c.cmd(&format!("{body}\r\n."), &[250]).await?;
        c.cmd("QUIT", &[221]).await.ok();
        Ok(())
    }
}

#[cfg(not(feature = "mail"))]
mod smtp {
    pub(super) async fn send(_: &str, _: &str, _: &[String], _: &str) -> crate::Result<()> {
        Err(crate::Error::Task("SMTP needs the `mail` feature".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_refuses_header_injection() {
        let m = Mail::new("bob@example.com", "Héllo", "line one\n.dot line").html("<p>hi</p>");
        let out = render(&m, "Acme <no-reply@acme.com>", 1_800_000_000);
        assert!(out.contains("Subject: =?UTF-8?B?SMOpbGxv?="), "{out}");
        assert!(out.contains("multipart/alternative") && out.contains("Message-ID: <") && out.contains("@acme.com>"));
        assert!(out.contains("Date: Fri, 15 Jan 2027 08:00:00 +0000"), "{out}");
        assert!(check(&Mail::new("bob@example.com", "Hi\r\nBcc: everyone@x.com", "x")).is_err(), "subject injection");
        assert!(check(&Mail::new("bob@example.com\r\nBcc: x@y.z", "Hi", "x")).is_err(), "recipient injection");
        assert!(check(&Mail::new("not-an-address", "Hi", "x")).is_err());
        assert_eq!(bare("Acme <no-reply@acme.com>"), "no-reply@acme.com");
    }
}

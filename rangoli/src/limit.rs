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
use std::net::{IpAddr, SocketAddr};
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
    let key = match (user, client_ip(&req, &crate::settings().trusted_proxies)) {
        (Some(id), _) => format!("user:{id}"),
        (None, Some(ip)) => format!("ip:{}", bucket(ip)),
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

/// An address or network: `10.0.0.0/8`, `fd00::/8`, or a bare address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Net(IpAddr, u8);

impl Net {
    pub fn parse(s: &str) -> Option<Net> {
        let (ip, bits) = s.trim().split_once('/').map_or((s.trim(), None), |(a, b)| (a, Some(b)));
        let ip: IpAddr = ip.parse().ok()?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let bits = bits.map_or(Some(max), |b| b.parse().ok().filter(|b| *b <= max))?;
        Some(Net(ip, bits))
    }

    fn contains(self, ip: IpAddr) -> bool {
        let (a, b) = match (self.0, ip.to_canonical()) {
            (IpAddr::V4(a), IpAddr::V4(b)) => (u128::from(u32::from(a)) << 96, u128::from(u32::from(b)) << 96),
            (IpAddr::V6(a), IpAddr::V6(b)) => (u128::from(a), u128::from(b)),
            _ => return false,
        };
        let mask = if self.1 == 0 { 0 } else { u128::MAX << (128 - u32::from(self.1)) };
        a & mask == b & mask
    }
}

/// Where a request came from. The socket's peer address, unless that peer is one of your
/// `RANGOLI_TRUSTED_PROXIES` (a load balancer, nginx): then the nearest address in
/// `X-Forwarded-For` that isn't a trusted proxy. Read right to left, because a client can put
/// anything at the left of that header; only what your proxies appended can be believed.
pub(crate) fn client_ip(req: &Request, trusted: &[Net]) -> Option<IpAddr> {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>()?.0.ip().to_canonical();
    if !trusted.iter().any(|n| n.contains(peer)) {
        return Some(peer);
    }
    let hops: Vec<IpAddr> = req
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|h| h.trim().parse::<IpAddr>().map(|ip| ip.to_canonical()))
        .collect::<Result<_, _>>()
        .unwrap_or_default(); // a garbled header proves nothing: fall back to the proxy itself
    Some(hops.into_iter().rev().find(|ip| !trusted.iter().any(|n| n.contains(*ip))).unwrap_or(peer))
}

/// IPv6 clients usually get a whole /64, so one person could rotate through billions of
/// addresses; they share one bucket.
fn bucket(ip: IpAddr) -> String {
    match ip {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        v4 => v4.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{bucket, client_ip, Net};
    use axum::extract::{ConnectInfo, Request};

    fn req(peer: &str, xff: &[&str]) -> Request {
        let mut r = Request::new(axum::body::Body::empty());
        r.extensions_mut().insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
        for v in xff {
            r.headers_mut().append("x-forwarded-for", v.parse().unwrap());
        }
        r
    }

    #[test]
    fn forwarded_for_is_believed_only_from_trusted_proxies() {
        let lb = [Net::parse("10.0.0.0/8").unwrap()];
        let ip = |r: &Request, t: &[Net]| client_ip(r, t).unwrap().to_string();
        assert_eq!(ip(&req("203.0.113.9:5000", &["1.2.3.4"]), &lb), "203.0.113.9", "a direct client can't spoof the header");
        assert_eq!(ip(&req("10.1.2.3:5000", &["198.51.100.7"]), &[]), "10.1.2.3", "no trusted proxies configured: header ignored");
        assert_eq!(ip(&req("10.1.2.3:5000", &["198.51.100.7"]), &lb), "198.51.100.7", "behind the load balancer");
        // The client wrote "6.6.6.6" itself; the proxy appended the real address.
        assert_eq!(ip(&req("10.1.2.3:5000", &["6.6.6.6, 198.51.100.7"]), &lb), "198.51.100.7");
        assert_eq!(ip(&req("10.1.2.3:5000", &["198.51.100.7", "10.9.9.9"]), &lb), "198.51.100.7", "two proxy hops, two headers");
        assert_eq!(ip(&req("10.1.2.3:5000", &["not-an-ip"]), &lb), "10.1.2.3", "garbage falls back to the proxy");
        assert_eq!(ip(&req("[::ffff:10.1.2.3]:5000", &["198.51.100.7"]), &lb), "198.51.100.7", "IPv4-mapped peers match v4 ranges");
    }

    #[test]
    fn ipv6_shares_a_bucket_per_64() {
        let a = bucket("2001:db8:1:2:aaaa::1".parse().unwrap());
        assert_eq!(a, bucket("2001:db8:1:2:ffff:ffff:ffff:ffff".parse().unwrap()));
        assert_ne!(a, bucket("2001:db8:1:3::1".parse().unwrap()));
        assert_eq!(bucket("192.0.2.1".parse().unwrap()), "192.0.2.1");
        assert_eq!(Net::parse("10.0.0.0/33"), None);
        assert!(Net::parse("::/0").unwrap().contains("2001:db8::1".parse().unwrap()));
    }

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

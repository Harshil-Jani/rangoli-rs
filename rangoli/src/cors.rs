//! CORS built in (Django needs the django-cors-headers package): `App::cors(["https://app.example.com"])`.
//! A listed origin may call the app from a browser, preflights included, and is trusted by the
//! cross-origin write protection too, since you named it. `"*"` lets any site read public data,
//! without cookies; it never exempts writes.

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub(crate) struct Cors {
    pub(crate) origins: Vec<String>,
    pub(crate) credentials: bool,
}

/// Set on requests from an origin listed by exact name, for the cross-origin write check.
#[derive(Clone, Copy)]
pub(crate) struct TrustedOrigin;

impl Cors {
    /// The `Access-Control-Allow-Origin` value for this request's origin, if it is allowed.
    fn allow<'a>(&'a self, origin: &'a str) -> Option<(&'a str, bool)> {
        if self.origins.iter().any(|o| o == origin) {
            Some((origin, true))
        } else if self.origins.iter().any(|o| o == "*") {
            Some(("*", false))
        } else {
            None
        }
    }

    fn decorate(&self, h: &mut HeaderMap, origin: &str, listed: bool) {
        if let Ok(v) = HeaderValue::from_str(origin) {
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
        h.append(header::VARY, HeaderValue::from_static("Origin"));
        if listed && self.credentials {
            h.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
        }
    }
}

pub(crate) async fn middleware(State(cors): State<Arc<Cors>>, mut req: Request, next: Next) -> Response {
    let origin = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let Some((allowed, listed)) = origin.as_deref().and_then(|o| cors.allow(o)).map(|(a, l)| (a.to_owned(), l)) else {
        return next.run(req).await;
    };
    // A preflight: answer it here, before routing.
    if req.method() == Method::OPTIONS && req.headers().contains_key(header::ACCESS_CONTROL_REQUEST_METHOD) {
        let mut res = StatusCode::NO_CONTENT.into_response();
        let h = res.headers_mut();
        cors.decorate(h, &allowed, listed);
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"));
        let asked = req.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS).cloned();
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, asked.unwrap_or(HeaderValue::from_static("authorization, content-type")));
        h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
        return res;
    }
    if listed {
        req.extensions_mut().insert(TrustedOrigin);
    }
    let mut res = next.run(req).await;
    cors.decorate(res.headers_mut(), &allowed, listed);
    res
}

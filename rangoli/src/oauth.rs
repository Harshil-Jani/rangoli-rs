//! Sign in to MCP with OAuth, the way claude.ai and Claude Desktop custom connectors (and
//! other MCP clients) connect: paste the server address, sign in, choose what the agent may
//! do. No token to copy. Follows the MCP authorization spec (2025-06-18):
//!
//! - `/.well-known/oauth-protected-resource` (RFC 9728) and
//!   `/.well-known/oauth-authorization-server` (RFC 8414) say where to sign in.
//! - `POST /oauth/register` registers a client (RFC 7591), with no secret.
//! - `/admin/oauth/authorize` is a consent page for signed-in staff (in the admin). Only the
//!   authorization code flow, only with PKCE S256.
//! - `POST /oauth/token` swaps a code for an ordinary API token, scoped as the user chose, which
//!   shows up under API tokens in the admin, where deleting it disconnects the client.
//!
//! Codes are stored hashed, live 60 seconds and are spent by a single `DELETE`, so two servers
//! racing to redeem one can't both win.

#![cfg_attr(not(feature = "admin"), allow(dead_code))]

use crate::auth::{TokenScope, User};
use crate::orm::Model;
use crate::{DateTime, Error, Result};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
use serde_json::json;

const CODE_SECS: i64 = 60;
/// Tokens from a sign-in last 90 days; the client then asks the person to sign in again.
const TOKEN_DAYS: u64 = 90;

/// A client that registered itself (`POST /oauth/register`).
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_oauth_client", display = "name")]
pub struct OAuthClient {
    pub id: Option<i64>,
    #[field(max_length = 64, unique)]
    pub client_id: String,
    #[field(max_length = 200)]
    pub name: String,
    /// One per line; a sign-in may only return to one of these.
    #[field(text)]
    pub redirect_uris: String,
    #[field(auto_now_add)]
    pub created_at: DateTime,
}

/// An authorization code waiting to be swapped for a token.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_oauth_code", display = "client_id")]
pub struct OAuthCode {
    pub id: Option<i64>,
    #[field(max_length = 64, unique)]
    pub code_hash: String,
    #[field(max_length = 64)]
    pub client_id: String,
    #[field(fk = crate::auth::User, cascade)]
    pub user_id: i64,
    #[field(text)]
    pub redirect_uri: String,
    #[field(max_length = 128)]
    pub challenge: String,
    pub read_only: bool,
    pub needs_approval: bool,
    pub expires_at: DateTime,
}

/// `https://host` as the client reached us (plain http in debug).
pub(crate) fn origin(headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let scheme = if crate::settings().debug { "http" } else { "https" };
    format!("{scheme}://{host}")
}

/// The `WWW-Authenticate` value that starts an MCP client's sign-in.
pub(crate) fn challenge(headers: &HeaderMap) -> String {
    format!(r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource""#, origin(headers))
}

pub(crate) async fn protected_resource(headers: HeaderMap) -> Json<serde_json::Value> {
    let o = origin(&headers);
    Json(json!({
        "resource": format!("{o}/mcp"),
        "authorization_servers": [o],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["read", "write"],
    }))
}

pub(crate) async fn authorization_server(headers: HeaderMap) -> Json<serde_json::Value> {
    let o = origin(&headers);
    Json(json!({
        "issuer": o,
        "authorization_endpoint": format!("{o}/admin/oauth/authorize"),
        "token_endpoint": format!("{o}/oauth/token"),
        "registration_endpoint": format!("{o}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["read", "write"],
    }))
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(json!({ "error": error, "error_description": description }))).into_response()
}

/// Where a sign-in may send the person back: https anywhere, http only to this machine
/// (OAuth 2.1, for desktop apps listening on a local port).
fn redirect_ok(uri: &str) -> bool {
    if uri.chars().any(char::is_control) {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once("://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host });
    !uri.contains('#')
        && !host.is_empty()
        && !host.contains('@')
        && match scheme {
            "https" => true,
            "http" => matches!(host, "localhost" | "127.0.0.1" | "[::1]"),
            _ => false,
        }
}

pub(crate) async fn register(body: axum::body::Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", "Expected a JSON object.");
    };
    let uris: Vec<String> =
        req["redirect_uris"].as_array().map(|a| a.iter().filter_map(|u| u.as_str().map(str::to_string)).collect()).unwrap_or_default();
    if uris.is_empty() || uris.len() > 10 || uris.iter().any(|u| u.len() > 2000 || !redirect_ok(u)) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris: 1 to 10 https addresses (http only for localhost), without fragments.",
        );
    }
    let name: String = req["client_name"].as_str().unwrap_or("MCP client").chars().filter(|c| !c.is_control()).take(200).collect();
    let name = if name.trim().is_empty() { "MCP client".to_string() } else { name };
    let created = async {
        let mut c = OAuthClient {
            id: None,
            client_id: crate::auth::random_hex(16)?,
            name,
            redirect_uris: uris.join("\n"),
            created_at: DateTime::now(),
        };
        c.save().await?;
        Ok::<_, Error>(c)
    };
    match created.await {
        Ok(c) => (
            StatusCode::CREATED,
            Json(json!({
                "client_id": c.client_id,
                "client_name": c.name,
                "redirect_uris": uris,
                "grant_types": ["authorization_code"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// A checked authorization request, ready for the consent page.
pub(crate) struct AuthRequest {
    pub client: OAuthClient,
    pub redirect_uri: String,
    pub challenge: String,
    pub state: Option<String>,
}

/// Why an authorization request was refused. `Show` errors go on the page: the redirect target
/// can't be trusted yet. `Redirect` errors go back to the (verified) client.
pub(crate) enum AuthError {
    Show(&'static str),
    Redirect(String),
}

pub(crate) async fn check_request(p: &[(String, String)]) -> Result<std::result::Result<AuthRequest, AuthError>> {
    let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let client_id = get("client_id").unwrap_or("");
    let unknown = || Ok(Err(AuthError::Show("This app isn't registered here. Start the connection again from the app.")));
    if !hex(client_id, 32) {
        return unknown(); // never a valid id, so don't ask the database (Postgres rejects NUL bytes in text)
    }
    let Some(client) = OAuthClient::objects().filter(OAuthClient::CLIENT_ID.eq(client_id)).first().await? else {
        return unknown();
    };
    let redirect_uri = match get("redirect_uri") {
        Some(u) if client.redirect_uris.lines().any(|r| r == u) => u.to_string(),
        None if client.redirect_uris.lines().count() == 1 => client.redirect_uris.clone(),
        _ => return Ok(Err(AuthError::Show("The app asked to return to an address it didn't register."))),
    };
    let state = get("state").map(str::to_string);
    let back = |error: &str, description: &str| {
        Ok(Err(AuthError::Redirect(with_query(&redirect_uri, &[("error", error), ("error_description", description)], state.as_deref()))))
    };
    if get("response_type") != Some("code") {
        return back("unsupported_response_type", "Only response_type=code is supported.");
    }
    let challenge = get("code_challenge").unwrap_or("");
    if get("code_challenge_method") != Some("S256") || !verifier_ok(challenge) {
        return back("invalid_request", "PKCE with code_challenge_method=S256 is required.");
    }
    Ok(Ok(AuthRequest { client, challenge: challenge.into(), redirect_uri, state }))
}

/// `uri` with `pairs` (and `state`, when given) added to its query string.
pub(crate) fn with_query(uri: &str, pairs: &[(&str, &str)], state: Option<&str>) -> String {
    let mut all: Vec<(&str, &str)> = pairs.to_vec();
    if let Some(s) = state {
        all.push(("state", s));
    }
    let q = serde_urlencoded::to_string(&all).unwrap_or_default();
    format!("{uri}{}{q}", if uri.contains('?') { '&' } else { '?' })
}

/// The person said yes: store a code for `user` and return where to send them.
pub(crate) async fn grant(req: &AuthRequest, user: &User, read_only: bool, needs_approval: bool) -> Result<String> {
    let now = DateTime::now();
    OAuthCode::objects().filter(OAuthCode::EXPIRES_AT.lt(now)).delete().await?; // tidy up unredeemed ones
    let code = crate::auth::random_hex(32)?;
    OAuthCode {
        id: None,
        code_hash: digest(&code),
        client_id: req.client.client_id.clone(),
        user_id: user.id.ok_or(Error::NotFound)?,
        redirect_uri: req.redirect_uri.clone(),
        challenge: req.challenge.clone(),
        read_only,
        needs_approval: needs_approval && !read_only,
        expires_at: DateTime::from_unix(now.unix() + CODE_SECS),
    }
    .save()
    .await?;
    Ok(with_query(&req.redirect_uri, &[("code", &code)], req.state.as_deref()))
}

fn digest(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// PKCE S256: BASE64URL(SHA256(verifier)) without padding.
fn s256(verifier: &str) -> String {
    use sha2::Digest;
    crate::django::base64(&sha2::Sha256::digest(verifier.as_bytes())).trim_end_matches('=').replace('+', "-").replace('/', "_")
}

pub(crate) async fn token(Form(p): Form<Vec<(String, String)>>) -> Response {
    let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or("");
    if get("grant_type") != "authorization_code" {
        return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "Only authorization_code is supported.");
    }
    match redeem(get("code"), get("client_id"), get("redirect_uri"), get("code_verifier")).await {
        Ok(Some((token, scope))) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "access_token": token, "token_type": "Bearer", "expires_in": TOKEN_DAYS * 86_400, "scope": scope })),
        )
            .into_response(),
        Ok(None) => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "The code is unknown, used, expired or doesn't match."),
        Err(e) => e.into_response(),
    }
}

/// `n` lowercase hex digits: the shape of the ids and codes this server issues.
fn hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// RFC 7636: 43 to 128 of A-Z a-z 0-9 - . _ ~
fn verifier_ok(v: &str) -> bool {
    (43..=128).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

async fn redeem(code: &str, client_id: &str, redirect_uri: &str, verifier: &str) -> Result<Option<(String, &'static str)>> {
    if !hex(code, 64) || !hex(client_id, 32) || !verifier_ok(verifier) || redirect_uri.chars().any(char::is_control) {
        return Ok(None); // malformed: nothing to look up
    }
    let Some(row) = OAuthCode::objects().filter(OAuthCode::CODE_HASH.eq(digest(code))).first().await? else { return Ok(None) };
    // Spend it first: of two racing redemptions, only one deletes the row.
    if OAuthCode::objects().filter(OAuthCode::ID.eq(row.id.unwrap_or_default())).delete().await? != 1 {
        return Ok(None);
    }
    let fresh = row.expires_at.unix() >= DateTime::now().unix();
    let matches = row.client_id == client_id && (redirect_uri.is_empty() || row.redirect_uri == redirect_uri);
    if !fresh || !matches || s256(verifier) != row.challenge {
        return Ok(None);
    }
    let Some(client) = OAuthClient::objects().filter(OAuthClient::CLIENT_ID.eq(client_id)).first().await? else { return Ok(None) };
    let user = User::get(row.user_id).await?;
    let scope = TokenScope { read_only: row.read_only, needs_approval: row.needs_approval, ..Default::default() };
    let ttl = std::time::Duration::from_secs(TOKEN_DAYS * 86_400);
    let (_, token) = crate::auth::create_scoped_token(&user, &format!("{} (signed in)", client.name), Some(ttl), &scope).await?;
    Ok(Some((token, if row.read_only { "read" } else { "read write" })))
}

#[cfg(test)]
mod tests {
    #[test]
    fn pkce_s256_matches_rfc_7636() {
        // RFC 7636 appendix B.
        assert_eq!(super::s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn redirect_targets() {
        use super::redirect_ok;
        assert!(redirect_ok("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_ok("http://localhost:6274/oauth/callback") && redirect_ok("http://127.0.0.1:33418/"));
        assert!(!redirect_ok("http://evil.example/cb"), "plain http only to this machine");
        assert!(!redirect_ok("http://localhost.evil.example/cb"));
        assert!(!redirect_ok("https://claude.ai/cb#frag") && !redirect_ok("javascript:alert(1)") && !redirect_ok("https://user@host/"));
    }
}

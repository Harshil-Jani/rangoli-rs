//! The admin's pages, modeled on Django's: generated from model metadata, server-rendered,
//! HTMX-enhanced. Works without JavaScript; with it, `hx-boost` turns navigation
//! into partial swaps and search filters live.

// Handlers short-circuit with ready-made responses; boxing them buys nothing here.
#![allow(clippy::result_large_err)]

use crate::auth::{self, CurrentUser, User};
use crate::orm::{self, by_id, like_escape, FieldMeta, FieldType, Model, ModelMeta, Node, Query, Value};
use crate::{DateTime, Error};
use axum::extract::{Form, Path, Query as UrlQuery, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use minijinja::{context, Environment};
use std::collections::HashMap;
use std::sync::Arc;

use super::{LogEntry, Options};

const ADDITION: i64 = 1;
const CHANGE: i64 = 2;
const DELETION: i64 = 3;
use crate::forms::validate;

const PAGE_SIZE: u64 = 100;

struct Site {
    admin: Vec<&'static ModelMeta>,
    options: HashMap<&'static str, Options>,
    models: Vec<&'static ModelMeta>,
    env: Environment<'static>,
    version: String,
}

type S = State<Arc<Site>>;
type Pairs = Vec<(String, String)>;
type Page = Result<Response, Response>;

pub fn router(admin: Vec<(&'static ModelMeta, Options)>, models: Vec<&'static ModelMeta>) -> Router {
    let options = admin.iter().map(|(m, o)| (m.table, o.clone())).collect();
    let admin: Vec<_> = admin.into_iter().map(|(m, _)| m).collect();
    let mut env = Environment::new();
    for (name, src) in [
        ("base.html", include_str!("templates/base.html")),
        ("login.html", include_str!("templates/login.html")),
        ("logged_out.html", include_str!("templates/logged_out.html")),
        ("index.html", include_str!("templates/index.html")),
        ("list.html", include_str!("templates/list.html")),
        ("form.html", include_str!("templates/form.html")),
        ("delete.html", include_str!("templates/delete.html")),
        ("history.html", include_str!("templates/history.html")),
        ("password_change.html", include_str!("templates/password_change.html")),
        ("agents.html", include_str!("templates/agents.html")),
        ("two_factor.html", include_str!("templates/two_factor.html")),
        ("oauth.html", include_str!("templates/oauth.html")),
        ("import.html", include_str!("templates/import.html")),
        ("requests.html", include_str!("templates/requests.html")),
        ("password_reset.html", include_str!("templates/password_reset.html")),
    ] {
        env.add_template(name, src).expect("admin template");
    }
    let version = asset_version();
    env.add_global("v", version.clone());
    Router::new()
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(index))
        .route("/admin/login", get(login_page).post(login_submit))
        .route("/admin/logout", post(logout))
        .route("/admin/password_change/", get(password_page).post(password_submit))
        .route("/admin/agents/", get(agents_page).post(agents_submit))
        .route("/admin/2fa/", get(two_factor_page).post(two_factor_submit))
        .route("/admin/oauth/authorize", get(oauth_page).post(oauth_submit))
        .route("/admin/requests/", get(requests_page))
        .route("/admin/password_reset/", get(reset_request_page).post(reset_request_submit))
        .route("/admin/reset/{token}", get(reset_page).post(reset_submit))
        .route("/admin/static/{file}", get(static_file))
        .route("/admin/{table}/", get(list).post(bulk))
        .route("/admin/{table}/export.csv", get(export_csv))
        .route("/admin/{table}/autocomplete", get(autocomplete))
        .route("/admin/{table}/import", get(import_page).post(import_submit))
        .route("/admin/{table}/add", get(add_page).post(add_submit))
        .route("/admin/{table}/{id}/", get(change_page).post(change_submit))
        .route("/admin/{table}/{id}/delete", get(delete_page).post(delete_submit))
        .route("/admin/{table}/{id}/history", get(history))
        .route("/admin/{table}/{id}/undo/{entry}", post(undo))
        .layer(axum::middleware::from_fn(load_permissions))
        .with_state(Arc::new(Site { admin, options, models, env, version }))
}

// ---------------------------------------------------------------- two-factor sign-in

async fn two_factor_page(State(site): S, user: CurrentUser, uri: Uri) -> Page {
    let u = staff(&user, &uri)?;
    let enabled = u.totp_secret.as_deref().is_some_and(|s| !s.is_empty());
    let secret = crate::totp::new_secret();
    let uri = crate::totp::uri("Rangoli", &u.username, &secret);
    render(&site, "two_factor.html", context! { title => "Two-factor sign-in", enabled, secret, uri, ..chrome(&site, u, None) })
}

/// Turn it on (with the new key and a code from it) or off (with a current code).
async fn two_factor_submit(State(site): S, user: CurrentUser, uri: Uri, Form(f): Form<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let code = param(&f, "code").unwrap_or("");
    let now = auth::now();
    let (secret, turning_on) = match param(&f, "action") {
        Some("enable") => (param(&f, "secret").unwrap_or("").to_string(), true),
        _ => (u.totp_secret.clone().unwrap_or_default(), false),
    };
    let step =
        (!secret.is_empty()).then(|| crate::totp::verify(&secret, code, now, if turning_on { 0 } else { u.totp_last_step })).flatten();
    let Some(step) = step else {
        let enabled = !turning_on;
        let fresh = if turning_on { secret.clone() } else { crate::totp::new_secret() };
        let uri = crate::totp::uri("Rangoli", &u.username, &fresh);
        let error = "That code doesn't match. Check the time on your phone and try the newest code.";
        return render(
            &site,
            "two_factor.html",
            context! { title => "Two-factor sign-in", enabled, secret => fresh, uri, error, ..chrome(&site, u, None) },
        );
    };
    let mut me = u.clone();
    me.totp_secret = turning_on.then_some(secret);
    me.totp_last_step = step;
    me.save().await.map_err(fail)?;
    let done = if turning_on { "Two-factor sign-in is on. Next time, you'll be asked for a code." } else { "Two-factor sign-in is off." };
    let secret = crate::totp::new_secret();
    let uri = crate::totp::uri("Rangoli", &u.username, &secret);
    render(
        &site,
        "two_factor.html",
        context! { title => "Two-factor sign-in", enabled => turning_on, done, secret, uri, ..chrome(&site, u, None) },
    )
}

// ---------------------------------------------------------------- AI agents (MCP)

/// The MCP address as the browser reached this admin: `https://host/mcp` (plain http in debug).
fn mcp_url(headers: &axum::http::HeaderMap) -> String {
    let host = headers.get(axum::http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let scheme = if crate::settings().debug { "http" } else { "https" };
    format!("{scheme}://{host}/mcp")
}

/// A short MCP server name for config snippets: `blog` for `blog.example.com`, `rangoli`
/// for localhost or a bare IP address.
fn server_name(url: &str) -> String {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':']).next().unwrap_or("");
    let first = host.split('.').next().unwrap_or("");
    let ok = !first.is_empty()
        && first != "localhost"
        && first.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !first.chars().all(|c| c.is_ascii_digit());
    if ok {
        first.to_ascii_lowercase()
    } else {
        "rangoli".into()
    }
}

async fn agent_rows(u: &User) -> Result<Vec<minijinja::Value>, Response> {
    let tokens = auth::ApiToken::objects()
        .filter(auth::ApiToken::USER_ID.eq(u.id.unwrap_or(-1)))
        .order_by(auth::ApiToken::ID.desc())
        .all()
        .await
        .map_err(fail)?;
    Ok(tokens
        .iter()
        .map(|t| {
            context! {
                id => t.id, name => t.name, prefix => t.prefix,
                access => if t.read_only { "Read only" } else { "Read and change" },
                tables => t.tables.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "All".to_string()),
                used => t.last_used_at.map_or("Never".to_string(), DateTime::human),
                expires => t.expires_at.map_or("Never".to_string(), DateTime::human),
            }
        })
        .collect())
}

/// The app's own tables an agent can be limited to (framework tables are never offered).
fn agent_tables(site: &Site) -> Vec<(&'static str, String)> {
    site.models.iter().filter(|m| !m.table.starts_with("rangoli_")).map(|m| (m.table, title_plural(m.name))).collect()
}

fn table_choices(site: &Site) -> Vec<minijinja::Value> {
    agent_tables(site).into_iter().map(|(table, label)| context! { table, label }).collect()
}

async fn agents_page(State(site): S, user: CurrentUser, uri: Uri) -> Page {
    let u = staff(&user, &uri)?;
    let tokens = agent_rows(u).await?;
    let choices = table_choices(&site);
    render(&site, "agents.html", context! { title => "Connect an AI agent", tokens, choices, ..chrome(&site, u, None) })
}

/// Create a token for the signed-in user and show it once, with setup for common clients.
async fn agents_submit(State(site): S, user: CurrentUser, uri: Uri, headers: axum::http::HeaderMap, Form(f): Form<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let name = param(&f, "name").unwrap_or("").trim().to_string();
    let choices = table_choices(&site);
    if name.is_empty() || name.chars().count() > 100 || name.contains('\0') {
        let tokens = agent_rows(u).await?;
        let error = "Give the agent a name of up to 100 characters.";
        return render(&site, "agents.html", context! { title => "Connect an AI agent", error, tokens, choices, ..chrome(&site, u, None) });
    }
    let days: u64 = param(&f, "days").and_then(|d| d.parse().ok()).filter(|d| *d <= 3650).unwrap_or(30);
    let ttl = (days > 0).then(|| std::time::Duration::from_secs(days * 86_400));
    // Safe by default: read-only unless "Allow changes" is ticked; only known tables are accepted.
    let known = agent_tables(&site);
    let picked: Vec<String> =
        f.iter().filter(|(k, _)| k == "tables").filter(|(_, v)| known.iter().any(|(t, _)| t == v)).map(|(_, v)| v.clone()).collect();
    let writes = param(&f, "allow_writes") == Some("on");
    let scope = auth::TokenScope {
        read_only: !writes,
        tables: (!picked.is_empty()).then_some(picked),
        needs_approval: writes && param(&f, "ask_first") == Some("on"),
        max_writes_per_day: param(&f, "max_writes").and_then(|n| n.trim().parse::<i64>().ok()).filter(|n| *n > 0),
        ..Default::default()
    };
    let (_, token) = auth::create_scoped_token(u, &name, ttl, &scope).await.map_err(fail)?;
    let url = mcp_url(&headers);
    let tokens = agent_rows(u).await?;
    let can = match (&scope.read_only, &scope.tables) {
        (true, None) => "It can read everything you can, and change nothing.".to_string(),
        (false, None) if scope.needs_approval => {
            "It can read everything you can; each change it asks for waits for your approval under Pending changes.".to_string()
        }
        (false, None) => "It can read and change everything you can.".to_string(),
        (ro, Some(ts)) => format!("It can {} only these tables: {}.", if *ro { "read" } else { "read and change" }, ts.join(", ")),
    };
    render(
        &site,
        "agents.html",
        context! { title => "Connect an AI agent", name, token, can, server => server_name(&url), url, tokens, choices, ..chrome(&site, u, None) },
    )
}

// ---------------------------------------------------------------- MCP sign-in (OAuth consent)

/// Check the request; on a problem, the page to show or the error redirect to send.
async fn oauth_request(site: &Site, u: &User, p: &[(String, String)]) -> Result<crate::oauth::AuthRequest, Response> {
    match crate::oauth::check_request(p).await.map_err(fail)? {
        Ok(req) => Ok(req),
        Err(crate::oauth::AuthError::Redirect(to)) => Err(Redirect::to(&to).into_response()),
        Err(crate::oauth::AuthError::Show(error)) => {
            Err(render(site, "oauth.html", context! { title => "Connect an app", error, ..chrome(site, u, None) }).unwrap_or_else(|e| e))
        }
    }
}

async fn oauth_page(State(site): S, user: CurrentUser, uri: Uri, UrlQuery(p): UrlQuery<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let req = match oauth_request(&site, u, &p).await {
        Ok(r) => r,
        Err(page) => return Ok(page),
    };
    let host = req.redirect_uri.split("://").nth(1).unwrap_or("").split(['/', '?']).next().unwrap_or("").to_string();
    let title = format!("Connect {}", req.client.name);
    let mut page = render(&site, "oauth.html", context! { title, client => req.client.name, host, ..chrome(&site, u, None) })?;
    // Browsers apply form-action to the redirect after submitting, so allow the app's address too.
    let scheme = req.redirect_uri.split("://").next().unwrap_or("https");
    let csp = format!("default-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self' {scheme}://{host}");
    if let Ok(v) = axum::http::HeaderValue::from_str(&csp) {
        page.headers_mut().insert(axum::http::header::CONTENT_SECURITY_POLICY, v);
    }
    Ok(page)
}

async fn oauth_submit(State(site): S, user: CurrentUser, uri: Uri, UrlQuery(p): UrlQuery<Pairs>, Form(f): Form<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let req = match oauth_request(&site, u, &p).await {
        Ok(r) => r,
        Err(page) => return Ok(page),
    };
    if param(&f, "decision") != Some("allow") {
        let to = crate::oauth::with_query(&req.redirect_uri, &[("error", "access_denied")], req.state.as_deref());
        return Ok(Redirect::to(&to).into_response());
    }
    let (read_only, ask) = match param(&f, "access") {
        Some("write") => (false, false),
        Some("ask") => (false, true),
        _ => (true, false), // anything unexpected gets the safest choice
    };
    let to = crate::oauth::grant(&req, u, read_only, ask).await.map_err(fail)?;
    Ok(Redirect::to(&to).into_response())
}

// ---------------------------------------------------------------- helpers

fn fail(e: Error) -> Response {
    e.into_response()
}

fn render(site: &Site, name: &str, ctx: minijinja::Value) -> Page {
    let html = site.env.get_template(name).and_then(|t| t.render(ctx)).map_err(|e| {
        eprintln!("rangoli admin: template {name}: {e:#}");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;
    Ok(Html(html).into_response())
}

fn staff<'a>(user: &'a CurrentUser, uri: &Uri) -> Result<&'a User, Response> {
    match &user.0 {
        Some(u) if u.is_staff => Ok(u),
        _ => {
            let next = serde_urlencoded::to_string([("next", uri.path_and_query().map_or("/admin/", |p| p.as_str()))]).unwrap();
            Err(Redirect::to(&format!("/admin/login?{next}")).into_response())
        }
    }
}

fn model(site: &Site, table: &str) -> Result<&'static ModelMeta, Response> {
    let meta = site.admin.iter().copied().find(|m| m.table == table).ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    allowed(meta.table, "view")?;
    Ok(meta)
}

tokio::task_local! {
    /// The signed-in staff member's permissions, loaded once per admin request.
    static PERMS: auth::Permissions;
}

/// Load the staff member's permissions for this request (one query, only when enforced).
async fn load_permissions(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let user = req.extensions().get::<CurrentUser>().and_then(|u| u.0.clone());
    match user.filter(|u| u.is_staff) {
        Some(u) => match auth::permissions(&u).await {
            Ok(p) => PERMS.scope(p, next.run(req)).await,
            Err(e) => fail(e),
        },
        None => next.run(req).await,
    }
}

/// May the current staff member do `action` (view, add, change, delete) on `table`?
fn can(table: &str, action: &str) -> bool {
    PERMS.try_with(|p| p.allows(table, action)).unwrap_or(true)
}

fn allowed(table: &str, action: &str) -> Result<(), Response> {
    if can(table, action) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, Html("<h1>403 Forbidden</h1><p>Your groups don't allow this. Ask an administrator.</p>"))
            .into_response())
    }
}

/// A model whose admin allows adding; `no_add()` models answer 403 like Django.
fn addable(site: &Site, table: &str) -> Result<&'static ModelMeta, Response> {
    let meta = model(site, table)?;
    if options(site, meta).no_add {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    allowed(meta.table, "add")?;
    Ok(meta)
}

fn options<'a>(site: &'a Site, meta: &ModelMeta) -> &'a Options {
    static DEFAULT: std::sync::OnceLock<Options> = std::sync::OnceLock::new();
    site.options.get(meta.table).unwrap_or_else(|| DEFAULT.get_or_init(Options::default))
}

/// `TaskRecord` -> `task record`, Django's default `verbose_name`.
fn verbose(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    // Acronyms stay acronyms: `ApiToken` is an "API token", not an "api token".
    out.split(' ').map(|w| if ACRONYMS.contains(&w) { w.to_uppercase() } else { w.to_string() }).collect::<Vec<_>>().join(" ")
}

const ACRONYMS: [&str; 8] = ["api", "mcp", "url", "uuid", "ip", "sms", "http", "ai"];

/// `TaskRecord` -> `task records`.
fn verbose_plural(name: &str) -> String {
    plural(&verbose(name))
}

/// `TaskRecord` -> `Task records`, for headings and the sidebar.
fn title_plural(name: &str) -> String {
    capitalize(&verbose_plural(name))
}

fn plural(name: &str) -> String {
    if name.ends_with(|c: char| c.is_ascii_uppercase()) {
        return format!("{name}s"); // "URLs", "APIs"
    }
    match name.strip_suffix('y') {
        Some(stem) if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) => format!("{stem}ies"),
        _ if name.ends_with('s') => format!("{name}es"),
        _ => format!("{name}s"),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn label(field: &str) -> String {
    capitalize(&field.strip_suffix("_id").unwrap_or(field).replace('_', " "))
}

/// Django groups models by app; here the app is the table prefix (`blog_post` is in `Blog`).
fn app_of(table: &str) -> String {
    if table.starts_with("rangoli_") {
        return "Authentication and Authorization".into();
    }
    capitalize(table.split_once('_').map_or(table, |(app, _)| app))
}

/// Apps and their models, in registration order, for the index and the sidebar.
fn apps(site: &Site, current: Option<&str>) -> Vec<minijinja::Value> {
    let mut groups: Vec<(String, Vec<minijinja::Value>)> = vec![];
    for m in site.admin.iter().filter(|m| can(m.table, "view")) {
        let app = app_of(m.table);
        let entry = context! {
            table => m.table, plural => title_plural(m.name), current => current == Some(m.table),
            can_add => !options(site, m).no_add && can(m.table, "add"),
        };
        match groups.iter_mut().find(|(a, _)| *a == app) {
            Some((_, models)) => models.push(entry),
            None => groups.push((app, vec![entry])),
        }
    }
    groups.into_iter().map(|(name, models)| context! { name, models }).collect()
}

/// Context every logged-in page shares: header, sidebar, current model.
fn chrome(site: &Site, u: &User, current: Option<&'static ModelMeta>) -> minijinja::Value {
    context! {
        username => u.username,
        inspecting => crate::inspect::enabled(),
        apps => apps(site, current.map(|m| m.table)),
        app => current.map(|m| app_of(m.table)),
        table => current.map(|m| m.table),
        name => current.map(|m| verbose(m.name)),
        plural => current.map(|m| title_plural(m.name)),
        can_add => current.is_some_and(|m| !options(site, m).no_add && can(m.table, "add")),
    }
}

fn display(v: &Value, f: &FieldMeta) -> String {
    match v {
        Value::Null => "-".into(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).into(),
        Value::Int(i) if f.ty == FieldType::DateTime => DateTime::from_unix(*i).human(),
        Value::Int(i) if f.ty == FieldType::Date => crate::Date::from_days(*i).to_string(),
        Value::Int(i) => match f.ty {
            FieldType::Decimal(p) => crate::decimal::format_units(*i, p),
            _ => i.to_string(),
        },
        Value::Float(x) => x.to_string(),
        Value::Text(s) if f.choices.is_some() => {
            f.choices.into_iter().flatten().find(|(v, _)| v == s).map_or(s.clone(), |(_, l)| l.to_string())
        }
        Value::Text(s) if matches!(f.ty, FieldType::Vector(_)) => format!("{} numbers", s.matches(',').count() + 1),
        Value::Text(s) if matches!(f.ty, FieldType::Text | FieldType::Json) && s.chars().count() > 80 => {
            s.chars().take(80).chain("…".chars()).collect()
        }
        Value::Text(s) => s.clone(),
    }
}

fn param<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Row label from already-fetched values: the `display` field, else `Name object (id)` like Django.
fn repr(meta: &ModelMeta, id: i64, vals: &[Value]) -> String {
    meta.display
        .and_then(|d| meta.fields.iter().position(|f| f.name == d))
        .map(|i| display(&vals[i], &meta.fields[i]))
        .unwrap_or_else(|| format!("{} object ({id})", meta.name))
}

async fn row_values(meta: &'static ModelMeta, id: i64) -> crate::Result<Vec<Value>> {
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    Ok(q.rows().await?.pop().ok_or(Error::NotFound)?.1)
}

async fn fetch_row(meta: &'static ModelMeta, id: i64) -> Result<Vec<Value>, Response> {
    row_values(meta, id).await.map_err(fail)
}

async fn log(u: &User, meta: &ModelMeta, object_id: i64, object_repr: &str, action: i64, message: String) -> crate::Result<i64> {
    let mut e = LogEntry {
        id: None,
        user_id: u.id.unwrap_or_default(),
        table_name: meta.table.into(),
        object_id,
        object_repr: object_repr.chars().take(200).collect(),
        action,
        message,
        at: auth::now(),
        snapshot: None,
    };
    e.save().await?;
    crate::webhooks::fire(&e).await?;
    Ok(e.id.unwrap())
}

/// The success message for `?log=<id>`, worded for the page it lands on.
async fn message(p: &[(String, String)], u: &User, landing: &str) -> Option<minijinja::Value> {
    if let Some(n) = param(p, "deleted").and_then(|n| n.parse::<u64>().ok()) {
        let what = param(p, "what").unwrap_or("items");
        return Some(context! { level => "success", text => format!("Successfully deleted {n} {what}.") });
    }
    match param(p, "warn") {
        Some("noitems") => {
            return Some(
                context! { level => "warning", text => "Items must be selected in order to perform actions on them. No items have been changed." },
            )
        }
        Some("noaction") => return Some(context! { level => "warning", text => "No action selected." }),
        _ => {}
    }
    let e = LogEntry::get(param(p, "log")?.parse().ok()?).await.ok().filter(|e| Some(e.user_id) == u.id)?;
    let name = e.table_name.split_once('_').map_or(e.table_name.as_str(), |(_, m)| m).replace('_', " ");
    let (verb, url) = match e.action {
        ADDITION => ("added", Some(format!("/admin/{}/{}/", e.table_name, e.object_id))),
        CHANGE => ("changed", Some(format!("/admin/{}/{}/", e.table_name, e.object_id))),
        _ => ("deleted", None),
    };
    let tail = match landing {
        "change" => " You may edit it again below.".to_string(),
        "add" => format!(" You may add another {name} below."),
        _ => String::new(),
    };
    Some(context! {
        level => "success", prefix => format!("The {name} “"), repr => e.object_repr, url,
        suffix => format!("” was {verb} successfully.{tail}"),
    })
}

// ---------------------------------------------------------------- auth pages

async fn login_page(State(site): S, user: CurrentUser, UrlQuery(p): UrlQuery<Pairs>) -> Response {
    let next = safe_next(param(&p, "next"));
    if user.0.is_some_and(|u| u.is_staff) {
        return Redirect::to(next).into_response();
    }
    let reset = param(&p, "reset").is_some();
    render(&site, "login.html", context! { title => "Log in", next, reset }).unwrap_or_else(|e| e)
}

/// Only redirect back into the admin, never to another site.
fn safe_next(next: Option<&str>) -> &str {
    next.filter(|n| n.starts_with("/admin/") && !n.starts_with("//")).unwrap_or("/admin/")
}

async fn login_submit(State(site): S, Form(f): Form<Pairs>) -> Response {
    let (username, password) = (param(&f, "username").unwrap_or(""), param(&f, "password").unwrap_or(""));
    let next = safe_next(param(&f, "next"));
    let code = param(&f, "code").filter(|c| !c.trim().is_empty());
    let mut ask_code = code.is_some();
    let error = match auth::authenticate_with_code(username, password, code).await {
        Ok(Some(u)) if u.is_staff => match auth::login(&u).await {
            Ok(cookie) => return ([(header::SET_COOKIE, cookie)], Redirect::to(next)).into_response(),
            Err(e) => return fail(e),
        },
        Ok(_) => {
            "Please enter the correct username and password for a staff account. Note that both fields may be case-sensitive.".to_string()
        }
        Err(Error::Locked) => Error::Locked.to_string(),
        Err(Error::CodeRequired) => {
            ask_code = true;
            "Enter the 6-digit code from your authenticator app.".to_string()
        }
        Err(e) => return fail(e),
    };
    let page = render(&site, "login.html", context! { title => "Log in", next, error, username, ask_code });
    (StatusCode::UNAUTHORIZED, page.unwrap_or_else(|e| e)).into_response()
}

async fn logout(State(site): S, headers: HeaderMap) -> Response {
    let cookie = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    match auth::logout(cookie).await {
        Ok(clear) => {
            ([(header::SET_COOKIE, clear)], render(&site, "logged_out.html", context! { title => "Logged out" }).unwrap_or_else(|e| e))
                .into_response()
        }
        Err(e) => fail(e),
    }
}

async fn password_page(State(site): S, user: CurrentUser, uri: Uri) -> Page {
    let u = staff(&user, &uri)?;
    render(&site, "password_change.html", context! { title => "Password change", ..chrome(&site, u, None) })
}

async fn password_submit(State(site): S, user: CurrentUser, uri: Uri, Form(f): Form<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let (old, new1, new2) =
        (param(&f, "old_password").unwrap_or(""), param(&f, "new_password1").unwrap_or(""), param(&f, "new_password2").unwrap_or(""));
    let mut errors: HashMap<&str, &str> = HashMap::new();
    if !auth::verify_password(old, &u.password).await {
        errors.insert("old_password", "Your old password was entered incorrectly. Please enter it again.");
    }
    if new1.chars().count() < 8 {
        errors.insert("new_password1", "This password is too short. It must contain at least 8 characters.");
    } else if new1 != new2 {
        errors.insert("new_password2", "The two password fields didn’t match.");
    }
    if !errors.is_empty() {
        let page = render(&site, "password_change.html", context! { title => "Password change", errors, ..chrome(&site, u, None) })?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    let mut changed = u.clone();
    changed.password = auth::hash_password(new1).await.map_err(fail)?;
    changed.save().await.map_err(fail)?;
    render(&site, "password_change.html", context! { title => "Password change successful", done => true, ..chrome(&site, u, None) })
}

/// Admin assets, compiled into the binary.
const ASSETS: &[(&str, &[u8], &str)] = &[
    ("admin.css", include_bytes!("static/admin.css"), "text/css; charset=utf-8"),
    ("admin.js", include_bytes!("static/admin.js"), "text/javascript; charset=utf-8"),
    ("htmx.min.js", include_bytes!("static/htmx.min.js"), "text/javascript; charset=utf-8"),
    ("icon-yes.svg", include_bytes!("static/icon-yes.svg"), "image/svg+xml"),
    ("icon-no.svg", include_bytes!("static/icon-no.svg"), "image/svg+xml"),
    ("icon-addlink.svg", include_bytes!("static/icon-addlink.svg"), "image/svg+xml"),
    ("icon-changelink.svg", include_bytes!("static/icon-changelink.svg"), "image/svg+xml"),
    ("icon-deletelink.svg", include_bytes!("static/icon-deletelink.svg"), "image/svg+xml"),
    ("search.svg", include_bytes!("static/search.svg"), "image/svg+xml"),
];

/// Content hash of every asset. Pages link `?v=<hash>`, so a new build can never be
/// shown with a stylesheet the browser cached from an old one.
fn asset_version() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ASSETS.iter().for_each(|(_, body, _)| body.hash(&mut h));
    format!("{:016x}", h.finish())
}

async fn static_file(State(site): S, Path(file): Path<String>, UrlQuery(p): UrlQuery<Pairs>) -> Response {
    let Some((_, body, ty)) = ASSETS.iter().find(|(name, ..)| *name == file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Versioned URLs never change; anything else must be revalidated.
    let cache = if param(&p, "v") == Some(site.version.as_str()) { "public, max-age=31536000, immutable" } else { "no-cache" };
    ([(header::CONTENT_TYPE, *ty), (header::CACHE_CONTROL, cache)], *body).into_response()
}

// ---------------------------------------------------------------- index

async fn index(State(site): S, user: CurrentUser, uri: Uri) -> Page {
    let u = staff(&user, &uri)?;
    let entries = LogEntry::objects()
        .filter(LogEntry::USER_ID.eq(u.id.unwrap_or_default()))
        .order_by(LogEntry::ID.desc())
        .limit(10)
        .all()
        .await
        .map_err(fail)?;
    let recent: Vec<_> = entries
        .iter()
        .map(|e| {
            let meta = site.models.iter().find(|m| m.table == e.table_name);
            let linkable = e.action != DELETION && site.admin.iter().any(|m| m.table == e.table_name);
            context! {
                class => match e.action { ADDITION => "addlink", CHANGE => "changelink", _ => "deletelink" },
                repr => e.object_repr,
                url => linkable.then(|| format!("/admin/{}/{}/", e.table_name, e.object_id)),
                model => meta.map_or(e.table_name.clone(), |m| capitalize(&verbose(m.name))),
            }
        })
        .collect();
    let attention = attention().await.map_err(fail)?;
    render(&site, "index.html", context! { title => "Site administration", recent, attention, ..chrome(&site, u, None) })
}

/// What a person should look at: waiting approvals, failed jobs and webhooks, active agents.
/// Only shown to staff who can view the model it links to.
async fn attention() -> crate::Result<Vec<minijinja::Value>> {
    use crate::approvals::{ApprovalStatus, PendingChange};
    use crate::tasks::{TaskRecord, TaskStatus};
    let day_ago = DateTime::from_unix(DateTime::now().unix() - 86_400);
    let mut out = vec![];
    let mut add = |n: i64, one: &str, many: &str, url: &str, table: &str, level: &str| {
        if n > 0 && can(table, "view") {
            out.push(context! { text => format!("{n} {}", if n == 1 { one } else { many }), url, level });
        }
    };
    let pending = PendingChange::objects().filter(PendingChange::STATUS.eq(ApprovalStatus::Pending)).count().await?;
    add(pending, "agent change waits for your approval", "agent changes wait for your approval", "/admin/rangoli_pending_change/?f.status=pending", "rangoli_pending_change", "warning");
    let failed = |name: &'static str| async move {
        let mut q = TaskRecord::objects().filter(TaskRecord::STATUS.eq(TaskStatus::Failed) & TaskRecord::FINISHED_AT.gte(day_ago));
        q = if name.is_empty() { q.exclude(TaskRecord::NAME.eq("rangoli.webhook")) } else { q.filter(TaskRecord::NAME.eq(name)) };
        q.count().await
    };
    let jobs = failed("").await?;
    add(jobs, "background job failed today", "background jobs failed today", "/admin/rangoli_task/?f.status=failed", "rangoli_task", "error");
    let hooks = failed("rangoli.webhook").await?;
    add(hooks, "webhook delivery failed today", "webhook deliveries failed today", "/admin/rangoli_task/?f.status=failed&q=rangoli.webhook", "rangoli_task", "error");
    let agents = crate::auth::ApiToken::objects().filter(crate::auth::ApiToken::LAST_USED_AT.gte(day_ago)).count().await?;
    add(agents, "agent or API token used today", "agents or API tokens used today", "/admin/rangoli_api_token/", "rangoli_api_token", "info");
    Ok(out)
}

// ---------------------------------------------------------------- changelist

/// One changelist column: the primary key or a model field.
#[derive(Clone, Copy)]
enum Column {
    Id,
    Field(usize, &'static FieldMeta),
}

impl Column {
    fn name(self) -> &'static str {
        match self {
            Column::Id => "id",
            Column::Field(_, f) => f.name,
        }
    }
}

fn columns_for(meta: &'static ModelMeta, opts: &Options) -> Vec<Column> {
    let field = |name: &str| meta.fields.iter().enumerate().find(|(_, f)| f.name == name && !f.password).map(|(i, f)| Column::Field(i, f));
    if let Some(names) = &opts.list_display {
        return names.iter().filter_map(|n| if *n == "id" { Some(Column::Id) } else { field(n) }).collect();
    }
    // Default: the display field first (Django's `__str__` column), then the rest.
    let mut cols: Vec<Column> = meta
        .fields
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.password && !matches!(f.ty, FieldType::Text | FieldType::Json))
        .map(|(i, f)| Column::Field(i, f))
        .collect();
    if let Some(pos) = meta.display.and_then(|d| cols.iter().position(|c| c.name() == d)) {
        let first = cols.remove(pos);
        cols.insert(0, first);
    }
    cols.truncate(6);
    cols
}

fn search_fields(meta: &'static ModelMeta, opts: &Options) -> Vec<&'static str> {
    match &opts.search_fields {
        Some(names) => names.clone(),
        None => {
            meta.fields.iter().filter(|f| !f.password && matches!(f.ty, FieldType::Varchar(_) | FieldType::Text)).map(|f| f.name).collect()
        }
    }
}

fn filter_fields(meta: &'static ModelMeta, opts: &Options) -> Vec<&'static FieldMeta> {
    match &opts.list_filter {
        Some(names) => names.iter().filter_map(|n| meta.field(n)).collect(),
        None => meta
            .fields
            .iter()
            .filter(|f| matches!(f.ty, FieldType::Bool | FieldType::DateTime | FieldType::Date) || f.choices.is_some())
            .collect(),
    }
}

async fn list(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, UrlQuery(p): UrlQuery<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let opts = options(&site, meta);
    let per_page = opts.list_per_page.unwrap_or(PAGE_SIZE);
    let base = format!("/admin/{table}/");
    let q = param(&p, "q").unwrap_or("").trim().to_string();
    let page: u64 = param(&p, "p").and_then(|v| v.parse().ok()).filter(|n| *n >= 1).unwrap_or(1);

    let (default_field, default_desc) = opts.ordering.unwrap_or(("id", true));
    let (desc, oname) = match param(&p, "o") {
        Some(o) if o.starts_with('-') => (true, &o[1..]),
        Some(o) => (false, o),
        None => (default_desc, default_field),
    };
    // Only real, visible columns can be sorted on: the name comes from metadata, never from the URL.
    let order_field: &'static str = meta.field(oname).filter(|f| !f.password).map_or("id", |f| f.name);

    let columns = columns_for(meta, opts);
    let searchable = search_fields(meta, opts);
    let filters = filter_fields(meta, opts);

    let mut query = changelist_query(&site, meta, u, &p, &q);
    let filtered = !query.filter.is_empty();
    let (full_count, estimated) = Query::new(meta).table_count().await.map_err(fail)?;
    let count = if filtered { query.count().await.map_err(fail)? } else { full_count };
    query.few_search_matches(count);
    let pages = (count as u64).div_ceil(per_page).max(1);
    let page = page.min(pages);
    query.order = vec![(order_field, desc)];
    query.limit = Some(per_page);
    query.offset = Some((page - 1) * per_page);
    let raw_rows = query.rows().await.map_err(fail)?;

    // Foreign keys show their target's label: one IN query per FK column, not one per row.
    let mut fk_labels: HashMap<usize, HashMap<i64, String>> = HashMap::new();
    for c in &columns {
        if let Column::Field(i, FieldMeta { fk: Some(target), .. }) = *c {
            let ids = raw_rows.iter().filter_map(|(_, v)| match v[i] {
                Value::Int(id) => Some(id),
                _ => None,
            });
            fk_labels.insert(i, labels(&site, target, ids.collect()).await?);
        }
    }
    let rows: Vec<_> = raw_rows
        .iter()
        .map(|(id, vals)| {
            let mut cells: Vec<minijinja::Value> = columns
                .iter()
                .map(|c| match *c {
                    Column::Id => context! { text => id.to_string() },
                    Column::Field(i, f) => match (&vals[i], fk_labels.get(&i)) {
                        (Value::Bool(b), _) => context! { bool => b },
                        (Value::Int(fid), Some(names)) => context! { text => names.get(fid).cloned().unwrap_or_else(|| fid.to_string()) },
                        (v, _) => context! { text => display(v, f) },
                    },
                })
                .collect();
            if cells.is_empty() {
                cells.push(context! { text => repr(meta, *id, vals) });
            }
            context! { id, url => format!("{base}{id}/"), repr => repr(meta, *id, vals), cells }
        })
        .collect();

    // Links keep every current parameter except the ones they change.
    let link = |set: &[(&str, &str)]| {
        let mut pairs: Vec<(String, String)> = p
            .iter()
            .filter(|(k, _)| {
                !matches!(k.as_str(), "p" | "log" | "deleted" | "what" | "warn" | "applied" | "failed" | "n")
                    && !set.iter().any(|(s, _)| s == k)
            })
            .cloned()
            .collect();
        pairs.extend(set.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.to_string(), v.to_string())));
        match serde_urlencoded::to_string(&pairs).unwrap() {
            qs if qs.is_empty() => base.clone(),
            qs => format!("{base}?{qs}"),
        }
    };
    let headers: Vec<_> = if columns.is_empty() {
        vec![context! { label => meta.name, url => None::<String>, sorted => None::<&str> }]
    } else {
        columns
            .iter()
            .map(|c| {
                let name = c.name();
                let active = order_field == name;
                let next = if active && !desc { format!("-{name}") } else { name.to_string() };
                let heading = if name == "id" { "ID".to_string() } else { label(name) };
                context! { label => heading, url => link(&[("o", &next)]), sorted => active.then_some(if desc { "descending" } else { "ascending" }) }
            })
            .collect()
    };
    let mut filter_blocks = vec![];
    for f in &filters {
        let key = format!("f.{}", f.name);
        let current = param(&p, &key).unwrap_or("");
        let choices: Vec<(String, String)> = match (f.ty, f.fk) {
            (FieldType::Bool, _) => [("", "All"), ("1", "Yes"), ("0", "No")].iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
            (FieldType::DateTime | FieldType::Date, _) => DATE_RANGES.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
            (FieldType::Varchar(_), _) if f.choices.is_some() => std::iter::once((String::new(), "All".to_string()))
                .chain(f.choices.into_iter().flatten().map(|(v, l)| (v.to_string(), l.to_string())))
                .collect(),
            (_, Some(target)) => std::iter::once((String::new(), "All".to_string()))
                .chain(label_rows(&site, target, None).await?.into_iter().map(|(id, l)| (id.to_string(), l)))
                .collect(),
            _ => continue,
        };
        let options: Vec<_> =
            choices.iter().map(|(v, l)| context! { label => l, url => link(&[(&key, v)]), active => current == v.as_str() }).collect();
        filter_blocks.push(context! { label => label(f.name).to_lowercase(), options });
    }
    let keep: Vec<(String, String)> = p.iter().filter(|(k, _)| k.starts_with("f.") || k == "o").cloned().collect();
    let shown: Vec<&(String, String)> = p.iter().filter(|(k, _)| k.starts_with("f.") || k == "q").collect();
    let export_url = format!("/admin/{table}/export.csv?{}", serde_urlencoded::to_string(&shown).unwrap_or_default());
    let page_links: Vec<_> = (1..=pages)
        .filter(|n| *n == 1 || *n == pages || n.abs_diff(page) <= 3)
        .map(|n| context! { n, url => link(&[("p", &n.to_string())]), current => n == page })
        .collect();
    let what = if count == 1 { verbose(meta.name) } else { verbose_plural(meta.name) };
    render(
        &site,
        "list.html",
        context! {
            title => format!("Select {} to change", verbose(meta.name)),
            messages => match action_message(&site, meta, &p) { Some(m) => Some(m), None => message(&p, u, "list").await },
            actions => options(&site, meta).actions.iter().map(|a| context! { name => a.name, label => a.label }).collect::<Vec<_>>(),
            q, keep, count, full_count, filtered, what, page_links, estimated, export_url,
            columns => headers, rows, filters => filter_blocks, searchable => !searchable.is_empty() || !meta.search.is_empty(), clear_url => base.clone(),
            ..chrome(&site, u, Some(meta))
        },
    )
}

/// The rows a changelist shows for these parameters: row permissions, search and filters.
/// The export uses it too, so a download is exactly what's on screen (every page of it).
fn changelist_query(site: &Site, meta: &'static ModelMeta, u: &User, p: &[(String, String)], q: &str) -> Query {
    let opts = options(site, meta);
    let searchable = search_fields(meta, opts);
    let filters = filter_fields(meta, opts);
    let mut query = Query::new(meta);
    query.filter.extend(row_filter(site, meta, u));
    if !q.is_empty() && !meta.search.is_empty() {
        query.filter.push(orm::search_node(meta, q)); // the model's full-text index
    } else if !q.is_empty() && !searchable.is_empty() {
        let pat = format!("%{}%", like_escape(q));
        query.filter.push(Node::Or(searchable.iter().map(|c| Node::Like(c, pat.clone())).collect()));
    }
    for f in &filters {
        let Some(v) = param(p, &format!("f.{}", f.name)) else { continue };
        let node = match f.ty {
            FieldType::Bool if v == "1" || v == "0" => Node::Cmp(f.name, "=", Value::Bool(v == "1"), FieldType::Bool),
            FieldType::DateTime => match date_range_start(v) {
                Some(since) => Node::Cmp(f.name, ">=", Value::Int(since.unix()), FieldType::DateTime),
                None => continue,
            },
            FieldType::Date => match date_range_start(v) {
                Some(since) => Node::Cmp(f.name, ">=", Value::Int(since.unix().div_euclid(86_400)), FieldType::Date),
                None => continue,
            },
            FieldType::Int if f.fk.is_some() => match v.parse::<i64>() {
                Ok(id) => Node::Cmp(f.name, "=", Value::Int(id), FieldType::Int),
                Err(_) => continue,
            },
            FieldType::Varchar(_) if f.choices.is_some_and(|c| c.iter().any(|(val, _)| *val == v)) => {
                Node::Cmp(f.name, "=", Value::Text(v.to_string()), f.ty)
            }
            _ => continue,
        };
        query.filter.push(node);
    }
    query
}

/// A cell that spreadsheets would run as a formula (`=`, `+`, `-`, `@`, tab, CR) gets a `'`
/// in front: an exported CSV can't attack whoever opens it (CWE-1236).
fn csv_cell(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

fn csv_value(v: &Value, f: &FieldMeta) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) if f.ty == FieldType::DateTime => DateTime::from_unix(*i).to_string(),
        Value::Int(i) if f.ty == FieldType::Date => crate::Date::from_days(*i).to_string(),
        Value::Int(i) => match f.ty {
            FieldType::Decimal(p) => crate::decimal::format_units(*i, p),
            _ => i.to_string(),
        },
        Value::Float(x) => x.to_string(),
        Value::Text(t) => t.clone(),
    }
}

/// GET /admin/<table>/export.csv: every row the changelist shows, streamed in batches by id.
/// Passwords, private, encrypted and excluded fields are left out: files travel.
async fn export_csv(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, UrlQuery(p): UrlQuery<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let q = param(&p, "q").unwrap_or("").trim().to_string();
    let base = changelist_query(&site, meta, u, &p, &q);
    let excluded = &options(&site, meta).exclude;
    let cols: Vec<usize> = meta
        .fields
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            !f.password && !f.private && !matches!(f.ty, FieldType::Encrypted | FieldType::Vector(_)) && !excluded.contains(&f.name)
        })
        .map(|(i, _)| i)
        .collect();
    let header: Vec<String> = std::iter::once("id".to_string()).chain(cols.iter().map(|i| meta.fields[*i].name.to_string())).collect();
    let first = format!("{}\r\n", header.join(","));
    // State: the last id sent, or None when finished.
    let stream = futures_util::stream::unfold(Some(0i64), move |after| {
        let (mut q, cols) = (base.clone(), cols.clone());
        async move {
            let after = after?;
            q.filter.push(Node::Cmp("id", ">", Value::Int(after), FieldType::Int));
            q.order = vec![("id", false)];
            q.limit = Some(2_000);
            let rows = match q.rows().await {
                Ok(r) => r,
                Err(e) => return Some((Err(std::io::Error::other(e.to_string())), None)),
            };
            if rows.is_empty() {
                return None;
            }
            let mut chunk = String::new();
            for (id, vals) in &rows {
                let cells = std::iter::once(id.to_string()).chain(cols.iter().map(|i| csv_cell(&csv_value(&vals[*i], &meta.fields[*i]))));
                chunk.push_str(&cells.collect::<Vec<_>>().join(","));
                chunk.push_str("\r\n");
            }
            let next = (rows.len() == 2_000).then(|| rows.last().map_or(after, |(id, _)| *id));
            Some((Ok::<_, std::io::Error>(bytes::Bytes::from(chunk)), next))
        }
    });
    let body = futures_util::StreamExt::chain(futures_util::stream::once(async move { Ok(bytes::Bytes::from(first)) }), stream);
    let filename = format!("attachment; filename=\"{table}.csv\"");
    Ok((
        [(header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()), (header::CONTENT_DISPOSITION, filename)],
        axum::body::Body::from_stream(body),
    )
        .into_response())
}

// ---------------------------------------------------------------- password reset

async fn reset_request_page(State(site): S) -> Page {
    render(&site, "password_reset.html", context! { title => "Forgot your password?" })
}

async fn reset_request_submit(State(site): S, headers: HeaderMap, Form(f): Form<Pairs>) -> Page {
    let origin = crate::oauth::origin(&headers);
    auth::request_password_reset(param(&f, "email").unwrap_or(""), |t| format!("{origin}/admin/reset/{t}")).await.map_err(fail)?;
    // The same answer whether or not the address has an account.
    render(&site, "password_reset.html", context! { title => "Check your email", sent => true })
}

async fn reset_page(State(site): S, Path(token): Path<String>) -> Page {
    let valid = auth::password_reset_user(&token).await.map_err(fail)?.is_some();
    render(&site, "password_reset.html", context! { title => "Choose a new password", token => valid.then_some(token), invalid => !valid })
}

async fn reset_submit(State(site): S, Path(token): Path<String>, Form(f): Form<Pairs>) -> Page {
    let (p1, p2) = (param(&f, "new_password1").unwrap_or(""), param(&f, "new_password2").unwrap_or(""));
    let error = if p1.chars().count() < 8 {
        Some("This password is too short. It must contain at least 8 characters.")
    } else if p1 != p2 {
        Some("The two password fields didn’t match.")
    } else {
        None
    };
    if let Some(error) = error {
        let page = render(&site, "password_reset.html", context! { title => "Choose a new password", token, error })?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    if !auth::reset_password(&token, p1).await.map_err(fail)? {
        return render(&site, "password_reset.html", context! { title => "Choose a new password", invalid => true });
    }
    Ok(Redirect::to("/admin/login?reset=1").into_response())
}

// ---------------------------------------------------------------- what requests did to the database

async fn requests_page(State(site): S, user: CurrentUser, uri: Uri) -> Page {
    let u = staff(&user, &uri)?;
    let round = |x: f64| (x * 10.0).round() / 10.0;
    let requests: Vec<_> = crate::inspect::recent()
        .iter()
        .map(|r| {
            let repeats: Vec<_> = r.repeats().iter().map(|x| context! { sql => x.sql, times => x.times, ms => round(x.ms) }).collect();
            let statements: Vec<_> = r.statements.iter().map(|s| context! { sql => s.sql, ms => round(s.ms) }).collect();
            context! { method => r.method, path => r.path, status => r.status, ms => round(r.ms), db_ms => round(r.db_ms()), count => r.statements.len(), repeats, statements }
        })
        .collect();
    let (enabled, repeat) = (crate::inspect::enabled(), crate::inspect::REPEAT_WARNING);
    render(&site, "requests.html", context! { title => "Requests", requests, enabled, repeat, ..chrome(&site, u, None) })
}

// ---------------------------------------------------------------- foreign-key search

/// GET /admin/<table>/autocomplete?q=: up to 20 `<option>`s for a foreign-key search box,
/// matching the table's display field, within the staff member's permissions and rows.
async fn autocomplete(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, UrlQuery(p): UrlQuery<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = site.models.iter().copied().find(|m| m.table == table).ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    allowed(meta.table, "view")?;
    let q = param(&p, "q").unwrap_or("").trim().to_string();
    let disp = meta.display.and_then(|d| meta.fields.iter().find(|f| f.name == d));
    let mut query = Query::new(meta);
    query.fields = disp.map_or(&[][..], std::slice::from_ref);
    query.filter.extend(row_filter(&site, meta, u));
    match (q.parse::<i64>(), disp) {
        (Ok(id), _) => query.filter.push(Node::Cmp("id", "=", Value::Int(id), FieldType::Int)),
        (_, Some(f)) if !q.is_empty() => query.filter.push(Node::Like(f.name, format!("%{}%", like_escape(&q)))),
        _ => {}
    }
    query.order = vec![("id", false)];
    query.limit = Some(20);
    let rows = query.rows().await.map_err(fail)?;
    let esc = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;");
    let html: String = rows
        .iter()
        .map(|(id, vals)| {
            let label = match (disp, vals.first()) {
                (Some(f), Some(v)) => display(v, f),
                _ => format!("{} object ({id})", meta.name),
            };
            format!("<option value=\"{} (#{id})\" data-id=\"{id}\"></option>", esc(&label))
        })
        .collect();
    Ok(Html(html).into_response())
}

/// The admin form's field validation, then the model's whole-row rules (`App::validate`).
async fn validate_all(
    meta: &'static ModelMeta,
    form: &HashMap<String, String>,
    id: Option<i64>,
    fixed: &[&str],
) -> Result<Vec<(&'static str, Value, FieldType)>, HashMap<String, String>> {
    let cols = validate(meta, form, id.is_none(), fixed).await?;
    match crate::validators::check(meta, id, &cols).await {
        Ok(Ok(())) => Ok(cols),
        Ok(Err(errors)) => Err(errors),
        Err(e) => Err([("__all__".to_string(), e.to_string())].into()),
    }
}

// ---------------------------------------------------------------- CSV import

const IMPORT_MAX_ROWS: usize = 10_000;

/// One CSV row, checked and ready to apply.
struct Planned {
    line: usize,
    id: Option<i64>,
    action: &'static str,
    cols: Vec<(&'static str, Value, FieldType)>,
    repr: String,
    errors: String,
}

async fn import_page(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = addable(&site, &table)?;
    let title = format!("Import {}", verbose_plural(meta.name));
    render(&site, "import.html", context! { title, ..chrome(&site, u, Some(meta)) })
}

/// Turn the CSV into a plan: every row validated exactly as the admin form would, against the
/// row as it stands (for updates), within the staff member's row permissions.
async fn plan_import(site: &Site, meta: &'static ModelMeta, u: &User, text: &str) -> Result<Vec<Planned>, String> {
    let rows =
        super::csv::parse(text).map_err(|line| format!("The CSV stops making sense on line {line} (an unclosed or stray quote?)."))?;
    let Some((header, body)) = rows.split_first() else { return Err("The CSV is empty.".into()) };
    if body.len() > IMPORT_MAX_ROWS {
        return Err(format!("{} rows is more than one import takes ({IMPORT_MAX_ROWS}); split the file.", body.len()));
    }
    let opts = options(site, meta);
    let mut columns: Vec<Option<&'static FieldMeta>> = vec![];
    for h in header {
        let h = h.trim();
        if h == "id" {
            columns.push(None);
            continue;
        }
        match meta.field(h) {
            Some(f)
                if !f.password
                    && !f.is_auto()
                    && !matches!(f.ty, FieldType::Opaque | FieldType::Vector(_))
                    && !opts.exclude.contains(&f.name)
                    && !opts.readonly_fields.contains(&f.name) =>
            {
                columns.push(Some(f))
            }
            Some(_) => return Err(format!("The column `{h}` can't be imported.")),
            None => return Err(format!("There's no field called `{h}`. Columns must match field names; an export shows them.")),
        }
    }
    let id_col = header.iter().position(|h| h.trim() == "id");
    let mut plan = vec![];
    for (n, cells) in body.iter().enumerate() {
        let line = n + 2;
        let fail = |errors: String| Planned { line, id: None, action: "Error", cols: vec![], repr: String::new(), errors };
        if cells.len() != header.len() {
            plan.push(fail(format!("{} cells, but the header has {}.", cells.len(), header.len())));
            continue;
        }
        let id = match id_col.map(|i| cells[i].trim()).filter(|s| !s.is_empty()) {
            None => None,
            Some(s) => match s.parse::<i64>() {
                Ok(id) => Some(id),
                Err(_) => {
                    plan.push(fail(format!("`{s}` isn't an id.")));
                    continue;
                }
            },
        };
        let existing = match id {
            Some(id) => {
                let mut q = Query::new(meta);
                q.filter.push(by_id(id));
                q.filter.extend(row_filter(site, meta, u));
                match q.rows().await.map_err(|e| e.to_string())?.pop() {
                    Some((_, vals)) => Some(vals),
                    None => {
                        plan.push(fail(format!("There's no row {id} (or it isn't one you may change).")));
                        continue;
                    }
                }
            }
            None => None,
        };
        let mut form = existing.as_ref().map(|v| raw_values(meta, v)).unwrap_or_default();
        for (f, cell) in columns.iter().zip(cells) {
            let Some(f) = f else { continue };
            let v = cell.trim();
            if f.ty == FieldType::Bool {
                if matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on" | "y") {
                    form.insert(f.name.to_string(), "on".into());
                } else {
                    form.remove(f.name);
                }
            } else {
                form.insert(f.name.to_string(), cell.clone());
            }
        }
        match validate_all(meta, &form, id, &fixed_fields(meta, opts, &form)).await {
            Err(errors) => {
                let mut msgs: Vec<String> = errors.into_iter().map(|(k, v)| format!("{k}: {v}")).collect();
                msgs.sort();
                plan.push(fail(msgs.join(" ")));
            }
            Ok(cols) => {
                let repr = form.get(meta.display.unwrap_or("id")).cloned().unwrap_or_else(|| id.map(|i| i.to_string()).unwrap_or_default());
                let action = match &existing {
                    None => "Add",
                    Some(old) => {
                        let same = cols
                            .iter()
                            .filter(|(c, ..)| !meta.field(c).is_some_and(|f| f.is_auto()))
                            .all(|(c, v, _)| meta.fields.iter().position(|f| f.name == *c).is_some_and(|i| &old[i] == v));
                        if same {
                            "Unchanged"
                        } else {
                            "Change"
                        }
                    }
                };
                plan.push(Planned { line, id, action, cols, repr, errors: String::new() });
            }
        }
    }
    Ok(plan)
}

async fn import_submit(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, Form(f): Form<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = addable(&site, &table)?;
    let title = format!("Import {}", verbose_plural(meta.name));
    let text = param(&f, "csv").unwrap_or("").to_string();
    let plan = match plan_import(&site, meta, u, &text).await {
        Ok(p) => p,
        Err(error) => return render(&site, "import.html", context! { title, error, csv => text, ..chrome(&site, u, Some(meta)) }),
    };
    let count = |a: &str| plan.iter().filter(|p| p.action == a).count();
    let (adds, changes, same, errors) = (count("Add"), count("Change"), count("Unchanged"), count("Error"));
    if changes > 0 && !can(meta.table, "change") {
        let error = "These rows change existing records, and your groups don't allow changing them.";
        return render(&site, "import.html", context! { title, error, csv => text, ..chrome(&site, u, Some(meta)) });
    }
    if param(&f, "confirm") == Some("1") && errors == 0 {
        let applied = crate::atomic(async {
            for p in plan.iter().filter(|p| p.action == "Add" || p.action == "Change") {
                let id = match p.id {
                    None => orm::insert_row(meta.table, &p.cols).await?,
                    Some(id) => {
                        let mut q = Query::new(meta);
                        q.filter.push(by_id(id));
                        q.update(&p.cols).await?;
                        id
                    }
                };
                still_visible(&site, meta, u, id).await?;
                let vals = row_values(meta, id).await?;
                let (action, msg) = if p.id.is_none() { (ADDITION, "Added by CSV import.") } else { (CHANGE, "Changed by CSV import.") };
                log(u, meta, id, &repr(meta, id, &vals), action, msg.into()).await?;
            }
            Ok(())
        })
        .await;
        return match applied {
            Ok(()) => {
                let done = format!("Imported: {adds} added, {changes} changed, {same} unchanged.");
                render(&site, "import.html", context! { title, done, ..chrome(&site, u, Some(meta)) })
            }
            Err(e) => {
                let error = format!("Nothing was imported: {}", db_error_message(&e).map_or_else(|| e.to_string(), str::to_string));
                render(&site, "import.html", context! { title, error, csv => text, ..chrome(&site, u, Some(meta)) })
            }
        };
    }
    let summary = format!("{adds} to add, {changes} to change, {same} unchanged, {errors} with problems.");
    let rows: Vec<_> = plan.iter().map(|p| context! { line => p.line, action => p.action, repr => p.repr, errors => p.errors }).collect();
    let can_apply = errors == 0 && adds + changes > 0;
    let error = (errors > 0).then_some("Fix the rows with problems and preview again; nothing is imported while any row has one.");
    render(
        &site,
        "import.html",
        context! { title, plan => rows, summary, can_apply, changes => adds + changes, csv => text, error, ..chrome(&site, u, Some(meta)) },
    )
}

/// The result of a custom action. Only registered action names are shown, with their
/// registered labels, so a crafted link can't put words in the admin's mouth.
fn action_message(site: &Site, meta: &ModelMeta, p: &[(String, String)]) -> Option<minijinja::Value> {
    let find = |name: &str| options(site, meta).actions.iter().find(|a| a.name == name);
    if let Some(a) = param(p, "failed").and_then(find) {
        return Some(context! { level => "error", text => format!("The “{}” action failed, so nothing was changed.", a.label) });
    }
    let a = param(p, "applied").and_then(find)?;
    let n: usize = param(p, "n")?.parse().ok()?;
    let what = if n == 1 { verbose(meta.name) } else { verbose_plural(meta.name) };
    Some(context! { level => "success", text => format!("Applied “{}” to {n} {what}.", a.label) })
}

/// Django's date filter choices: (URL value, label).
const DATE_RANGES: [(&str, &str); 5] =
    [("", "Any date"), ("today", "Today"), ("7d", "Past 7 days"), ("month", "This month"), ("year", "This year")];

fn date_range_start(v: &str) -> Option<DateTime> {
    let now = DateTime::now();
    Some(match v {
        "today" => now.start_of_day(),
        "7d" => DateTime::from_unix(now.start_of_day().unix() - 6 * 86_400),
        "month" => now.start_of_month(),
        "year" => now.start_of_year(),
        _ => return None,
    })
}

/// Actions from the changelist. Like Django, deleting asks for confirmation on its own page first.
async fn bulk(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, Form(f): Form<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let ids: Vec<i64> = f.iter().filter(|(k, _)| k == "_selected_action").filter_map(|(_, v)| v.parse().ok()).collect();
    if param(&f, "action").unwrap_or("").is_empty() {
        return Ok(Redirect::to(&format!("/admin/{table}/?warn=noaction")).into_response());
    }
    if ids.is_empty() {
        return Ok(Redirect::to(&format!("/admin/{table}/?warn=noitems")).into_response());
    }
    let mut q = Query::new(meta);
    q.filter.push(Node::In("id", ids.iter().map(|i| Value::Int(*i)).collect(), FieldType::Int));
    q.filter.extend(row_filter(&site, meta, u));
    let rows = q.rows().await.map_err(fail)?;
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect(); // only rows this staff member may touch
    let chosen = param(&f, "action").unwrap_or("");
    allowed(meta.table, if chosen == "delete_selected" { "delete" } else { "change" })?;
    if chosen != "delete_selected" {
        let Some(action) = options(&site, meta).actions.iter().find(|a| a.name == chosen).cloned() else {
            return Ok(Redirect::to(&format!("/admin/{table}/?warn=noaction")).into_response());
        };
        // Only rows that still exist count; the action and its History entries commit together.
        let found: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let outcome = crate::atomic(async {
            super::ACTING.scope(u.clone(), (action.run)(found.clone())).await?;
            for (id, vals) in &rows {
                log(u, meta, *id, &repr(meta, *id, vals), CHANGE, format!("Ran the “{}” action.", action.label)).await?;
            }
            Ok(())
        })
        .await;
        let qs = match outcome {
            Ok(()) => serde_urlencoded::to_string([("applied", action.name.to_string()), ("n", found.len().to_string())]),
            Err(e) => {
                eprintln!("rangoli admin: action `{}` on {table} failed: {e}", action.name);
                serde_urlencoded::to_string([("failed", action.name)])
            }
        }
        .unwrap();
        return Ok(Redirect::to(&format!("/admin/{table}/?{qs}")).into_response());
    }
    if param(&f, "post") != Some("yes") {
        let related = related(&site, meta, &ids).await?;
        let objects: Vec<_> = rows
            .iter()
            .map(|(id, v)| context! { model => capitalize(&verbose(meta.name)), repr => repr(meta, *id, v), url => format!("/admin/{table}/{id}/") })
            .collect();
        return render(
            &site,
            "delete.html",
            context! {
                title => "Are you sure?", bulk => true, objects, protected => related.protected, cascaded => related.cascaded,
                summary => summary(meta, rows.len(), &related), ids => ids, ..chrome(&site, u, Some(meta))
            },
        );
    }
    // The rows and their log entries go together or not at all.
    crate::atomic(async {
        q.delete().await?;
        for (id, vals) in &rows {
            log(u, meta, *id, &repr(meta, *id, vals), DELETION, String::new()).await?;
        }
        Ok(())
    })
    .await
    .map_err(fail)?;
    let what = if rows.len() == 1 { verbose(meta.name) } else { verbose_plural(meta.name) };
    let qs = serde_urlencoded::to_string([("deleted", rows.len().to_string()), ("what", what)]).unwrap();
    Ok(Redirect::to(&format!("/admin/{table}/?{qs}")).into_response())
}

// ---------------------------------------------------------------- related objects

struct Related {
    protected: Vec<String>,
    /// (model name, plural, labels) for rows deleted along with the target.
    cascaded: Vec<minijinja::Value>,
    counts: Vec<(String, usize)>,
}

/// What deleting `ids` of `meta` touches: rows that block it, and rows that cascade with it.
async fn related(site: &Site, meta: &ModelMeta, ids: &[i64]) -> Result<Related, Response> {
    let mut out = Related { protected: vec![], cascaded: vec![], counts: vec![] };
    for m in &site.models {
        for f in m.fields.iter().filter(|f| f.fk == Some(meta.table)) {
            let mut q = Query::new(m);
            q.filter.push(Node::In(f.name, ids.iter().map(|i| Value::Int(*i)).collect(), FieldType::Int));
            q.limit = Some(50);
            let rows = q.rows().await.map_err(fail)?;
            if rows.is_empty() {
                continue;
            }
            let labels: Vec<String> = rows.iter().map(|(id, v)| format!("{}: {}", capitalize(&verbose(m.name)), repr(m, *id, v))).collect();
            if !f.cascade {
                out.protected.extend(labels);
            } else if site.admin.iter().any(|a| a.table == m.table) {
                // Framework bookkeeping (sessions) cascades silently, as Django's would.
                out.counts.push((title_plural(m.name), rows.len()));
                out.cascaded.extend(labels.into_iter().map(|l| context! { label => l }));
            }
        }
    }
    Ok(out)
}

fn summary(meta: &ModelMeta, n: usize, r: &Related) -> Vec<String> {
    std::iter::once(format!("{}: {n}", title_plural(meta.name))).chain(r.counts.iter().map(|(p, c)| format!("{p}: {c}"))).collect()
}

// ---------------------------------------------------------------- forms

/// Raw form strings for a stored row (password fields stay blank).
fn raw_values(meta: &ModelMeta, vals: &[Value]) -> HashMap<String, String> {
    meta.fields
        .iter()
        .zip(vals)
        .filter(|(f, _)| !f.password)
        .filter_map(|(f, v)| {
            let s = match v {
                Value::Null | Value::Bool(false) => return None,
                Value::Bool(true) => "on".into(),
                Value::Int(i) if f.ty == FieldType::DateTime => DateTime::from_unix(*i).input_value(),
                Value::Int(i) if f.ty == FieldType::Date => crate::Date::from_days(*i).to_string(),
                Value::Int(i) => match f.ty {
                    FieldType::Decimal(p) => crate::decimal::format_units(*i, p),
                    _ => i.to_string(),
                },
                Value::Float(x) => x.to_string(),
                Value::Text(s) if f.ty == FieldType::Json => serde_json::from_str::<serde_json::Value>(s)
                    .and_then(|j| serde_json::to_string_pretty(&j))
                    .unwrap_or_else(|_| s.clone()),
                Value::Text(s) => s.clone(),
            };
            Some((f.name.to_string(), s))
        })
        .collect()
}

/// `(id, label)` rows of `table`, optionally limited to `ids`.
/// The most rows a foreign key's `<select>` lists; bigger tables get a search box.
const SELECT_MAX: usize = 1000;

async fn label_rows(site: &Site, table: &str, ids: Option<Vec<i64>>) -> Result<Vec<(i64, String)>, Response> {
    let Some(target) = site.models.iter().find(|m| m.table == table) else {
        return Ok(vec![]);
    };
    let disp = target.display.and_then(|d| target.fields.iter().find(|f| f.name == d));
    let mut q = Query::new(target);
    q.fields = disp.map_or(&[][..], std::slice::from_ref);
    q.order = vec![("id", false)];
    match ids {
        Some(ids) => q.filter.push(Node::In("id", ids.into_iter().map(Value::Int).collect(), FieldType::Int)),
        None => q.limit = Some(SELECT_MAX as u64 + 1), // one more than a <select> shows: past that, forms search instead
    }
    Ok(q.rows()
        .await
        .map_err(fail)?
        .into_iter()
        .map(|(id, vals)| {
            (
                id,
                match (disp, vals.first()) {
                    (Some(f), Some(v)) => display(v, f),
                    _ => format!("{} object ({id})", target.name),
                },
            )
        })
        .collect())
}

async fn labels(site: &Site, table: &str, mut ids: Vec<i64>) -> Result<HashMap<i64, String>, Response> {
    ids.sort_unstable();
    ids.dedup();
    Ok(label_rows(site, table, Some(ids)).await?.into_iter().collect())
}

/// The fields of one form. `prefix` goes in front of input names (inline rows use
/// `table-0-`); `raw` and `errors` are keyed by the bare field name.
#[allow(clippy::too_many_arguments)]
async fn form_fields(
    site: &Site,
    meta: &ModelMeta,
    raw: &HashMap<String, String>,
    errors: &HashMap<String, String>,
    is_add: bool,
    readonly: &[&str],
    prefix: &str,
    hidden: &[&str],
) -> Result<Vec<minijinja::Value>, Response> {
    let mut out = vec![];
    for f in meta.fields.iter().filter(|f| !f.is_auto() && !hidden.contains(&f.name)) {
        let mut search_in: Option<&str> = None;
        let options: Vec<(String, String)> = match (f.fk, f.choices) {
            (Some(t), _) => {
                let rows = label_rows(site, t, None).await?;
                if rows.len() > SELECT_MAX {
                    search_in = Some(t); // too many for a <select>: a search box
                    vec![]
                } else {
                    rows.into_iter().map(|(id, l)| (id.to_string(), l)).collect()
                }
            }
            (_, Some(choices)) => choices.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
            _ => vec![],
        };
        if readonly.contains(&f.name) || matches!(f.ty, FieldType::Opaque | FieldType::Vector(_)) {
            let raw = raw.get(f.name).cloned().unwrap_or_default();
            let shown = match (f.ty, raw.as_str()) {
                (_, "") if f.ty != FieldType::Bool => "-".to_string(),
                (FieldType::Bool, v) => (if v.is_empty() { "No" } else { "Yes" }).to_string(),
                (FieldType::DateTime, v) => DateTime::parse(v).map_or(v.to_string(), DateTime::human),
                _ => options.iter().find(|(id, _)| *id == raw).map_or(raw.clone(), |(_, l)| l.clone()),
            };
            out.push(context! { name => format!("{prefix}{}", f.name), label => label(f.name), kind => "readonly", value => shown });
            continue;
        }
        let kind = match (f.fk.is_some() || f.choices.is_some(), f.password, f.ty) {
            (true, ..) => "select",
            (_, _, FieldType::Json) => "json",
            (_, true, _) => "password",
            (_, _, FieldType::Bool) => "checkbox",
            (_, _, FieldType::Int) => "number",
            (_, _, FieldType::Float) => "float",
            (_, _, FieldType::Text) => "textarea",
            (_, _, FieldType::DateTime) => "datetime",
            (_, _, FieldType::Date) => "date",
            (_, _, FieldType::Decimal(_)) => "decimal",
            _ => "text",
        };
        let value = raw.get(f.name).cloned().unwrap_or_default();
        let (kind, current) = match search_in {
            Some(t) => {
                let shown = match value.parse::<i64>() {
                    Ok(id) => label_rows(site, t, Some(vec![id])).await?.pop().map(|(_, l)| l).unwrap_or_default(),
                    Err(_) => String::new(),
                };
                ("autocomplete", Some(shown))
            }
            None => (kind, None),
        };
        out.push(context! {
            name => format!("{prefix}{}", f.name), label => label(f.name), kind, checked => !value.is_empty(), value, options,
            target => search_in, current,
            required => !f.null && f.ty != FieldType::Bool && (is_add || !f.password),
            maxlength => match f.ty { FieldType::Varchar(n) => Some(n), _ => None },
            help => if f.password && !is_add { Some("Leave blank to keep the current password.") } else if f.help.is_some() { f.help } else if f.ty == FieldType::DateTime { Some("Date and time in UTC.") } else { None },
            error => errors.get(f.name),
        });
    }
    Ok(out)
}

/// Row `id` exists but is outside this staff member's rows: answer 404, as if it didn't.
async fn visible(site: &Site, meta: &'static ModelMeta, u: &User, id: i64) -> Result<(), Response> {
    let Some(f) = row_filter(site, meta, u) else { return Ok(()) };
    let exists = orm::row_matches(meta, id, Node::In("id", vec![Value::Int(id)], FieldType::Int)).await.map_err(fail)?;
    if exists && !orm::row_matches(meta, id, f).await.map_err(fail)? {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    Ok(())
}

fn row_filter(site: &Site, meta: &ModelMeta, u: &User) -> Option<Node> {
    options(site, meta).rows.as_ref().and_then(|p| p.filter(Some(u)))
}

/// Inside a save's transaction: the row must still be one this staff member may see.
async fn still_visible(site: &Site, meta: &'static ModelMeta, u: &User, id: i64) -> crate::Result<()> {
    if let Some(f) = row_filter(site, meta, u) {
        if !orm::row_matches(meta, id, f).await? {
            return Err(Error::Forbidden(OUTSIDE.into()));
        }
    }
    Ok(())
}

const OUTSIDE: &str = "You can't save a row outside the rows you may see.";

fn db_error_message(e: &Error) -> Option<&'static str> {
    if matches!(e, Error::Forbidden(_)) {
        Some(OUTSIDE)
    } else if e.is_unique_violation() {
        Some("A record with one of these unique values already exists.")
    } else if e.is_foreign_key_violation() {
        Some("Select a valid choice. That choice is not one of the available choices.")
    } else {
        None
    }
}

/// Django's change message: "Changed title and body."
fn change_message(meta: &ModelMeta, old: &[Value], cols: &[(&'static str, Value, FieldType)], relations: &[&str]) -> String {
    let changed: Vec<String> = cols
        .iter()
        .filter(|(name, v, _)| {
            meta.fields
                .iter()
                .position(|f| f.name == *name)
                .is_some_and(|i| !meta.fields[i].is_auto() && (meta.fields[i].password || &old[i] != v))
        })
        .map(|(name, ..)| label(name).to_lowercase())
        .chain(relations.iter().map(|r| label(r).to_lowercase()))
        .collect();
    match changed.as_slice() {
        [] => "No fields changed.".into(),
        [one] => format!("Changed {one}."),
        [init @ .., last] => format!("Changed {} and {last}.", init.join(", ")),
    }
}

/// The form's fields grouped into its fieldsets: unlisted fields first, untitled.
fn sections(sets: &[(&'static str, Vec<&'static str>)], fields: Vec<minijinja::Value>) -> Vec<minijinja::Value> {
    let name = |f: &minijinja::Value| f.get_attr("name").ok().and_then(|n| n.as_str().map(str::to_string)).unwrap_or_default();
    let listed = |n: &str| sets.iter().any(|(_, fs)| fs.contains(&n));
    let mut out = vec![];
    let rest: Vec<_> = fields.iter().filter(|f| !listed(&name(f))).cloned().collect();
    if !rest.is_empty() || sets.is_empty() {
        out.push(context! { title => None::<&str>, fields => rest });
    }
    for (title, names) in sets {
        let fs: Vec<_> = names.iter().filter_map(|n| fields.iter().find(|f| name(f) == *n).cloned()).collect();
        if !fs.is_empty() {
            out.push(context! { title, fields => fs });
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn form_page(
    site: &Site,
    u: &User,
    meta: &'static ModelMeta,
    id: Option<i64>,
    raw: &HashMap<String, String>,
    links: &Links,
    errors: HashMap<String, String>,
    status: StatusCode,
    messages: Option<minijinja::Value>,
) -> Page {
    let is_add = id.is_none();
    let o = options(site, meta);
    let mut fields = form_fields(site, meta, raw, &errors, is_add, &o.readonly_fields, "", &o.exclude).await?;
    for f in &mut fields {
        let name = f.get_attr("name").ok().and_then(|n| n.as_str().map(str::to_string)).unwrap_or_default();
        if let Some((_, c, values)) = o.show_when.iter().find(|(x, ..)| *x == name) {
            let shown = values.contains(&control_value(meta, raw, c));
            *f = context! { show_when => c, show_values => values.join("|"), hidden => !shown, ..f.clone() };
        }
    }
    let inlines = match id {
        Some(id) => inline_blocks(site, meta, id, raw, &errors).await?,
        None => vec![],
    };
    for rel in meta.m2m {
        let chosen = links.get(rel.name).cloned().unwrap_or_default();
        let options: Vec<_> = label_rows(site, rel.target, None)
            .await?
            .into_iter()
            .map(|(oid, l)| context! { id => oid, label => l, selected => chosen.contains(&oid) })
            .collect();
        fields.push(context! {
            name => rel.name, label => label(rel.name), kind => "multiselect", options, error => errors.get(rel.name),
            help => "Hold down Control, or Command on a Mac, to select more than one.",
        });
    }
    let obj = match id {
        Some(id) => Some(repr(meta, id, &fetch_row(meta, id).await?)),
        None => None,
    };
    let mut panels = vec![];
    if let Some(id) = id {
        for p in &o.panels {
            let html = (p.render)(id).await.unwrap_or_else(|e| format!("<p class=\"field-error\">{}</p>", e.to_string().replace('<', "&lt;")));
            panels.push(context! { title => p.title, html => minijinja::Value::from_safe_string(html) });
        }
    }
    let title = if is_add { format!("Add {}", verbose(meta.name)) } else { format!("Change {}", verbose(meta.name)) };
    let page = render(
        site,
        "form.html",
        context! {
            title, obj, id, is_add, sections => sections(&o.fieldsets, fields), inlines, panels, messages, form_error => errors.get("__all__"),
            error_count => errors.len(), ..chrome(site, u, Some(meta))
        },
    )?;
    Ok((status, page).into_response())
}

/// Many-to-many selections: relation name -> related ids.
type Links = HashMap<&'static str, Vec<i64>>;

fn submitted_links(meta: &ModelMeta, pairs: &[(String, String)]) -> Links {
    meta.m2m
        .iter()
        .map(|rel| (rel.name, pairs.iter().filter(|(k, _)| k == rel.name).filter_map(|(_, v)| v.parse().ok()).collect()))
        .collect()
}

async fn current_links(meta: &ModelMeta, id: i64) -> crate::Result<Links> {
    let mut out = Links::new();
    for rel in meta.m2m {
        out.insert(rel.name, orm::link_ids(rel.through, id).await?);
    }
    Ok(out)
}

async fn save_links(meta: &ModelMeta, id: i64, links: &Links) -> crate::Result<()> {
    for rel in meta.m2m {
        orm::set_links(rel.through, id, links.get(rel.name).map_or(&[][..], Vec::as_slice)).await?;
    }
    Ok(())
}

fn after_save(table: &str, id: i64, log_id: i64, form: &HashMap<String, String>) -> Response {
    let to = if form.contains_key("_continue") {
        format!("/admin/{table}/{id}/?log={log_id}")
    } else if form.contains_key("_addanother") {
        format!("/admin/{table}/add?log={log_id}")
    } else {
        format!("/admin/{table}/?log={log_id}")
    };
    Redirect::to(&to).into_response()
}

async fn add_page(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, UrlQuery(p): UrlQuery<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let messages = message(&p, u, "add").await;
    let meta = addable(&site, &table)?;
    // New forms start from the model's defaults, like Django's `initial`.
    let defaults: Vec<Value> = meta.fields.iter().map(|f| f.default.map_or(Value::Null, |d| d.value())).collect();
    form_page(&site, u, meta, None, &raw_values(meta, &defaults), &Links::new(), HashMap::new(), StatusCode::OK, messages).await
}

async fn add_submit(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, Form(pairs): Form<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = addable(&site, &table)?;
    let form: HashMap<String, String> = pairs.iter().cloned().collect();
    let links = submitted_links(meta, &pairs);
    let errors = match validate_all(meta, &form, None, &fixed_fields(meta, options(&site, meta), &form)).await {
        Ok(cols) => match crate::atomic(async {
            let id = orm::insert_row(meta.table, &cols).await?;
            save_links(meta, id, &links).await?;
            still_visible(&site, meta, u, id).await?;
            let vals = row_values(meta, id).await?;
            Ok((id, log(u, meta, id, &repr(meta, id, &vals), ADDITION, "Added.".into()).await?))
        })
        .await
        {
            Ok((id, log_id)) => return Ok(after_save(&table, id, log_id, &form)),
            Err(e) => [("__all__".to_string(), db_error_message(&e).ok_or_else(|| fail(e))?.to_string())].into(),
        },
        Err(errors) => errors,
    };
    form_page(&site, u, meta, None, &form, &links, errors, StatusCode::UNPROCESSABLE_ENTITY, None).await
}

async fn change_page(
    State(site): S,
    user: CurrentUser,
    uri: Uri,
    Path((table, id)): Path<(String, i64)>,
    UrlQuery(p): UrlQuery<Pairs>,
) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    visible(&site, meta, u, id).await?;
    let vals = fetch_row(meta, id).await?;
    let links = current_links(meta, id).await.map_err(fail)?;
    let messages = message(&p, u, "change").await;
    form_page(&site, u, meta, Some(id), &raw_values(meta, &vals), &links, HashMap::new(), StatusCode::OK, messages).await
}

async fn change_submit(
    State(site): S,
    user: CurrentUser,
    uri: Uri,
    Path((table, id)): Path<(String, i64)>,
    Form(pairs): Form<Pairs>,
) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    visible(&site, meta, u, id).await?;
    allowed(meta.table, "change")?;
    let form: HashMap<String, String> = pairs.iter().cloned().collect();
    let links = submitted_links(meta, &pairs);
    let old = fetch_row(meta, id).await?;
    let old_links = current_links(meta, id).await.map_err(fail)?;
    let (inline_ops, inline_errors) = inline_changes(&site, meta, id, &form).await;
    let errors = match validate_all(meta, &form, Some(id), &fixed_fields(meta, options(&site, meta), &form)).await {
        Ok(_) if !inline_errors.is_empty() => inline_errors,
        Ok(cols) => {
            let mut q = Query::new(meta);
            q.filter.push(by_id(id));
            let saved = crate::atomic(async {
                q.update(&cols).await?;
                save_links(meta, id, &links).await?;
                still_visible(&site, meta, u, id).await?;
                let changed_inlines = apply_inlines(u, id, &inline_ops).await?;
                let vals = row_values(meta, id).await?;
                let sorted = |l: &Links, n: &str| {
                    let mut v = l.get(n).cloned().unwrap_or_default();
                    v.sort_unstable();
                    v.dedup();
                    v
                };
                let mut rels: Vec<&str> = meta.m2m.iter().map(|r| r.name).filter(|n| sorted(&old_links, n) != sorted(&links, n)).collect();
                rels.extend(changed_inlines);
                log(u, meta, id, &repr(meta, id, &vals), CHANGE, change_message(meta, &old, &cols, &rels)).await
            })
            .await;
            match saved {
                Ok(log_id) => return Ok(after_save(&table, id, log_id, &form)),
                Err(e) => [("__all__".to_string(), db_error_message(&e).ok_or_else(|| fail(e))?.to_string())].into(),
            }
        }
        Err(mut errors) => {
            errors.extend(inline_errors);
            errors
        }
    };
    form_page(&site, u, meta, Some(id), &form, &links, errors, StatusCode::UNPROCESSABLE_ENTITY, None).await
}

/// Fields a form submission can't change: read-only ones, excluded ones, and conditional ones
/// whose controlling field doesn't currently show them.
fn fixed_fields(meta: &ModelMeta, o: &Options, form: &HashMap<String, String>) -> Vec<&'static str> {
    let hidden = o.show_when.iter().filter(|(_, c, values)| !values.contains(&control_value(meta, form, c))).map(|(f, ..)| *f);
    o.readonly_fields.iter().chain(&o.exclude).copied().chain(hidden).collect()
}

/// A controlling field's submitted value as `show_when` compares it (checkboxes: "true"/"false").
fn control_value(meta: &ModelMeta, form: &HashMap<String, String>, name: &str) -> String {
    let raw = form.get(name).map(String::as_str).unwrap_or_default();
    match meta.field(name).map(|f| f.ty) {
        Some(FieldType::Bool) => (if raw.is_empty() || raw == "false" { "false" } else { "true" }).into(),
        _ => raw.into(),
    }
}

// ---------------------------------------------------------------- inlines

type InlineRow = (Option<i64>, HashMap<String, String>, bool);

/// Rows of one inline as submitted: (child id if it exists, its fields, ticked for delete).
fn parse_inline(form: &HashMap<String, String>, inl: &super::Inline) -> Vec<InlineRow> {
    let t = inl.meta.table;
    let total: usize = form.get(&format!("{t}-TOTAL")).and_then(|n| n.parse().ok()).unwrap_or(0).min(500);
    (0..total)
        .map(|i| {
            let prefix = format!("{t}-{i}-");
            let id = form.get(&format!("{prefix}id")).and_then(|v| v.parse().ok());
            let raw = inl
                .meta
                .fields
                .iter()
                .filter_map(|f| form.get(&format!("{prefix}{}", f.name)).map(|v| (f.name.to_string(), v.clone())))
                .collect();
            (id, raw, form.contains_key(&format!("{prefix}DELETE")))
        })
        .collect()
}

/// The inline tables for a change page: existing child rows plus blank ones (or, after a
/// failed save, the rows as submitted, with their errors).
async fn inline_blocks(
    site: &Site,
    meta: &'static ModelMeta,
    parent: i64,
    posted: &HashMap<String, String>,
    errors: &HashMap<String, String>,
) -> Result<Vec<minijinja::Value>, Response> {
    let mut out = vec![];
    for inl in &options(site, meta).inlines {
        let t = inl.meta.table;
        let mut rows: Vec<InlineRow> = if posted.contains_key(&format!("{t}-TOTAL")) {
            parse_inline(posted, inl)
        } else {
            let mut q = Query::new(inl.meta);
            q.filter.push(Node::Cmp(inl.fk, "=", Value::Int(parent), FieldType::Int));
            q.order = vec![("id", false)];
            q.limit = Some(200);
            let mut rows: Vec<InlineRow> =
                q.rows().await.map_err(fail)?.into_iter().map(|(cid, v)| (Some(cid), raw_values(inl.meta, &v), false)).collect();
            rows.extend((0..inl.extra).map(|_| (None, HashMap::new(), false)));
            rows
        };
        if rows.is_empty() {
            rows.extend((0..inl.extra).map(|_| (None, HashMap::new(), false))); // always room to add
        }
        // Column headings come from the model, not the first row, so an empty table still has them.
        let columns: Vec<String> = inl.meta.fields.iter().filter(|f| !f.is_auto() && f.name != inl.fk).map(|f| label(f.name)).collect();
        let mut shown = vec![];
        for (i, (cid, raw, delete)) in rows.iter().enumerate() {
            let prefix = format!("{t}-{i}-");
            let row_errors: HashMap<String, String> =
                errors.iter().filter_map(|(k, v)| k.strip_prefix(&prefix).map(|f| (f.to_string(), v.clone()))).collect();
            let fields = form_fields(site, inl.meta, raw, &row_errors, cid.is_none(), &[inl.fk], &prefix, &[]).await?;
            shown.push(context! { prefix, id => cid, fields, delete });
        }
        out.push(context! { table => t, label => title_plural(inl.meta.name), fk => inl.fk, total => rows.len(), rows => shown, columns });
    }
    Ok(out)
}

enum InlineOp {
    Delete(&'static ModelMeta, &'static str, i64),
    Update(&'static ModelMeta, &'static str, i64, Vec<(&'static str, Value, FieldType)>),
    Insert(&'static ModelMeta, Vec<(&'static str, Value, FieldType)>),
}

/// Validate every submitted inline row. Blank new rows are skipped; the foreign key is always
/// the parent's, whatever was posted.
async fn inline_changes(
    site: &Site,
    meta: &'static ModelMeta,
    parent: i64,
    form: &HashMap<String, String>,
) -> (Vec<InlineOp>, HashMap<String, String>) {
    let (mut ops, mut errors) = (vec![], HashMap::new());
    for inl in &options(site, meta).inlines {
        for (i, (cid, mut raw, delete)) in parse_inline(form, inl).into_iter().enumerate() {
            if delete {
                if let Some(cid) = cid {
                    ops.push(InlineOp::Delete(inl.meta, inl.fk, cid));
                }
                continue;
            }
            let blank = inl
                .meta
                .fields
                .iter()
                .filter(|f| f.name != inl.fk && !f.is_auto())
                .all(|f| raw.get(f.name).is_none_or(|v| v.trim().is_empty()));
            if cid.is_none() && blank {
                continue;
            }
            raw.insert(inl.fk.to_string(), parent.to_string());
            match (validate(inl.meta, &raw, cid.is_none(), &[]).await, cid) {
                (Ok(cols), Some(cid)) => ops.push(InlineOp::Update(inl.meta, inl.fk, cid, cols)),
                (Ok(cols), None) => ops.push(InlineOp::Insert(inl.meta, cols)),
                (Err(e), _) => errors.extend(e.into_iter().map(|(k, v)| (format!("{}-{i}-{k}", inl.meta.table), v))),
            }
        }
    }
    (ops, errors)
}

/// Apply validated inline changes (inside the parent's transaction), logging each child.
/// Updates and deletes only reach rows that belong to this parent. Returns what changed.
async fn apply_inlines(u: &User, parent: i64, ops: &[InlineOp]) -> crate::Result<Vec<&'static str>> {
    let mut changed: Vec<&'static str> = vec![];
    let owned = |m: &'static ModelMeta, fk: &'static str, cid: i64| {
        let mut q = Query::new(m);
        q.filter.push(by_id(cid));
        q.filter.push(Node::Cmp(fk, "=", Value::Int(parent), FieldType::Int));
        q
    };
    for op in ops {
        let m = match op {
            InlineOp::Delete(m, fk, cid) => {
                let name = repr(m, *cid, &row_values(m, *cid).await?);
                if owned(m, fk, *cid).delete().await? == 0 {
                    return Err(Error::NotFound);
                }
                log(u, m, *cid, &name, DELETION, String::new()).await?;
                m
            }
            InlineOp::Update(m, fk, cid, cols) => {
                let q = owned(m, fk, *cid);
                if q.update(cols).await? == 0 && q.count().await? == 0 {
                    return Err(Error::NotFound); // not this parent's row
                }
                log(u, m, *cid, &repr(m, *cid, &row_values(m, *cid).await?), CHANGE, "Changed.".into()).await?;
                m
            }
            InlineOp::Insert(m, cols) => {
                let cid = orm::insert_row(m.table, cols).await?;
                log(u, m, cid, &repr(m, cid, &row_values(m, cid).await?), ADDITION, "Added.".into()).await?;
                m
            }
        };
        let name = m.table.split_once('_').map_or(m.table, |(_, n)| n);
        if !changed.contains(&name) {
            changed.push(name);
        }
    }
    Ok(changed)
}

// ---------------------------------------------------------------- delete & history

async fn delete_page(State(site): S, user: CurrentUser, uri: Uri, Path((table, id)): Path<(String, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    visible(&site, meta, u, id).await?;
    allowed(meta.table, "delete")?;
    let obj = repr(meta, id, &fetch_row(meta, id).await?);
    let related = related(&site, meta, &[id]).await?;
    let status = if related.protected.is_empty() { StatusCode::OK } else { StatusCode::CONFLICT };
    let page = render(
        &site,
        "delete.html",
        context! {
            title => "Are you sure?", obj, id, objects => vec![context! { model => capitalize(&verbose(meta.name)), repr => obj.clone(), url => format!("/admin/{table}/{id}/") }],
            protected => related.protected, cascaded => related.cascaded, summary => summary(meta, 1, &related),
            ..chrome(&site, u, Some(meta))
        },
    )?;
    Ok((status, page).into_response())
}

async fn delete_submit(State(site): S, user: CurrentUser, uri: Uri, Path((table, id)): Path<(String, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    visible(&site, meta, u, id).await?;
    allowed(meta.table, "delete")?;
    let obj = repr(meta, id, &fetch_row(meta, id).await?);
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    let deleted = crate::atomic(async {
        q.delete().await?;
        log(u, meta, id, &obj, DELETION, String::new()).await
    })
    .await;
    match deleted {
        Ok(log_id) => Ok(Redirect::to(&format!("/admin/{table}/?log={log_id}")).into_response()),
        // The confirmation page already lists what blocks this; show it again.
        Err(e) if e.is_foreign_key_violation() => delete_page(State(site), user, uri, Path((table, id))).await,
        Err(e) => Err(fail(e)),
    }
}

async fn history(State(site): S, user: CurrentUser, uri: Uri, Path((table, id)): Path<(String, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    visible(&site, meta, u, id).await?;
    let entries = LogEntry::objects()
        .filter(LogEntry::TABLE_NAME.eq(table.as_str()) & LogEntry::OBJECT_ID.eq(id))
        .order_by(LogEntry::ID.asc())
        .all()
        .await
        .map_err(fail)?;
    // A deleted row still has a history (and an undo): name it as the log last did.
    let obj = match row_values(meta, id).await {
        Ok(vals) => repr(meta, id, &vals),
        Err(Error::NotFound) => entries.last().map(|e| e.object_repr.clone()).ok_or_else(|| StatusCode::NOT_FOUND.into_response())?,
        Err(e) => return Err(fail(e)),
    };
    let newest_undoable = entries.iter().rev().find(|e| undoable(e)).and_then(|e| e.id);
    let users = User::in_bulk(entries.iter().map(|e| e.user_id)).await.map_err(fail)?;
    let rows: Vec<_> = entries
        .iter()
        .map(|e| {
            context! {
                at => DateTime::from_unix(e.at).human(),
                user => users.get(&e.user_id).map_or("(deleted user)".to_string(), |u| u.username.clone()),
                message => e.message,
                undo => (e.id == newest_undoable).then(|| format!("/admin/{}/{}/undo/{}", e.table_name, e.object_id, e.id.unwrap_or(0))),
            }
        })
        .collect();
    render(&site, "history.html", context! { title => format!("Change history: {obj}"), obj, id, rows, ..chrome(&site, u, Some(meta)) })
}

/// API and MCP writes can be undone from History (the newest one at a time, so undos never
/// skip over a later change).
fn undoable(e: &LogEntry) -> bool {
    e.message.contains(" via ") && !e.message.starts_with("Undid") && (e.action == ADDITION || e.snapshot.is_some())
}

/// Reverse one API or MCP write: remove what was added, restore what was changed, bring back
/// what was deleted (with its id). Runs in one transaction with its own History entry.
async fn undo(State(site): S, user: CurrentUser, uri: Uri, Path((table, id, entry)): Path<(String, i64, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    visible(&site, meta, u, id).await?;
    allowed(meta.table, "change")?;
    let e = LogEntry::get(entry).await.map_err(fail)?;
    let newest = LogEntry::objects()
        .filter(LogEntry::TABLE_NAME.eq(table.as_str()) & LogEntry::OBJECT_ID.eq(id))
        .order_by(LogEntry::ID.desc())
        .all()
        .await
        .map_err(fail)?
        .into_iter()
        .find(undoable);
    if e.table_name != table || e.object_id != id || newest.and_then(|n| n.id) != Some(entry) {
        return Err(StatusCode::CONFLICT.into_response()); // not this row's newest undoable change
    }
    let restore = |snap: &str| -> crate::Result<Vec<(&'static str, Value, FieldType)>> {
        let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(snap).map_err(|e| Error::Decode(e.to_string()))?;
        meta.fields
            .iter()
            .filter_map(|f| obj.get(f.name).map(|j| (f, j)))
            .map(|(f, j)| {
                let v: Value = serde_json::from_value(j.clone()).map_err(|e| Error::Decode(e.to_string()))?;
                let v = match (f.ty, v) {
                    (FieldType::Encrypted, Value::Text(s)) => Value::Text(crate::crypt::decrypt(&s)?),
                    (_, v) => v,
                };
                Ok((f.name, v, f.ty))
            })
            .collect()
    };
    let what = e.message.trim_end_matches('.').to_string();
    let log_id = crate::atomic(async {
        let mut q = Query::new(meta);
        q.filter.push(by_id(id));
        let (action, verb) = match e.action {
            ADDITION => {
                q.delete().await?;
                (DELETION, "deleted")
            }
            CHANGE => {
                q.update(&restore(e.snapshot.as_deref().unwrap_or("{}"))?).await?;
                (CHANGE, "restored")
            }
            _ => {
                orm::insert_row_with_id(meta.table, id, &restore(e.snapshot.as_deref().unwrap_or("{}"))?).await?;
                (ADDITION, "restored")
            }
        };
        let name = match row_values(meta, id).await {
            Ok(vals) => repr(meta, id, &vals),
            Err(_) => e.object_repr.clone(),
        };
        log(u, meta, id, &name, action, format!("Undid “{what}”: {verb}.")).await
    })
    .await;
    match log_id {
        Ok(log_id) => Ok(Redirect::to(&format!("/admin/{table}/{id}/history?undone={log_id}")).into_response()),
        Err(err) => Err(fail(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(plural("Category"), "Categories");
        assert_eq!(plural("Day"), "Days");
        assert_eq!(plural("Bus"), "Buses");
        assert_eq!(plural("Post"), "Posts");
        assert_eq!(label("author_id"), "Author");
        assert_eq!(label("is_staff"), "Is staff");
        assert_eq!(app_of("blog_post"), "Blog");
        assert_eq!(app_of("rangoli_user"), "Authentication and Authorization");
        assert_eq!(app_of("things"), "Things");
        assert_eq!(
            (verbose("TaskRecord"), title_plural("TaskRecord"), verbose("PostV2")),
            ("task record".into(), "Task records".into(), "post v2".into())
        );
    }

    /// The designer review as a test: every text/background pair in the stylesheet
    /// meets WCAG AA (4.5:1), and control borders meet the 3:1 non-text minimum.
    #[test]
    fn admin_contrast() {
        let css = std::str::from_utf8(ASSETS[0].1).unwrap();
        let tokens = |block: &str| -> HashMap<String, (f64, f64, f64)> {
            block
                .lines()
                .filter_map(|l| {
                    let (name, value) = l.trim().strip_prefix("--")?.split_once(": #")?;
                    let hex = value.trim_end_matches(';');
                    let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok().map(|v| f64::from(v) / 255.0);
                    Some((name.to_string(), (c(0)?, c(2)?, c(4)?)))
                })
                .collect()
        };
        let light = tokens(&css[..css.find("@media (prefers-color-scheme: dark)").unwrap()]);
        let mut dark = light.clone();
        let dark_block = &css[css.find(":root[data-theme=\"dark\"]").unwrap()..];
        dark.extend(tokens(&dark_block[..dark_block.find('}').unwrap()]));

        let lum = |(r, g, b): (f64, f64, f64)| {
            let f = |c: f64| if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
            0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
        };
        let ratio = |a, b| {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };
        let text_pairs = [
            ("text", "surface"),
            ("text", "bg"),
            ("text", "surface-2"),
            ("text", "selected"),
            ("text-2", "surface"),
            ("text-2", "surface-2"),
            ("text-2", "bg"),
            ("brand-text", "brand"),
            ("brand-mark", "brand"),
            ("brand-muted", "brand"),
            ("crumb-text", "crumb-bg"),
            ("link", "surface"),
            ("link", "bg"),
            ("link", "surface-2"),
            ("link", "crumb-bg"),
            ("on-primary", "primary"),
            ("on-primary", "primary-hover"),
            ("on-danger", "danger"),
            ("on-danger", "danger-hover"),
            ("success-text", "success-bg"),
            ("warning-text", "warning-bg"),
            ("error-text", "error-bg"),
            ("error-text", "surface"),
        ];
        let ui_pairs = [("line-strong", "surface"), ("focus", "surface"), ("primary", "surface"), ("error-line", "surface")];
        for (theme, t) in [("light", &light), ("dark", &dark)] {
            for (pairs, min) in [(&text_pairs[..], 4.5), (&ui_pairs[..], 3.0)] {
                for (fg, bg) in pairs {
                    let r = ratio(t[*fg], t[*bg]);
                    assert!(r >= min, "{theme}: --{fg} on --{bg} is {r:.2}:1, needs {min}:1");
                }
            }
        }
    }

    #[test]
    fn names_read_like_english() {
        assert_eq!(super::title_plural("ApiToken"), "API tokens");
        assert_eq!(super::title_plural("TaskRecord"), "Task records");
        assert_eq!(super::server_name("https://blog.example.com/mcp"), "blog");
        assert_eq!(super::server_name("http://127.0.0.1:8000/mcp"), "rangoli");
        assert_eq!(super::server_name("http://localhost:8000/mcp"), "rangoli");
    }

    #[test]
    fn next_stays_inside_admin() {
        assert_eq!(safe_next(Some("/admin/post/")), "/admin/post/");
        assert_eq!(safe_next(Some("https://evil.example")), "/admin/");
        assert_eq!(safe_next(Some("//evil.example/admin/")), "/admin/");
        assert_eq!(safe_next(None), "/admin/");
    }

    #[test]
    fn change_messages_read_like_django() {
        const fn field(name: &'static str, ty: FieldType) -> FieldMeta {
            FieldMeta {
                name,
                ty,
                null: false,
                unique: false,
                password: false,
                private: false,
                fk: None,
                cascade: false,
                auto_now: false,
                auto_now_add: false,
                index: false,
                choices: None,
                default: None,
                help: None,
            }
        }
        static FIELDS: [FieldMeta; 3] =
            [field("title", FieldType::Text), field("body", FieldType::Text), field("author_id", FieldType::Int)];
        let meta = ModelMeta {
            name: "Post",
            table: "blog_post",
            display: None,
            fields: &FIELDS,
            m2m: &[],
            indexes: &[],
            search: &[],
            unique_together: &[],
        };
        let old = [Value::Text("a".into()), Value::Text("b".into()), Value::Int(1)];
        let cols = |t: &str, b: &str, a: i64| {
            vec![
                ("title", Value::Text(t.into()), FieldType::Text),
                ("body", Value::Text(b.into()), FieldType::Text),
                ("author_id", Value::Int(a), FieldType::Int),
            ]
        };
        assert_eq!(change_message(&meta, &old, &cols("a", "b", 1), &[]), "No fields changed.");
        assert_eq!(change_message(&meta, &old, &cols("x", "b", 1), &[]), "Changed title.");
        assert_eq!(change_message(&meta, &old, &cols("x", "y", 2), &[]), "Changed title, body and author.");
        assert_eq!(change_message(&meta, &old, &cols("a", "b", 1), &["tags"]), "Changed tags.");
    }
}

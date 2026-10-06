//! The admin, modeled on Django's: generated from model metadata, server-rendered,
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
use std::marker::PhantomData;
use std::sync::Arc;

const PAGE_SIZE: u64 = 100;

/// One admin action, like Django's `LogEntry`. Powers "Recent actions" and History.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_admin_log")]
pub struct LogEntry {
    pub id: Option<i64>,
    /// Plain id, not a foreign key: history outlives deleted users.
    pub user_id: i64,
    #[field(max_length = 100)]
    pub table_name: String,
    pub object_id: i64,
    #[field(max_length = 200)]
    pub object_repr: String,
    /// 1 added, 2 changed, 3 deleted.
    pub action: i64,
    #[field(text)]
    pub message: String,
    pub at: i64,
}

const ADDITION: i64 = 1;
const CHANGE: i64 = 2;
const DELETION: i64 = 3;

// ---------------------------------------------------------------- ModelAdmin

/// A column of model `M` usable in `ModelAdmin` settings: `Post::TITLE`, `Post::ID`, ...
pub trait Field<M> {
    fn name(&self) -> &'static str;
}

impl<M, T> Field<M> for crate::orm::Col<M, T> {
    fn name(&self) -> &'static str {
        crate::orm::Col::name(*self)
    }
}

/// Per-model admin settings, like Django's `ModelAdmin`. Columns are typed, so a
/// misspelled field or one from another model does not compile.
///
/// ```ignore
/// App::new().admin_with::<Post>(
///     ModelAdmin::new()
///         .list_display([&Post::TITLE, &Post::AUTHOR_ID, &Post::PUBLISHED])
///         .search_fields([&Post::TITLE, &Post::BODY])
///         .list_filter([&Post::PUBLISHED, &Post::AUTHOR_ID])
///         .ordering(Post::ID.desc())
///         .readonly_fields([&Post::RATING]),
/// )
/// ```
pub struct ModelAdmin<M> {
    pub(crate) opts: Options,
    _m: PhantomData<fn() -> M>,
}

/// The settings `ModelAdmin` collects, with the model type erased.
#[derive(Clone, Debug, Default)]
pub struct Options {
    list_display: Option<Vec<&'static str>>,
    search_fields: Option<Vec<&'static str>>,
    list_filter: Option<Vec<&'static str>>,
    ordering: Option<(&'static str, bool)>,
    readonly_fields: Vec<&'static str>,
    list_per_page: Option<u64>,
}

impl<M: Model> Default for ModelAdmin<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Model> ModelAdmin<M> {
    pub fn new() -> Self {
        ModelAdmin { opts: Options::default(), _m: PhantomData }
    }

    fn names<const N: usize>(cols: [&dyn Field<M>; N]) -> Vec<&'static str> {
        cols.iter().map(|c| c.name()).collect()
    }

    /// Changelist columns, in order.
    pub fn list_display<const N: usize>(mut self, cols: [&dyn Field<M>; N]) -> Self {
        self.opts.list_display = Some(Self::names(cols));
        self
    }

    /// Text columns searched by the search box.
    pub fn search_fields<const N: usize>(mut self, cols: [&dyn Field<M>; N]) -> Self {
        self.opts.search_fields = Some(Self::names(cols));
        self
    }

    /// Sidebar filters: boolean, date and foreign key columns.
    pub fn list_filter<const N: usize>(mut self, cols: [&dyn Field<M>; N]) -> Self {
        self.opts.list_filter = Some(Self::names(cols));
        self
    }

    /// Default changelist order, e.g. `Post::CREATED_AT.desc()`.
    pub fn ordering(mut self, order: crate::orm::Order<M>) -> Self {
        self.opts.ordering = Some((order.0, order.1));
        self
    }

    /// Shown on the change form but not editable.
    pub fn readonly_fields<const N: usize>(mut self, cols: [&dyn Field<M>; N]) -> Self {
        self.opts.readonly_fields = Self::names(cols);
        self
    }

    pub fn list_per_page(mut self, n: u64) -> Self {
        self.opts.list_per_page = Some(n.max(1));
        self
    }

    /// Reject settings that can't work, at startup (Django's admin checks).
    pub(crate) fn check(&self) -> std::result::Result<(), String> {
        let meta = M::meta();
        let field = |n: &str| meta.field(n);
        for n in self.opts.search_fields.iter().flatten() {
            if !field(n).is_some_and(|f| matches!(f.ty, FieldType::Varchar(_) | FieldType::Text) && !f.password) {
                return Err(format!("{}: search_fields can only use text columns, not `{n}`", meta.name));
            }
        }
        for n in self.opts.list_filter.iter().flatten() {
            if !field(n).is_some_and(|f| matches!(f.ty, FieldType::Bool | FieldType::DateTime) || f.fk.is_some() || f.choices.is_some()) {
                return Err(format!("{}: list_filter supports boolean, date, choice and foreign key columns, not `{n}`", meta.name));
            }
        }
        Ok(())
    }
}

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
        ("base.html", include_str!("admin/templates/base.html")),
        ("login.html", include_str!("admin/templates/login.html")),
        ("logged_out.html", include_str!("admin/templates/logged_out.html")),
        ("index.html", include_str!("admin/templates/index.html")),
        ("list.html", include_str!("admin/templates/list.html")),
        ("form.html", include_str!("admin/templates/form.html")),
        ("delete.html", include_str!("admin/templates/delete.html")),
        ("history.html", include_str!("admin/templates/history.html")),
        ("password_change.html", include_str!("admin/templates/password_change.html")),
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
        .route("/admin/static/{file}", get(static_file))
        .route("/admin/{table}/", get(list).post(bulk))
        .route("/admin/{table}/add", get(add_page).post(add_submit))
        .route("/admin/{table}/{id}/", get(change_page).post(change_submit))
        .route("/admin/{table}/{id}/delete", get(delete_page).post(delete_submit))
        .route("/admin/{table}/{id}/history", get(history))
        .with_state(Arc::new(Site { admin, options, models, env, version }))
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
    site.admin.iter().copied().find(|m| m.table == table).ok_or_else(|| StatusCode::NOT_FOUND.into_response())
}

fn options<'a>(site: &'a Site, meta: &ModelMeta) -> &'a Options {
    static DEFAULT: std::sync::OnceLock<Options> = std::sync::OnceLock::new();
    site.options.get(meta.table).unwrap_or_else(|| DEFAULT.get_or_init(Options::default))
}

fn plural(name: &str) -> String {
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
    for m in &site.admin {
        let app = app_of(m.table);
        let entry = context! { table => m.table, plural => plural(m.name), current => current == Some(m.table) };
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
        apps => apps(site, current.map(|m| m.table)),
        app => current.map(|m| app_of(m.table)),
        table => current.map(|m| m.table),
        name => current.map(|m| m.name.to_lowercase()),
        plural => current.map(|m| plural(m.name)),
    }
}

fn display(v: &Value, f: &FieldMeta) -> String {
    match v {
        Value::Null => "-".into(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).into(),
        Value::Int(i) if f.ty == FieldType::DateTime => DateTime::from_unix(*i).human(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Text(s) if f.choices.is_some() => f.choices.unwrap().iter().find(|(v, _)| v == s).map_or(s.clone(), |(_, l)| l.to_string()),
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
    };
    e.save().await?;
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
    render(&site, "login.html", context! { title => "Log in", next }).unwrap_or_else(|e| e)
}

/// Only redirect back into the admin, never to another site.
fn safe_next(next: Option<&str>) -> &str {
    next.filter(|n| n.starts_with("/admin/") && !n.starts_with("//")).unwrap_or("/admin/")
}

async fn login_submit(State(site): S, Form(f): Form<Pairs>) -> Response {
    let (username, password) = (param(&f, "username").unwrap_or(""), param(&f, "password").unwrap_or(""));
    let next = safe_next(param(&f, "next"));
    let error = match auth::authenticate(username, password).await {
        Ok(Some(u)) if u.is_staff => match auth::login(&u).await {
            Ok(cookie) => return ([(header::SET_COOKIE, cookie)], Redirect::to(next)).into_response(),
            Err(e) => return fail(e),
        },
        Ok(_) => {
            "Please enter the correct username and password for a staff account. Note that both fields may be case-sensitive.".to_string()
        }
        Err(Error::Locked) => Error::Locked.to_string(),
        Err(e) => return fail(e),
    };
    let page = render(&site, "login.html", context! { title => "Log in", next, error, username });
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
    changed.password = auth::hash_password(new1).await;
    changed.save().await.map_err(fail)?;
    render(&site, "password_change.html", context! { title => "Password change successful", done => true, ..chrome(&site, u, None) })
}

/// Admin assets, compiled into the binary.
const ASSETS: &[(&str, &[u8], &str)] = &[
    ("admin.css", include_bytes!("admin/static/admin.css"), "text/css; charset=utf-8"),
    ("admin.js", include_bytes!("admin/static/admin.js"), "text/javascript; charset=utf-8"),
    ("htmx.min.js", include_bytes!("admin/static/htmx.min.js"), "text/javascript; charset=utf-8"),
    ("icon-yes.svg", include_bytes!("admin/static/icon-yes.svg"), "image/svg+xml"),
    ("icon-no.svg", include_bytes!("admin/static/icon-no.svg"), "image/svg+xml"),
    ("icon-addlink.svg", include_bytes!("admin/static/icon-addlink.svg"), "image/svg+xml"),
    ("icon-changelink.svg", include_bytes!("admin/static/icon-changelink.svg"), "image/svg+xml"),
    ("icon-deletelink.svg", include_bytes!("admin/static/icon-deletelink.svg"), "image/svg+xml"),
    ("search.svg", include_bytes!("admin/static/search.svg"), "image/svg+xml"),
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
                model => meta.map_or(e.table_name.clone(), |m| m.name.to_string()),
            }
        })
        .collect();
    render(&site, "index.html", context! { title => "Site administration", recent, ..chrome(&site, u, None) })
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
        None => meta.fields.iter().filter(|f| matches!(f.ty, FieldType::Bool | FieldType::DateTime) || f.choices.is_some()).collect(),
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

    let mut query = Query::new(meta);
    if !q.is_empty() && !searchable.is_empty() {
        let pat = format!("%{}%", like_escape(&q));
        query.filter.push(Node::Or(searchable.iter().map(|c| Node::Like(c, pat.clone())).collect()));
    }
    for f in &filters {
        let Some(v) = param(&p, &format!("f.{}", f.name)) else { continue };
        let node = match f.ty {
            FieldType::Bool if v == "1" || v == "0" => Node::Cmp(f.name, "=", Value::Bool(v == "1"), FieldType::Bool),
            FieldType::DateTime => match date_range_start(v) {
                Some(since) => Node::Cmp(f.name, ">=", Value::Int(since.unix()), FieldType::DateTime),
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
    let filtered = !query.filter.is_empty();
    let count = query.count().await.map_err(fail)?;
    let full_count = if filtered { Query::new(meta).count().await.map_err(fail)? } else { count };
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
            .filter(|(k, _)| !matches!(k.as_str(), "p" | "log" | "deleted" | "what" | "warn") && !set.iter().any(|(s, _)| s == k))
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
            (FieldType::DateTime, _) => DATE_RANGES.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
            (FieldType::Varchar(_), _) if f.choices.is_some() => std::iter::once((String::new(), "All".to_string()))
                .chain(f.choices.unwrap().iter().map(|(v, l)| (v.to_string(), l.to_string())))
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
    let page_links: Vec<_> = (1..=pages)
        .filter(|n| *n == 1 || *n == pages || n.abs_diff(page) <= 3)
        .map(|n| context! { n, url => link(&[("p", &n.to_string())]), current => n == page })
        .collect();
    let what = if count == 1 { meta.name.to_lowercase() } else { plural(meta.name).to_lowercase() };
    render(
        &site,
        "list.html",
        context! {
            title => format!("Select {} to change", meta.name.to_lowercase()),
            messages => message(&p, u, "list").await, q, keep, count, full_count, filtered, what, page_links,
            columns => headers, rows, filters => filter_blocks, searchable => !searchable.is_empty(), clear_url => base.clone(),
            ..chrome(&site, u, Some(meta))
        },
    )
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
    let rows = q.rows().await.map_err(fail)?;
    if param(&f, "post") != Some("yes") {
        let related = related(&site, meta, &ids).await?;
        let objects: Vec<_> = rows
            .iter()
            .map(|(id, v)| context! { model => meta.name, repr => repr(meta, *id, v), url => format!("/admin/{table}/{id}/") })
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
    let what = if rows.len() == 1 { meta.name.to_lowercase() } else { plural(meta.name).to_lowercase() };
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
            let labels: Vec<String> = rows.iter().map(|(id, v)| format!("{}: {}", m.name, repr(m, *id, v))).collect();
            if !f.cascade {
                out.protected.extend(labels);
            } else if site.admin.iter().any(|a| a.table == m.table) {
                // Framework bookkeeping (sessions) cascades silently, as Django's would.
                out.counts.push((plural(m.name), rows.len()));
                out.cascaded.extend(labels.into_iter().map(|l| context! { label => l }));
            }
        }
    }
    Ok(out)
}

fn summary(meta: &ModelMeta, n: usize, r: &Related) -> Vec<String> {
    std::iter::once(format!("{}: {n}", plural(meta.name))).chain(r.counts.iter().map(|(p, c)| format!("{p}: {c}"))).collect()
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
                Value::Int(i) => i.to_string(),
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
        None => q.limit = Some(1000), // ponytail: plain <select>; switch to an autocomplete endpoint for huge tables
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

async fn form_fields(
    site: &Site,
    meta: &ModelMeta,
    raw: &HashMap<String, String>,
    errors: &HashMap<String, String>,
    is_add: bool,
    readonly: &[&str],
) -> Result<Vec<minijinja::Value>, Response> {
    let mut out = vec![];
    for f in meta.fields.iter().filter(|f| !f.is_auto()) {
        let options: Vec<(String, String)> = match (f.fk, f.choices) {
            (Some(t), _) => label_rows(site, t, None).await?.into_iter().map(|(id, l)| (id.to_string(), l)).collect(),
            (_, Some(choices)) => choices.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
            _ => vec![],
        };
        if readonly.contains(&f.name) {
            let raw = raw.get(f.name).cloned().unwrap_or_default();
            let shown = match (f.ty, raw.as_str()) {
                (_, "") if f.ty != FieldType::Bool => "-".to_string(),
                (FieldType::Bool, v) => (if v.is_empty() { "No" } else { "Yes" }).to_string(),
                (FieldType::DateTime, v) => DateTime::parse(v).map_or(v.to_string(), DateTime::human),
                _ => options.iter().find(|(id, _)| *id == raw).map_or(raw.clone(), |(_, l)| l.clone()),
            };
            out.push(context! { name => f.name, label => label(f.name), kind => "readonly", value => shown });
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
            _ => "text",
        };
        let value = raw.get(f.name).cloned().unwrap_or_default();
        out.push(context! {
            name => f.name, label => label(f.name), kind, checked => !value.is_empty(), value, options,
            required => !f.null && f.ty != FieldType::Bool && (is_add || !f.password),
            maxlength => match f.ty { FieldType::Varchar(n) => Some(n), _ => None },
            help => if f.password && !is_add { Some("Leave blank to keep the current password.") } else if f.ty == FieldType::DateTime { Some("Date and time in UTC.") } else { None },
            error => errors.get(f.name),
        });
    }
    Ok(out)
}

/// Validate a submitted form against the model; password fields come back hashed.
pub(crate) async fn validate(
    meta: &'static ModelMeta,
    form: &HashMap<String, String>,
    is_add: bool,
    readonly: &[&str],
) -> Result<Vec<(&'static str, Value, FieldType)>, HashMap<String, String>> {
    let (mut cols, mut errors) = (vec![], HashMap::new());
    for f in meta.fields {
        if f.is_auto() {
            if f.auto_now || is_add {
                cols.push((f.name, Value::Int(DateTime::now().unix()), f.ty));
            }
            continue;
        }
        // Read-only fields ignore whatever was posted; new rows get NULL or the type's zero value.
        if readonly.contains(&f.name) {
            if is_add {
                cols.push((f.name, if f.null { Value::Null } else { crate::migrate::zero(f.ty) }, f.ty));
            }
            continue;
        }
        let raw = form.get(f.name).map(|s| s.trim()).unwrap_or("");
        let v = if f.ty == FieldType::Bool {
            Ok(Value::Bool(!raw.is_empty()))
        } else if raw.is_empty() {
            if f.password && !is_add {
                continue; // keep the existing hash
            }
            if f.null {
                Ok(Value::Null)
            } else {
                Err("This field is required.".to_string())
            }
        } else {
            match f.ty {
                FieldType::Int => raw.parse().map(Value::Int).map_err(|_| "Enter a whole number.".to_string()),
                FieldType::DateTime => DateTime::parse(raw).map(Value::from).ok_or("Enter a valid date and time.".to_string()),
                FieldType::Json => serde_json::from_str::<serde_json::Value>(raw)
                    .map(|j| Value::Text(j.to_string()))
                    .map_err(|e| format!("Enter valid JSON ({e}).")),
                FieldType::Varchar(_) if f.choices.is_some_and(|c| !c.iter().any(|(v, _)| *v == raw)) => {
                    Err(format!("Select a valid choice. {raw} is not one of the available choices."))
                }
                FieldType::Float => {
                    raw.parse::<f64>().ok().filter(|x| x.is_finite()).map(Value::Float).ok_or("Enter a number.".to_string())
                }
                FieldType::Varchar(n) if raw.chars().count() > n as usize => {
                    Err(format!("Ensure this value has at most {n} characters (it has {}).", raw.chars().count()))
                }
                FieldType::Text | FieldType::Varchar(_) if f.password => Ok(Value::Text(auth::hash_password(raw).await)),
                _ => Ok(Value::Text(raw.to_string())),
            }
        };
        match v {
            Ok(v) => cols.push((f.name, v, f.ty)),
            Err(e) => {
                errors.insert(f.name.to_string(), e);
            }
        }
    }
    if errors.is_empty() {
        Ok(cols)
    } else {
        Err(errors)
    }
}

fn db_error_message(e: &Error) -> Option<&'static str> {
    if e.is_unique_violation() {
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
    let mut fields = form_fields(site, meta, raw, &errors, is_add, &options(site, meta).readonly_fields).await?;
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
    let title = if is_add { format!("Add {}", meta.name.to_lowercase()) } else { format!("Change {}", meta.name.to_lowercase()) };
    let page = render(
        site,
        "form.html",
        context! {
            title, obj, id, is_add, fields, messages, form_error => errors.get("__all__"),
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
    let meta = model(&site, &table)?;
    // New forms start from the model's defaults, like Django's `initial`.
    let defaults: Vec<Value> = meta.fields.iter().map(|f| f.default.map_or(Value::Null, |d| d.value())).collect();
    form_page(&site, u, meta, None, &raw_values(meta, &defaults), &Links::new(), HashMap::new(), StatusCode::OK, messages).await
}

async fn add_submit(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, Form(pairs): Form<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let form: HashMap<String, String> = pairs.iter().cloned().collect();
    let links = submitted_links(meta, &pairs);
    let errors = match validate(meta, &form, true, &options(&site, meta).readonly_fields).await {
        Ok(cols) => match crate::atomic(async {
            let id = orm::insert_row(meta.table, &cols).await?;
            save_links(meta, id, &links).await?;
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
    let form: HashMap<String, String> = pairs.iter().cloned().collect();
    let links = submitted_links(meta, &pairs);
    let old = fetch_row(meta, id).await?;
    let old_links = current_links(meta, id).await.map_err(fail)?;
    let errors = match validate(meta, &form, false, &options(&site, meta).readonly_fields).await {
        Ok(cols) => {
            let mut q = Query::new(meta);
            q.filter.push(by_id(id));
            let saved = crate::atomic(async {
                q.update(&cols).await?;
                save_links(meta, id, &links).await?;
                let vals = row_values(meta, id).await?;
                let sorted = |l: &Links, n: &str| {
                    let mut v = l.get(n).cloned().unwrap_or_default();
                    v.sort_unstable();
                    v.dedup();
                    v
                };
                let rels: Vec<&str> = meta.m2m.iter().map(|r| r.name).filter(|n| sorted(&old_links, n) != sorted(&links, n)).collect();
                log(u, meta, id, &repr(meta, id, &vals), CHANGE, change_message(meta, &old, &cols, &rels)).await
            })
            .await;
            match saved {
                Ok(log_id) => return Ok(after_save(&table, id, log_id, &form)),
                Err(e) => [("__all__".to_string(), db_error_message(&e).ok_or_else(|| fail(e))?.to_string())].into(),
            }
        }
        Err(errors) => errors,
    };
    form_page(&site, u, meta, Some(id), &form, &links, errors, StatusCode::UNPROCESSABLE_ENTITY, None).await
}

// ---------------------------------------------------------------- delete & history

async fn delete_page(State(site): S, user: CurrentUser, uri: Uri, Path((table, id)): Path<(String, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let obj = repr(meta, id, &fetch_row(meta, id).await?);
    let related = related(&site, meta, &[id]).await?;
    let status = if related.protected.is_empty() { StatusCode::OK } else { StatusCode::CONFLICT };
    let page = render(
        &site,
        "delete.html",
        context! {
            title => "Are you sure?", obj, id, objects => vec![context! { model => meta.name, repr => obj.clone(), url => format!("/admin/{table}/{id}/") }],
            protected => related.protected, cascaded => related.cascaded, summary => summary(meta, 1, &related),
            ..chrome(&site, u, Some(meta))
        },
    )?;
    Ok((status, page).into_response())
}

async fn delete_submit(State(site): S, user: CurrentUser, uri: Uri, Path((table, id)): Path<(String, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
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
    let obj = repr(meta, id, &fetch_row(meta, id).await?);
    let entries = LogEntry::objects()
        .filter(LogEntry::TABLE_NAME.eq(table.as_str()) & LogEntry::OBJECT_ID.eq(id))
        .order_by(LogEntry::ID.asc())
        .all()
        .await
        .map_err(fail)?;
    let users = User::in_bulk(entries.iter().map(|e| e.user_id)).await.map_err(fail)?;
    let rows: Vec<_> = entries
        .iter()
        .map(|e| {
            context! {
                at => DateTime::from_unix(e.at).human(),
                user => users.get(&e.user_id).map_or("(deleted user)".to_string(), |u| u.username.clone()),
                message => e.message,
            }
        })
        .collect();
    render(&site, "history.html", context! { title => format!("Change history: {obj}"), obj, id, rows, ..chrome(&site, u, Some(meta)) })
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
    fn admin_checks_reject_unusable_settings() {
        assert!(ModelAdmin::<User>::new().search_fields([&User::USERNAME]).list_filter([&User::IS_STAFF]).check().is_ok());
        let err = ModelAdmin::<User>::new().search_fields([&User::PASSWORD]).check().unwrap_err();
        assert!(err.contains("search_fields"), "{err}");
        let err = ModelAdmin::<User>::new().list_filter([&User::USERNAME]).check().unwrap_err();
        assert!(err.contains("list_filter"), "{err}");
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
                fk: None,
                cascade: false,
                auto_now: false,
                auto_now_add: false,
                index: false,
                choices: None,
                default: None,
            }
        }
        static FIELDS: [FieldMeta; 3] =
            [field("title", FieldType::Text), field("body", FieldType::Text), field("author_id", FieldType::Int)];
        let meta = ModelMeta { name: "Post", table: "blog_post", display: None, fields: &FIELDS, m2m: &[] };
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

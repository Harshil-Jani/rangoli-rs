//! The admin: generated from model metadata, server-rendered, HTMX-enhanced.
//! Works without JavaScript; with it, `hx-boost` turns navigation into
//! partial swaps and search filters live.

// Handlers short-circuit with ready-made responses; boxing them buys nothing here.
#![allow(clippy::result_large_err)]

use crate::auth::{self, CurrentUser};
use crate::orm::{self, by_id, like_escape, FieldMeta, FieldType, ModelMeta, Node, Query, Value};
use crate::Error;
use axum::extract::{Form, Path, Query as UrlQuery, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use minijinja::{context, Environment};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

const PAGE_SIZE: u64 = 50;

struct Site {
    admin: Vec<&'static ModelMeta>,
    models: Vec<&'static ModelMeta>,
    env: Environment<'static>,
}

type S = State<Arc<Site>>;
type Pairs = Vec<(String, String)>;

pub fn router(admin: Vec<&'static ModelMeta>, models: Vec<&'static ModelMeta>) -> Router {
    let mut env = Environment::new();
    for (name, src) in [
        ("base.html", include_str!("admin/templates/base.html")),
        ("login.html", include_str!("admin/templates/login.html")),
        ("index.html", include_str!("admin/templates/index.html")),
        ("list.html", include_str!("admin/templates/list.html")),
        ("form.html", include_str!("admin/templates/form.html")),
        ("delete.html", include_str!("admin/templates/delete.html")),
    ] {
        env.add_template(name, src).expect("admin template");
    }
    Router::new()
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(index))
        .route("/admin/login", get(login_page).post(login_submit))
        .route("/admin/logout", post(logout))
        .route("/admin/static/{file}", get(static_file))
        .route("/admin/{table}/", get(list).post(bulk))
        .route("/admin/{table}/add", get(add_page).post(add_submit))
        .route("/admin/{table}/{id}/", get(change_page).post(change_submit))
        .route("/admin/{table}/{id}/delete", get(delete_page).post(delete_submit))
        .with_state(Arc::new(Site { admin, models, env }))
}

// ---------------------------------------------------------------- helpers

/// Early-return a ready response from a handler.
type Page = Result<Response, Response>;

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

fn staff<'a>(user: &'a CurrentUser, uri: &Uri) -> Result<&'a auth::User, Response> {
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

fn plural(name: &str) -> String {
    match name.strip_suffix('y') {
        Some(stem) if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) => format!("{stem}ies"),
        _ if name.ends_with('s') => format!("{name}es"),
        _ => format!("{name}s"),
    }
}

fn label(field: &str) -> String {
    let s = field.strip_suffix("_id").unwrap_or(field).replace('_', " ");
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn display(v: &Value, f: &FieldMeta) -> String {
    match v {
        Value::Null => "-".into(),
        Value::Bool(b) => (if *b { "Yes" } else { "No" }).into(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Text(s) if f.ty == FieldType::Text && s.chars().count() > 80 => s.chars().take(80).chain("…".chars()).collect(),
        Value::Text(s) => s.clone(),
    }
}

fn param<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Label for one row: its `display` field, else `Name #id`.
async fn object_label(meta: &'static ModelMeta, id: i64) -> Result<String, Response> {
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    let (_, vals) = q.rows().await.map_err(fail)?.pop().ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    Ok(meta
        .display
        .and_then(|d| meta.fields.iter().position(|f| f.name == d))
        .map(|i| display(&vals[i], &meta.fields[i]))
        .unwrap_or_else(|| format!("{} #{id}", meta.name)))
}

#[derive(Serialize)]
struct Crumb {
    label: String,
    url: Option<String>,
}

fn crumbs(meta: &ModelMeta, last: Option<&str>) -> Vec<Crumb> {
    let mut v = vec![Crumb { label: plural(meta.name), url: last.map(|_| format!("/admin/{}/", meta.table)) }];
    if let Some(l) = last {
        v.push(Crumb { label: l.into(), url: None });
    }
    v
}

fn flash(code: Option<&str>) -> Option<minijinja::Value> {
    let (kind, text) = match code? {
        "saved" => ("ok", "Saved."),
        "deleted" => ("ok", "Deleted."),
        "protected" => ("error", "Some rows were not deleted because other records still reference them."),
        _ => return None,
    };
    Some(context! { kind, text })
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
        Ok(_) => "Please enter the correct username and password for a staff account. Both fields may be case-sensitive.".to_string(),
        Err(Error::Locked) => Error::Locked.to_string(),
        Err(e) => return fail(e),
    };
    let page = render(&site, "login.html", context! { title => "Log in", next, error, username });
    (StatusCode::UNAUTHORIZED, page.unwrap_or_else(|e| e)).into_response()
}

async fn logout(headers: HeaderMap) -> Response {
    let cookie = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    match auth::logout(cookie).await {
        Ok(clear) => ([(header::SET_COOKIE, clear)], Redirect::to("/admin/login")).into_response(),
        Err(e) => fail(e),
    }
}

async fn static_file(Path(file): Path<String>) -> Response {
    let (body, ty): (&'static [u8], &str) = match file.as_str() {
        "admin.css" => (include_bytes!("admin/static/admin.css"), "text/css; charset=utf-8"),
        "htmx.min.js" => (include_bytes!("admin/static/htmx.min.js"), "text/javascript; charset=utf-8"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, ty), (header::CACHE_CONTROL, "public, max-age=3600")], body).into_response()
}

// ---------------------------------------------------------------- index & list

async fn index(State(site): S, user: CurrentUser, uri: Uri) -> Page {
    let u = staff(&user, &uri)?;
    let models: Vec<_> = site.admin.iter().map(|m| context! { table => m.table, plural => plural(m.name) }).collect();
    render(&site, "index.html", context! { title => "Site administration", username => u.username, models })
}

async fn list(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, UrlQuery(p): UrlQuery<Pairs>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let base = format!("/admin/{table}/");
    let q = param(&p, "q").unwrap_or("").trim().to_string();
    let page: u64 = param(&p, "p").and_then(|v| v.parse().ok()).filter(|n| *n >= 1).unwrap_or(1);

    let (desc, oname) = match param(&p, "o").unwrap_or("-id") {
        o if o.starts_with('-') => (true, &o[1..]),
        o => (false, o),
    };
    // Only real, visible columns can be sorted on: the name comes from metadata, never from the URL.
    let order_field: &'static str = meta.field(oname).filter(|f| !f.password).map_or("id", |f| f.name);

    let shown: Vec<(usize, &FieldMeta)> =
        meta.fields.iter().enumerate().filter(|(_, f)| !f.password && f.ty != FieldType::Text).take(6).collect();
    let searchable: Vec<&'static str> =
        meta.fields.iter().filter(|f| !f.password && matches!(f.ty, FieldType::Varchar(_) | FieldType::Text)).map(|f| f.name).collect();
    let bools: Vec<&'static FieldMeta> = meta.fields.iter().filter(|f| f.ty == FieldType::Bool).collect();

    let mut query = Query::new(meta);
    if !q.is_empty() && !searchable.is_empty() {
        let pat = format!("%{}%", like_escape(&q));
        query.filter.push(Node::Or(searchable.iter().map(|c| Node::Like(c, pat.clone())).collect()));
    }
    for f in &bools {
        if let Some(v @ ("1" | "0")) = param(&p, &format!("f.{}", f.name)) {
            query.filter.push(Node::Cmp(f.name, "=", Value::Bool(v == "1"), FieldType::Bool));
        }
    }
    let total = query.count().await.map_err(fail)?;
    let pages = (total as u64).div_ceil(PAGE_SIZE).max(1);
    query.order = vec![(order_field, desc)];
    query.limit = Some(PAGE_SIZE);
    query.offset = Some((page.min(pages) - 1) * PAGE_SIZE);
    let raw_rows = query.rows().await.map_err(fail)?;
    // Foreign keys show their target's label: one IN query per FK column, not one per row.
    let mut fk_labels: HashMap<usize, HashMap<i64, String>> = HashMap::new();
    for (i, f) in shown.iter().filter(|(_, f)| f.fk.is_some()) {
        let ids = raw_rows.iter().filter_map(|(_, v)| match v[*i] {
            Value::Int(id) => Some(id),
            _ => None,
        });
        fk_labels.insert(*i, labels(&site, f.fk.unwrap(), ids.collect()).await?);
    }
    let rows: Vec<_> = raw_rows
        .into_iter()
        .map(|(id, vals)| {
            let cells: Vec<String> = shown
                .iter()
                .map(|(i, f)| match (&vals[*i], fk_labels.get(i)) {
                    (Value::Int(fid), Some(names)) => names.get(fid).cloned().unwrap_or_else(|| fid.to_string()),
                    (v, _) => display(v, f),
                })
                .collect();
            context! { id, cells }
        })
        .collect();

    // Links keep every current parameter except the ones they change.
    let link = |set: &[(&str, &str)]| {
        let mut pairs: Vec<(String, String)> = p.iter().filter(|(k, _)| k != "p" && !set.iter().any(|(s, _)| s == k)).cloned().collect();
        pairs.extend(set.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.to_string(), v.to_string())));
        match serde_urlencoded::to_string(&pairs).unwrap() {
            qs if qs.is_empty() => base.clone(),
            qs => format!("{base}?{qs}"),
        }
    };
    let sort = |name: &str| {
        let active = order_field == name;
        let next = if active && !desc { format!("-{name}") } else { name.to_string() };
        context! { url => link(&[("o", &next)]), arrow => if !active { "" } else if desc { " ▼" } else { " ▲" } }
    };
    let columns: Vec<_> = shown.iter().map(|(_, f)| context! { label => label(f.name), ..sort(f.name) }).collect();
    let filters: Vec<_> = bools
        .iter()
        .map(|f| {
            let key = format!("f.{}", f.name);
            let current = param(&p, &key).unwrap_or("");
            let opts: Vec<_> = [("All", ""), ("Yes", "1"), ("No", "0")]
                .iter()
                .map(|(l, v)| context! { label => l, url => link(&[(&key, v)]), active => current == *v })
                .collect();
            context! { label => label(f.name), options => opts }
        })
        .collect();
    let keep: Vec<(String, String)> = p.iter().filter(|(k, _)| k != "q" && k != "p").cloned().collect();
    let (prev_url, next_url) =
        ((page > 1).then(|| link(&[("p", &(page - 1).to_string())])), (page < pages).then(|| link(&[("p", &(page + 1).to_string())])));
    render(
        &site,
        "list.html",
        context! {
            title => plural(meta.name), username => u.username, crumbs => crumbs(meta, None), msg => flash(param(&p, "msg")),
            table, name => meta.name, plural => plural(meta.name), q, keep, total, page => page.min(pages), pages,
            prev_url, next_url, columns, id_sort => sort("id"), rows, filters, searchable => !searchable.is_empty(),
        },
    )
}

/// Bulk action from the list's checkboxes. Django's "delete selected".
async fn bulk(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>, Form(f): Form<Pairs>) -> Page {
    staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let ids: Vec<Value> = f.iter().filter(|(k, _)| k == "ids").filter_map(|(_, v)| v.parse().ok().map(Value::Int)).collect();
    let mut msg = "deleted";
    if param(&f, "action") == Some("delete") && !ids.is_empty() {
        let mut q = Query::new(meta);
        q.filter.push(Node::In("id", ids, FieldType::Int));
        match q.delete().await {
            Ok(_) => {}
            Err(e) if e.is_foreign_key_violation() => msg = "protected",
            Err(e) => return Err(fail(e)),
        }
    }
    Ok(Redirect::to(&format!("/admin/{table}/?msg={msg}")).into_response())
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
                Value::Int(i) => i.to_string(),
                Value::Float(x) => x.to_string(),
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
                    _ => format!("{} #{id}", target.name),
                },
            )
        })
        .collect())
}

async fn fk_options(site: &Site, table: &str) -> Result<Vec<(String, String)>, Response> {
    Ok(label_rows(site, table, None).await?.into_iter().map(|(id, l)| (id.to_string(), l)).collect())
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
) -> Result<Vec<minijinja::Value>, Response> {
    let mut out = vec![];
    for f in meta.fields {
        let options = match f.fk {
            Some(t) => fk_options(site, t).await?,
            None => vec![],
        };
        let kind = match (f.fk.is_some(), f.password, f.ty) {
            (true, ..) => "select",
            (_, true, _) => "password",
            (_, _, FieldType::Bool) => "checkbox",
            (_, _, FieldType::Int) => "number",
            (_, _, FieldType::Float) => "float",
            (_, _, FieldType::Text) => "textarea",
            _ => "text",
        };
        let value = raw.get(f.name).cloned().unwrap_or_default();
        out.push(context! {
            name => f.name, label => label(f.name), kind, checked => !value.is_empty(), value, options,
            required => !f.null && f.ty != FieldType::Bool && (is_add || !f.password),
            maxlength => match f.ty { FieldType::Varchar(n) => Some(n), _ => None },
            help => (f.password && !is_add).then_some("Leave blank to keep the current password."),
            error => errors.get(f.name),
        });
    }
    Ok(out)
}

/// Validate a submitted form against the model; password fields come back hashed.
async fn validate(
    meta: &'static ModelMeta,
    form: &HashMap<String, String>,
    is_add: bool,
) -> Result<Vec<(&'static str, Value, FieldType)>, HashMap<String, String>> {
    let (mut cols, mut errors) = (vec![], HashMap::new());
    for f in meta.fields {
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
                FieldType::Float => {
                    raw.parse::<f64>().ok().filter(|x| x.is_finite()).map(Value::Float).ok_or("Enter a number.".to_string())
                }
                FieldType::Varchar(n) if raw.chars().count() > n as usize => Err(format!("Ensure this value has at most {n} characters.")),
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
        Some("A selected related record does not exist.")
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
async fn form_page(
    site: &Site,
    username: &str,
    meta: &'static ModelMeta,
    id: Option<i64>,
    raw: &HashMap<String, String>,
    errors: HashMap<String, String>,
    status: StatusCode,
) -> Page {
    let is_add = id.is_none();
    let fields = form_fields(site, meta, raw, &errors, is_add).await?;
    let obj = match id {
        Some(id) => object_label(meta, id).await?,
        None => format!("Add {}", meta.name.to_lowercase()),
    };
    let heading = if is_add { obj.clone() } else { format!("Change {}", meta.name.to_lowercase()) };
    let page = render(
        site,
        "form.html",
        context! {
            title => heading.clone(), heading, username, crumbs => crumbs(meta, Some(&obj)), table => meta.table,
            id, is_add, fields, form_error => errors.get("__all__"),
        },
    )?;
    Ok((status, page).into_response())
}

fn after_save(table: &str, id: i64, form: &HashMap<String, String>) -> Response {
    let to = if form.contains_key("_continue") {
        format!("/admin/{table}/{id}/")
    } else if form.contains_key("_addanother") {
        format!("/admin/{table}/add")
    } else {
        format!("/admin/{table}/?msg=saved")
    };
    Redirect::to(&to).into_response()
}

async fn add_page(State(site): S, user: CurrentUser, uri: Uri, Path(table): Path<String>) -> Page {
    let u = staff(&user, &uri)?;
    form_page(&site, &u.username, model(&site, &table)?, None, &HashMap::new(), HashMap::new(), StatusCode::OK).await
}

async fn add_submit(
    State(site): S,
    user: CurrentUser,
    uri: Uri,
    Path(table): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let errors = match validate(meta, &form, true).await {
        Ok(cols) => match orm::insert_row(meta.table, &cols).await {
            Ok(id) => return Ok(after_save(&table, id, &form)),
            Err(e) => [("__all__".to_string(), db_error_message(&e).ok_or_else(|| fail(e))?.to_string())].into(),
        },
        Err(errors) => errors,
    };
    form_page(&site, &u.username, meta, None, &form, errors, StatusCode::UNPROCESSABLE_ENTITY).await
}

async fn change_page(State(site): S, user: CurrentUser, uri: Uri, Path((table, id)): Path<(String, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    let (_, vals) = q.rows().await.map_err(fail)?.pop().ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    form_page(&site, &u.username, meta, Some(id), &raw_values(meta, &vals), HashMap::new(), StatusCode::OK).await
}

async fn change_submit(
    State(site): S,
    user: CurrentUser,
    uri: Uri,
    Path((table, id)): Path<(String, i64)>,
    Form(form): Form<HashMap<String, String>>,
) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let errors = match validate(meta, &form, false).await {
        Ok(cols) => {
            let mut q = Query::new(meta);
            q.filter.push(by_id(id));
            match q.update(&cols).await {
                Ok(_) => return Ok(after_save(&table, id, &form)),
                Err(e) => [("__all__".to_string(), db_error_message(&e).ok_or_else(|| fail(e))?.to_string())].into(),
            }
        }
        Err(errors) => errors,
    };
    form_page(&site, &u.username, meta, Some(id), &form, errors, StatusCode::UNPROCESSABLE_ENTITY).await
}

async fn delete_page(State(site): S, user: CurrentUser, uri: Uri, Path((table, id)): Path<(String, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let obj = object_label(meta, id).await?;
    render(
        &site,
        "delete.html",
        context! {
            title => format!("Delete {obj}"), username => u.username, crumbs => crumbs(meta, Some(&obj)),
            table, id, obj, name => meta.name.to_lowercase(),
        },
    )
}

async fn delete_submit(State(site): S, user: CurrentUser, uri: Uri, Path((table, id)): Path<(String, i64)>) -> Page {
    let u = staff(&user, &uri)?;
    let meta = model(&site, &table)?;
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    match q.delete().await {
        Ok(_) => Ok(Redirect::to(&format!("/admin/{table}/?msg=deleted")).into_response()),
        Err(e) if e.is_foreign_key_violation() => {
            let obj = object_label(meta, id).await?;
            let page = render(
                &site,
                "delete.html",
                context! {
                    title => format!("Delete {obj}"), username => u.username, crumbs => crumbs(meta, Some(&obj)),
                    table, id, obj, name => meta.name.to_lowercase(),
                    error => "This record can't be deleted because other records still reference it.",
                },
            )?;
            Ok((StatusCode::CONFLICT, page).into_response())
        }
        Err(e) => Err(fail(e)),
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
    }

    #[test]
    fn next_stays_inside_admin() {
        assert_eq!(safe_next(Some("/admin/post/")), "/admin/post/");
        assert_eq!(safe_next(Some("https://evil.example")), "/admin/");
        assert_eq!(safe_next(Some("//evil.example/admin/")), "/admin/");
        assert_eq!(safe_next(None), "/admin/");
    }
}

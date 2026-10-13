//! A JSON API per model, generated from the same metadata as the admin:
//! what Django needs Django REST framework (and drf-spectacular) for.
//!
//! `App::api::<Post>(Api::new().read(Access::Public))` serves
//!
//! | method        | path                | does                                     |
//! |---------------|---------------------|------------------------------------------|
//! | GET           | `/api/<table>/`     | list: `?limit=&offset=&ordering=-field&search=&field=value` |
//! | POST          | `/api/<table>/`     | create, `201` with the object            |
//! | GET           | `/api/<table>/{id}` | detail                                   |
//! | PUT / PATCH   | `/api/<table>/{id}` | replace / partial update                 |
//! | DELETE        | `/api/<table>/{id}` | `204`                                    |
//!
//! plus an OpenAPI 3 document for every exposed model at `/api/schema.json`.
//! Validation errors are `400 {"field": ["message"]}`, like DRF.

#![allow(clippy::result_large_err)]

use crate::auth::{self, CurrentUser};
use crate::orm::{self, by_id, like_escape, FieldMeta, FieldType, Model, ModelMeta, Node, Query, Value};
use crate::{DateTime, Error};
use axum::extract::{Path, Query as UrlQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Map, Value as JsonValue};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 500;

/// Who may use an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Anyone, logged in or not.
    Public,
    /// Any logged-in, active user.
    Authenticated,
    /// Staff users only. The default for reads and writes.
    Staff,
    /// Nobody: the operation is not offered.
    Nobody,
}

impl Access {
    pub(crate) fn allows(self, user: &CurrentUser) -> bool {
        match self {
            Access::Public => true,
            Access::Authenticated => user.0.is_some(),
            Access::Staff => user.0.as_ref().is_some_and(|u| u.is_staff),
            Access::Nobody => false,
        }
    }
}

/// API settings for model `M`.
pub struct Api<M> {
    pub(crate) opts: Options,
    _m: PhantomData<fn() -> M>,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub(crate) read: Access,
    pub(crate) write: Access,
    /// A foreign key to the user: non-staff users only ever see and change their own rows.
    pub(crate) owner: Option<&'static str>,
}

impl<M: Model> Default for Api<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Model> Api<M> {
    /// Staff-only reads and writes until you say otherwise.
    pub fn new() -> Self {
        Api { opts: Options { read: Access::Staff, write: Access::Staff, owner: None }, _m: PhantomData }
    }
    /// Who may list and fetch.
    pub fn read(mut self, access: Access) -> Self {
        self.opts.read = access;
        self
    }
    /// Who may create, update and delete.
    pub fn write(mut self, access: Access) -> Self {
        self.opts.write = access;
        self
    }
    /// Rows belong to the user in `owner` (a foreign key to the user model), DRF's `IsOwner`
    /// plus `get_queryset` plus `perform_create` in one line. Non-staff users list, read,
    /// update and delete only their own rows (anyone else's is a 404, so ids reveal nothing),
    /// and every row they create or update is theirs whatever the body says. Staff see all.
    /// Panics at startup if `owner` isn't a foreign key to the user model.
    pub fn owned_by(mut self, owner: &crate::orm::Col<M, i64>) -> Self {
        let name = owner.name();
        let is_user_fk = M::meta().field(name).is_some_and(|f| f.fk == Some(crate::auth::User::TABLE));
        assert!(is_user_fk, "rangoli api: owned_by(`{name}`) on {} needs `#[field(fk = User)]`", M::TABLE);
        self.opts.owner = Some(name);
        self
    }
}

/// The owner restriction for this user: `(column, user id)`, or `None` for staff and unowned models.
fn ownership(opts: &Options, user: &CurrentUser) -> Option<(&'static str, i64)> {
    let col = opts.owner?;
    match &user.0 {
        Some(u) if u.is_staff => None,
        Some(u) => Some((col, u.id.unwrap_or(-1))),
        None => Some((col, -1)), // a public read of an owned model shows nothing
    }
}

/// Claim the row for the user: non-staff always own what they write; staff default to themselves.
fn claim(opts: &Options, user: &CurrentUser, body: &JsonValue, adding: bool) -> JsonValue {
    let mut body = body.clone();
    if let (Some(col), Some(u), Some(obj)) = (opts.owner, &user.0, body.as_object_mut()) {
        if !u.is_staff || (adding && !obj.contains_key(col)) {
            obj.insert(col.into(), json!(u.id));
        }
    }
    body
}

struct Site {
    models: HashMap<&'static str, (&'static ModelMeta, Options)>,
}

type S = State<Arc<Site>>;
type Reply = Result<Response, Response>;

pub(crate) fn router(exposed: Vec<(&'static ModelMeta, Options)>) -> Router {
    if exposed.is_empty() {
        return Router::new();
    }
    let site = Arc::new(Site { models: exposed.into_iter().map(|(m, o)| (m.table, (m, o))).collect() });
    Router::new()
        .route("/api/schema.json", get(schema))
        .route("/api/{table}/", get(list).post(create))
        .route("/api/{table}/{id}", get(detail).put(replace).patch(patch).delete(remove))
        .with_state(site)
}

// ---------------------------------------------------------------- helpers

fn problem(status: StatusCode, detail: &str) -> Response {
    (status, Json(json!({ "detail": detail }))).into_response()
}

fn fail(e: Error) -> Response {
    match e {
        Error::NotFound => problem(StatusCode::NOT_FOUND, "Not found."),
        e if e.is_unique_violation() => {
            (StatusCode::BAD_REQUEST, Json(json!({ "non_field_errors": ["A record with one of these unique values already exists."] })))
                .into_response()
        }
        e if e.is_foreign_key_violation() => {
            (StatusCode::BAD_REQUEST, Json(json!({ "non_field_errors": ["A related object does not exist or is still referenced."] })))
                .into_response()
        }
        e => e.into_response(),
    }
}

/// A token's scope on this request, if it came with a bearer token.
pub(crate) type ScopeExt = Option<axum::Extension<auth::TokenScope>>;

fn endpoint<'a>(
    site: &'a Site,
    table: &str,
    user: &CurrentUser,
    scope: &ScopeExt,
    write: bool,
) -> Result<(&'static ModelMeta, &'a Options), Response> {
    let not_found = || problem(StatusCode::NOT_FOUND, "Not found.");
    let (meta, opts) = site.models.get(table).ok_or_else(not_found)?;
    if let Some(axum::Extension(scope)) = scope {
        if !scope.allows_table(table) {
            return Err(not_found()); // outside the token's tables: as if it didn't exist
        }
        if write && scope.read_only {
            return Err(problem(StatusCode::FORBIDDEN, "This token is read-only."));
        }
    }
    let access = if write { opts.write } else { opts.read };
    if access.allows(user) {
        Ok((meta, opts))
    } else if access == Access::Nobody {
        Err(problem(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed."))
    } else if user.0.is_none() {
        Err(problem(StatusCode::UNAUTHORIZED, "Authentication credentials were not provided."))
    } else {
        Err(problem(StatusCode::FORBIDDEN, "You do not have permission to perform this action."))
    }
}

/// One row as a JSON object. Password fields are never serialized.
fn to_json(meta: &ModelMeta, id: i64, vals: &[Value]) -> JsonValue {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(id));
    for (f, v) in meta.fields.iter().zip(vals).filter(|(f, _)| !f.password) {
        let j = match v {
            Value::Null => JsonValue::Null,
            Value::Int(i) if f.ty == FieldType::DateTime => json!(DateTime::from_unix(*i).to_string()),
            Value::Int(i) if f.ty == FieldType::Date => json!(crate::Date::from_days(*i).to_string()),
            Value::Int(i) => match f.ty {
                FieldType::Decimal(p) => json!(crate::decimal::format_units(*i, p)),
                _ => json!(i),
            },
            Value::Float(x) => json!(x),
            Value::Bool(b) => json!(b),
            Value::Text(s) if f.ty == FieldType::Json => serde_json::from_str(s).unwrap_or(JsonValue::Null),
            Value::Text(s) => json!(s),
        };
        obj.insert(f.name.into(), j);
    }
    JsonValue::Object(obj)
}

/// Parse one JSON value for a field, with the admin's validation rules.
fn from_json(f: &FieldMeta, j: &JsonValue) -> Result<Value, String> {
    if f.ty == FieldType::Json {
        // JSON escapes NUL as \u0000 when serialized, so any document is storable.
        // Any JSON document is valid content; `null` means SQL NULL only on nullable fields.
        return Ok(if j.is_null() && f.null { Value::Null } else { Value::Text(j.to_string()) });
    }
    if j.is_null() {
        return if f.null { Ok(Value::Null) } else { Err("This field may not be null.".into()) };
    }
    match f.ty {
        FieldType::Int => j.as_i64().map(Value::Int).ok_or_else(|| "A valid integer is required.".into()),
        FieldType::Float => j.as_f64().filter(|x| x.is_finite()).map(Value::Float).ok_or_else(|| "A valid number is required.".into()),
        FieldType::Bool => j.as_bool().map(Value::Bool).ok_or_else(|| "Must be a valid boolean.".into()),
        FieldType::DateTime => j
            .as_str()
            .and_then(DateTime::parse)
            .map(Value::from)
            .ok_or_else(|| "Datetime has wrong format. Use ISO 8601, e.g. 2026-10-07T02:15:00Z.".into()),
        FieldType::Date => {
            j.as_str().and_then(crate::Date::parse).map(Value::from).ok_or_else(|| "Date has wrong format. Use YYYY-MM-DD.".into())
        }
        // A fractional JSON number is a float by the time it is parsed (19.999999999999999 is
        // already 20.0), so it can't be checked exactly: those must come as strings.
        FieldType::Decimal(_) if j.is_f64() => Err("Send decimals with a fractional part as strings, e.g. \"19.99\".".into()),
        FieldType::Decimal(p) => match j {
            JsonValue::String(s) => Some(s.clone()),
            JsonValue::Number(n) => Some(n.to_string()),
            _ => None,
        }
        .and_then(|s| crate::decimal::parse_units(&s, p))
        .map(Value::Int)
        .ok_or_else(|| format!("A valid number with at most {p} decimal places is required.")),
        FieldType::Varchar(n) => match j.as_str() {
            Some(s) if s.chars().count() > n as usize => Err(format!("Ensure this field has no more than {n} characters.")),
            Some(s) if s.contains('\0') => Err("Null characters are not allowed.".into()),
            Some(s) if s.is_empty() && !f.null => Err("This field may not be blank.".into()),
            Some(s) if f.choices.is_some_and(|c| !c.iter().any(|(v, _)| *v == s)) => Err(format!("\"{s}\" is not a valid choice.")),
            Some(s) => Ok(Value::Text(s.into())),
            None => Err("Not a valid string.".into()),
        },
        FieldType::Json => unreachable!("handled above"),
        FieldType::Uuid => j.as_str().and_then(crate::Uuid::parse).map(Value::from).ok_or_else(|| "Must be a valid UUID.".into()),
        FieldType::Text => match j.as_str() {
            Some(s) if s.contains('\0') => Err("Null characters are not allowed.".into()),
            Some(s) if s.is_empty() && !f.null => Err("This field may not be blank.".into()),
            Some(s) => Ok(Value::Text(s.into())),
            None => Err("Not a valid string.".into()),
        },
    }
}

/// Add each many-to-many relation as an array of ids, one query per relation for all rows.
async fn attach_links(meta: &'static ModelMeta, objs: &mut [JsonValue]) -> Result<(), Response> {
    if meta.m2m.is_empty() || objs.is_empty() {
        return Ok(());
    }
    static PAIR: [FieldMeta; 2] = [orm::link_meta("source_id"), orm::link_meta("target_id")];
    let ids: Vec<Value> = objs.iter().filter_map(|o| o["id"].as_i64()).map(Value::Int).collect();
    for rel in meta.m2m {
        let mut q =
            Query { table: rel.through, fields: &PAIR, filter: vec![], order: vec![("target_id", false)], limit: None, offset: None };
        q.filter.push(Node::In("source_id", ids.clone(), FieldType::Int));
        let mut by_source: HashMap<i64, Vec<i64>> = HashMap::new();
        for (_, v) in q.rows().await.map_err(fail)? {
            if let (Value::Int(src), Value::Int(dst)) = (&v[0], &v[1]) {
                by_source.entry(*src).or_default().push(*dst);
            }
        }
        for o in objs.iter_mut() {
            let id = o["id"].as_i64().unwrap_or_default();
            o[rel.name] = json!(by_source.remove(&id).unwrap_or_default());
        }
    }
    Ok(())
}

/// Many-to-many ids from a request body; `None` for relations the body leaves out.
/// Join table -> ids to link (`None` when the body leaves the relation out).
type BodyLinks = Vec<(&'static str, Option<Vec<i64>>)>;

fn body_links(meta: &ModelMeta, body: &JsonValue) -> Result<BodyLinks, Response> {
    let mut errors = Map::new();
    let out = meta
        .m2m
        .iter()
        .map(|rel| {
            let ids = body.get(rel.name).map(|j| j.as_array().and_then(|a| a.iter().map(JsonValue::as_i64).collect::<Option<Vec<i64>>>()));
            if let Some(None) = ids {
                errors.insert(rel.name.into(), json!(["Expected a list of ids."]));
            }
            (rel.through, ids.flatten())
        })
        .collect();
    if errors.is_empty() {
        Ok(out)
    } else {
        Err((StatusCode::BAD_REQUEST, Json(JsonValue::Object(errors))).into_response())
    }
}

/// Validate a request body. `partial` (PATCH) skips missing fields.
async fn columns(
    meta: &'static ModelMeta,
    body: &JsonValue,
    adding: bool,
    partial: bool,
) -> Result<Vec<(&'static str, Value, FieldType)>, Response> {
    let Some(obj) = body.as_object() else {
        return Err(problem(StatusCode::BAD_REQUEST, "Expected a JSON object."));
    };
    let (mut cols, mut errors) = (vec![], Map::new());
    for f in meta.fields {
        if f.is_auto() {
            if f.auto_now || adding {
                cols.push((f.name, f.auto_value(), f.ty));
            }
            continue;
        }
        let value = match obj.get(f.name) {
            Some(j) => from_json(f, j),
            None if partial => continue,
            None if f.default.is_some() => Ok(f.default.map_or(Value::Null, |d| d.value())),
            None if f.null => Ok(Value::Null),
            None if f.ty == FieldType::Bool => Ok(Value::Bool(false)),
            None => Err("This field is required.".into()),
        };
        match value {
            Ok(Value::Text(raw)) if f.password => match auth::hash_password(&raw).await {
                Ok(hash) => cols.push((f.name, Value::Text(hash), f.ty)),
                Err(e) => return Err(fail(e)),
            },
            Ok(v) => cols.push((f.name, v, f.ty)),
            Err(e) => {
                errors.insert(f.name.into(), json!([e]));
            }
        }
    }
    if let Some(unknown) = obj.keys().find(|k| *k != "id" && meta.field(k).is_none() && !meta.m2m.iter().any(|r| r.name == *k)) {
        errors.insert(unknown.clone(), json!(["Unknown field."]));
    }
    if errors.is_empty() {
        Ok(cols)
    } else {
        Err((StatusCode::BAD_REQUEST, Json(JsonValue::Object(errors))).into_response())
    }
}

async fn fetch(meta: &'static ModelMeta, id: i64, owner: Option<(&'static str, i64)>) -> Result<JsonValue, Response> {
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    q.filter.extend(owned(owner));
    let (id, vals) = q.rows().await.map_err(fail)?.pop().ok_or_else(|| fail(Error::NotFound))?;
    let mut obj = [to_json(meta, id, &vals)];
    attach_links(meta, &mut obj).await?;
    let [obj] = obj;
    Ok(obj)
}

fn owned(owner: Option<(&'static str, i64)>) -> Option<Node> {
    owner.map(|(col, uid)| Node::Cmp(col, "=", Value::Int(uid), FieldType::Int))
}

fn param<'a>(p: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    p.get(k).map(String::as_str)
}

// ---------------------------------------------------------------- handlers

async fn list(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    Path(table): Path<String>,
    UrlQuery(p): UrlQuery<HashMap<String, String>>,
) -> Reply {
    let (meta, opts) = endpoint(&site, &table, &user, &scope, false)?;
    let mut q = Query::new(meta);
    q.filter.extend(owned(ownership(opts, &user)));
    // `?field=value` filters on exact matches, typed by the field.
    for (k, raw) in &p {
        if matches!(k.as_str(), "limit" | "offset" | "ordering" | "search") {
            continue;
        }
        let f = meta.field(k).filter(|f| !f.password).ok_or_else(|| problem(StatusCode::BAD_REQUEST, &format!("Unknown filter `{k}`.")))?;
        let j = match f.ty {
            FieldType::Json => return Err(problem(StatusCode::BAD_REQUEST, &format!("Cannot filter on JSON field `{k}`."))),
            FieldType::Varchar(_) | FieldType::Text | FieldType::DateTime | FieldType::Date | FieldType::Decimal(_) | FieldType::Uuid => {
                json!(raw)
            }
            _ if raw == "null" => JsonValue::Null,
            _ => serde_json::from_str(raw).unwrap_or(JsonValue::String(raw.clone())),
        };
        let v = from_json(f, &j).map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({ k.clone(): [e] }))).into_response())?;
        q.filter.push(if v == Value::Null { Node::IsNull(f.name, false) } else { Node::Cmp(f.name, "=", v, f.ty) });
    }
    if let Some(term) = param(&p, "search").filter(|s| !s.is_empty()) {
        let pat = format!("%{}%", like_escape(term));
        let text: Vec<Node> = meta
            .fields
            .iter()
            .filter(|f| !f.password && matches!(f.ty, FieldType::Varchar(_) | FieldType::Text))
            .map(|f| Node::Like(f.name, pat.clone()))
            .collect();
        q.filter.push(Node::Or(text));
    }
    if let Some(o) = param(&p, "ordering") {
        let (desc, name) = o.strip_prefix('-').map_or((false, o), |n| (true, n));
        let field = if name == "id" {
            "id"
        } else {
            meta.field(name)
                .filter(|f| !f.password)
                .map(|f| f.name)
                .ok_or_else(|| problem(StatusCode::BAD_REQUEST, &format!("Cannot order by `{name}`.")))?
        };
        q.order = vec![(field, desc)];
    } else {
        q.order = vec![("id", false)];
    }
    let limit = param(&p, "limit").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset: u64 = param(&p, "offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let count = q.count().await.map_err(fail)? as u64;
    q.limit = Some(limit);
    q.offset = Some(offset);
    let mut results: Vec<JsonValue> = q.rows().await.map_err(fail)?.iter().map(|(id, v)| to_json(meta, *id, v)).collect();
    attach_links(meta, &mut results).await?;
    // Like DRF's LimitOffsetPagination; links are relative so they work behind any host or proxy.
    let page = |off: u64| {
        let mut pairs: Vec<(String, String)> =
            p.iter().filter(|(k, _)| *k != "limit" && *k != "offset").map(|(k, v)| (k.clone(), v.clone())).collect();
        pairs.sort();
        pairs.push(("limit".into(), limit.to_string()));
        pairs.push(("offset".into(), off.to_string()));
        format!("/api/{table}/?{}", serde_urlencoded::to_string(pairs).unwrap())
    };
    let next = (offset + limit < count).then(|| page(offset + limit));
    let previous = (offset > 0).then(|| page(offset.saturating_sub(limit)));
    Ok(Json(json!({ "count": count, "next": next, "previous": previous, "results": results })).into_response())
}

async fn detail(State(site): S, user: CurrentUser, scope: ScopeExt, Path((table, id)): Path<(String, i64)>) -> Reply {
    let (meta, opts) = endpoint(&site, &table, &user, &scope, false)?;
    Ok(Json(fetch(meta, id, ownership(opts, &user)).await?).into_response())
}

/// How a write arrived, for the admin History. Set by the MCP server on its internal requests
/// (a request extension, so no client can claim it); plain API calls have none.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Via(pub(crate) &'static str);

type ViaExt = Option<axum::Extension<Via>>;

/// The admin's name for a row: its display field, else "Post object (7)".
async fn repr_of(meta: &'static ModelMeta, id: i64) -> crate::Result<String> {
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    let (_, vals) = q.rows().await?.pop().ok_or(Error::NotFound)?;
    let shown = meta.display.and_then(|d| meta.fields.iter().position(|f| f.name == d)).and_then(|i| match &vals[i] {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    });
    Ok(shown.unwrap_or_else(|| format!("{} object ({id})", meta.name)))
}

/// Record a write in the admin History, in the write's own transaction, so an app's or an AI
/// agent's changes are reviewable next to the admin's. Anonymous writes have no user to name.
async fn audit(user: &CurrentUser, meta: &'static ModelMeta, id: i64, action: i64, verb: &str, via: Option<Via>) -> crate::Result<()> {
    let Some(user_id) = user.0.as_ref().and_then(|u| u.id) else { return Ok(()) };
    let mut entry = crate::admin::LogEntry {
        id: None,
        user_id,
        table_name: meta.table.into(),
        object_id: id,
        object_repr: repr_of(meta, id).await?.chars().take(200).collect(),
        action,
        message: format!("{verb} via {}.", via.map_or("the API", |v| v.0)),
        at: auth::now(),
    };
    entry.save().await
}

async fn create(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    via: ViaExt,
    Path(table): Path<String>,
    Json(body): Json<JsonValue>,
) -> Reply {
    let via = via.map(|v| v.0);
    let (meta, opts) = endpoint(&site, &table, &user, &scope, true)?;
    if opts.owner.is_some() && user.0.is_none() {
        // An owned row needs an owner; with `write(Access::Public)` the body could otherwise name anyone.
        return Err(problem(StatusCode::UNAUTHORIZED, "Authentication credentials were not provided."));
    }
    let body = claim(opts, &user, &body, true);
    let cols = columns(meta, &body, true, false).await?;
    let links = body_links(meta, &body)?;
    let id = crate::atomic(async {
        let id = orm::insert_row(meta.table, &cols).await?;
        for (through, ids) in &links {
            orm::set_links(through, id, ids.as_deref().unwrap_or_default()).await?;
        }
        audit(&user, meta, id, 1, "Added", via).await?;
        Ok(id)
    })
    .await
    .map_err(fail)?;
    Ok((StatusCode::CREATED, Json(fetch(meta, id, None).await?)).into_response())
}

#[allow(clippy::too_many_arguments)]
async fn update(
    site: &Site,
    user: &CurrentUser,
    scope: &ScopeExt,
    via: Option<Via>,
    table: &str,
    id: i64,
    body: &JsonValue,
    partial: bool,
) -> Reply {
    let (meta, opts) = endpoint(site, table, user, scope, true)?;
    fetch(meta, id, ownership(opts, user)).await?; // 404 before validation, like DRF (and for others' rows)
    let body = &claim(opts, user, body, false);
    let cols = columns(meta, body, false, partial).await?;
    let links = body_links(meta, body)?;
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    crate::atomic(async {
        q.update(&cols).await?;
        for (through, ids) in &links {
            match ids {
                Some(ids) => orm::set_links(through, id, ids).await?,
                None if !partial => orm::set_links(through, id, &[]).await?,
                None => {}
            }
        }
        audit(user, meta, id, 2, "Changed", via).await?;
        Ok(())
    })
    .await
    .map_err(fail)?;
    Ok(Json(fetch(meta, id, None).await?).into_response())
}

async fn replace(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    via: ViaExt,
    Path((table, id)): Path<(String, i64)>,
    Json(body): Json<JsonValue>,
) -> Reply {
    update(&site, &user, &scope, via.map(|v| v.0), &table, id, &body, false).await
}

async fn patch(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    via: ViaExt,
    Path((table, id)): Path<(String, i64)>,
    Json(body): Json<JsonValue>,
) -> Reply {
    update(&site, &user, &scope, via.map(|v| v.0), &table, id, &body, true).await
}

async fn remove(State(site): S, user: CurrentUser, scope: ScopeExt, via: ViaExt, Path((table, id)): Path<(String, i64)>) -> Reply {
    let (meta, opts) = endpoint(&site, &table, &user, &scope, true)?;
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    q.filter.extend(owned(ownership(opts, &user)));
    // Logged first (it needs the row's name); a delete that finds nothing rolls the entry back.
    crate::atomic(async {
        audit(&user, meta, id, 3, "Deleted", via.map(|v| v.0)).await?;
        match q.delete().await? {
            0 => Err(Error::NotFound),
            _ => Ok(()),
        }
    })
    .await
    .map_err(fail)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------- OpenAPI

pub(crate) fn field_schema(f: &FieldMeta) -> JsonValue {
    let mut s = match f.ty {
        FieldType::Int => json!({ "type": "integer", "format": "int64" }),
        FieldType::Float => json!({ "type": "number", "format": "double" }),
        FieldType::Bool => json!({ "type": "boolean" }),
        FieldType::Varchar(n) => json!({ "type": "string", "maxLength": n }),
        FieldType::Text => json!({ "type": "string" }),
        FieldType::Uuid => json!({ "type": "string", "format": "uuid" }),
        FieldType::DateTime => json!({ "type": "string", "format": "date-time" }),
        FieldType::Date => json!({ "type": "string", "format": "date" }),
        FieldType::Decimal(p) => json!({ "type": "string", "format": "decimal", "pattern": format!(r"^-?\d*(\.\d{{0,{p}}})?$") }),
        FieldType::Json => json!({ "description": "Any JSON value" }),
    };
    if let Some(choices) = f.choices {
        s["enum"] = json!(choices.iter().map(|(v, _)| *v).collect::<Vec<_>>());
    }
    if let Some(d) = f.default {
        s["default"] = match d.value() {
            Value::Int(i) => json!(i),
            Value::Float(x) => json!(x),
            Value::Bool(b) => json!(b),
            Value::Text(t) => json!(t),
            Value::Null => JsonValue::Null,
        };
    }
    if f.null {
        s["nullable"] = json!(true);
    }
    if f.is_auto() {
        s["readOnly"] = json!(true);
    }
    if f.password {
        s["writeOnly"] = json!(true);
    }
    if let Some(t) = f.fk {
        s["description"] = json!(format!("id of a {t} row"));
    }
    s
}

/// OpenAPI 3.0 for every exposed model, generated from the model metadata.
async fn schema(State(site): S) -> Json<JsonValue> {
    let (mut paths, mut schemas) = (Map::new(), Map::new());
    let mut tables: Vec<_> = site.models.values().collect();
    tables.sort_by_key(|(m, _)| m.table);
    for (meta, _) in tables {
        let mut props = Map::new();
        props.insert("id".into(), json!({ "type": "integer", "format": "int64", "readOnly": true }));
        for f in meta.fields {
            props.insert(f.name.into(), field_schema(f));
        }
        for rel in meta.m2m {
            props.insert(
                rel.name.into(),
                json!({ "type": "array", "items": { "type": "integer" }, "description": format!("ids of related {} rows", rel.target) }),
            );
        }
        let required: Vec<&str> = meta
            .fields
            .iter()
            .filter(|f| !f.null && !f.is_auto() && f.ty != FieldType::Bool && f.default.is_none())
            .map(|f| f.name)
            .collect();
        schemas.insert(meta.name.into(), json!({ "type": "object", "properties": props, "required": required }));
        let item = json!({ "$ref": format!("#/components/schemas/{}", meta.name) });
        let page = json!({ "type": "object", "properties": {
            "count": { "type": "integer" }, "next": { "type": "string", "nullable": true },
            "previous": { "type": "string", "nullable": true }, "results": { "type": "array", "items": item } } });
        let ok = |schema: JsonValue| json!({ "description": "OK", "content": { "application/json": { "schema": schema } } });
        let body = json!({ "required": true, "content": { "application/json": { "schema": item } } });
        let id_param = json!([{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer" } }]);
        let list_params = json!([
            { "name": "limit", "in": "query", "schema": { "type": "integer", "maximum": MAX_LIMIT } },
            { "name": "offset", "in": "query", "schema": { "type": "integer" } },
            { "name": "ordering", "in": "query", "schema": { "type": "string" }, "description": "Field name, `-` prefix for descending" },
            { "name": "search", "in": "query", "schema": { "type": "string" } }
        ]);
        paths.insert(format!("/api/{}/", meta.table), json!({
            "get": { "summary": format!("List {}", meta.name), "parameters": list_params, "responses": { "200": ok(page) } },
            "post": { "summary": format!("Create a {}", meta.name), "requestBody": body, "responses": { "201": ok(item.clone()), "400": { "description": "Validation errors" } } },
        }));
        paths.insert(format!("/api/{}/{{id}}", meta.table), json!({
            "parameters": id_param,
            "get": { "summary": format!("Get a {}", meta.name), "responses": { "200": ok(item.clone()), "404": { "description": "Not found" } } },
            "put": { "summary": format!("Replace a {}", meta.name), "requestBody": body, "responses": { "200": ok(item.clone()) } },
            "patch": { "summary": format!("Update a {}", meta.name), "requestBody": body, "responses": { "200": ok(item.clone()) } },
            "delete": { "summary": format!("Delete a {}", meta.name), "responses": { "204": { "description": "Deleted" } } },
        }));
    }
    Json(json!({
        "openapi": "3.0.3",
        "info": { "title": "Rangoli API", "version": env!("CARGO_PKG_VERSION") },
        "paths": paths,
        "components": { "schemas": schemas },
    }))
}

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

#[derive(Clone, Debug)]
pub struct Options {
    pub(crate) read: Access,
    pub(crate) write: Access,
    /// A foreign key to the user: non-staff users only ever see and change their own rows.
    pub(crate) owner: Option<&'static str>,
    /// Which rows each user may see and change (`Api::rows`).
    pub(crate) rows: Option<crate::orm::Policy>,
}

impl<M: Model> Default for Api<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Model> Api<M> {
    /// Staff-only reads and writes until you say otherwise.
    pub fn new() -> Self {
        Api { opts: Options { read: Access::Staff, write: Access::Staff, owner: None, rows: None }, _m: PhantomData }
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

impl<M: Model + 'static> Api<M> {
    /// Row-level permissions: the rows each user may list, read, change and delete, as a filter.
    /// Everything else is a 404, as if it didn't exist; a write that would move a row outside
    /// the caller's rows is refused (403) and rolled back. MCP tools go through the same check.
    ///
    /// ```ignore
    /// Api::new().rows(|user| match user {
    ///     Some(u) if u.is_superuser => None,                          // everything
    ///     Some(u) => Some(Doc::TEAM_ID.is_in(teams_of(u))),           // their teams' docs
    ///     None => Some(Doc::PUBLIC.eq(true)),                         // the public ones
    /// })
    /// ```
    pub fn rows(mut self, policy: impl Fn(Option<&crate::auth::User>) -> Option<crate::orm::Expr<M>> + Send + Sync + 'static) -> Self {
        self.opts.rows = Some(crate::orm::Policy::new(policy));
        self
    }
}

/// Every row filter for this caller: `owned_by` and the `rows` policy.
fn restrict(opts: &Options, user: &CurrentUser) -> Vec<Node> {
    let mut out: Vec<Node> = owned(ownership(opts, user)).into_iter().collect();
    out.extend(opts.rows.as_ref().and_then(|p| p.filter(user.0.as_ref())));
    out
}

/// After a write, inside its transaction: the row must still be one the caller may see.
async fn still_visible(meta: &'static ModelMeta, opts: &Options, user: &CurrentUser, id: i64) -> crate::Result<()> {
    if let Some(f) = opts.rows.as_ref().and_then(|p| p.filter(user.0.as_ref())) {
        if !orm::row_matches(meta, id, f).await? {
            return Err(Error::Forbidden("You can't save a row outside the rows you may see.".into()));
        }
    }
    Ok(())
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
        .route("/api/{table}/", get(list).post(create).fallback(query_method))
        .route("/api/{table}/similar", get(similar))
        .route("/api/{table}/aggregate", get(aggregate))
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
        Error::Forbidden(m) => problem(StatusCode::FORBIDDEN, &m),
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
    for (f, v) in meta.fields.iter().zip(vals).filter(|(f, _)| f.exposed()) {
        obj.insert(f.name.into(), value_json(f, v));
    }
    JsonValue::Object(obj)
}

/// One stored value as the API shows it.
fn value_json(f: &FieldMeta, v: &Value) -> JsonValue {
    {
        match v {
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
        }
    }
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
        FieldType::Opaque | FieldType::Vector(_) => Err("This field is read-only.".into()),
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
        FieldType::Text | FieldType::Encrypted => match j.as_str() {
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
        if f.ty == FieldType::Opaque {
            continue; // shown as text, never written: the database keeps its value or default
        }
        if f.private {
            // Not writable here: new rows get the default (or empty), existing ones keep their value.
            if adding {
                cols.push((
                    f.name,
                    f.default.map_or_else(|| if f.null { Value::Null } else { crate::migrate::zero(f.ty) }, |d| d.value()),
                    f.ty,
                ));
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
    // A private field reads as unknown: the API doesn't confirm it exists.
    if let Some(unknown) =
        obj.keys().find(|k| *k != "id" && meta.field(k).is_none_or(|f| f.private) && !meta.m2m.iter().any(|r| r.name == *k))
    {
        errors.insert(unknown.clone(), json!(["Unknown field."]));
    }
    if errors.is_empty() {
        Ok(cols)
    } else {
        Err((StatusCode::BAD_REQUEST, Json(JsonValue::Object(errors))).into_response())
    }
}

async fn fetch(meta: &'static ModelMeta, id: i64, filters: Vec<Node>) -> Result<JsonValue, Response> {
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    q.filter.extend(filters);
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
    list_rows(&site, &user, &scope, &table, p).await
}

/// The HTTP QUERY method (django/new-features #185): a list request whose options travel in a
/// JSON body (`{"filters": {...}, "search", "ordering", "limit", "offset", "cursor"}`) instead of
/// the URL, for queries too long or too private for a link. Safe and repeatable, like GET.
async fn query_method(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    method: axum::http::Method,
    Path(table): Path<String>,
    body: axum::body::Bytes,
) -> Reply {
    if method.as_str() != "QUERY" {
        return Err(problem(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed."));
    }
    let body: JsonValue = serde_json::from_slice(&body).map_err(|_| problem(StatusCode::BAD_REQUEST, "Expected a JSON object."))?;
    let Some(obj) = body.as_object() else { return Err(problem(StatusCode::BAD_REQUEST, "Expected a JSON object.")) };
    let text = |v: &JsonValue| v.as_str().map_or_else(|| v.to_string(), str::to_owned);
    let mut p: HashMap<String, String> = HashMap::new();
    if let Some(filters) = obj.get("filters").and_then(JsonValue::as_object) {
        for (k, v) in filters {
            p.insert(k.clone(), text(v));
        }
    }
    for k in ["search", "ordering", "limit", "offset", "cursor"] {
        if let Some(v) = obj.get(k).filter(|v| !v.is_null()) {
            p.insert(k.into(), text(v));
        }
    }
    list_rows(&site, &user, &scope, &table, p).await
}

const LOOKUPS: &str = "exact, ne, gt, gte, lt, lte, in, contains, startswith, isnull";

/// One `?field__lookup=value` filter as a query node.
fn lookup(meta: &'static ModelMeta, key: &str, raw: &str) -> Result<Node, Response> {
    static ID: FieldMeta = orm::link_meta("id");
    let (name, op) = key.split_once("__").unwrap_or((key, "exact"));
    let bad = |msg: String| problem(StatusCode::BAD_REQUEST, &msg);
    let f = if name == "id" { Some(&ID) } else { meta.field(name).filter(|f| f.exposed()) };
    let f = f.ok_or_else(|| bad(format!("Unknown filter `{name}`.")))?;
    match f.ty {
        FieldType::Json => return Err(bad(format!("Cannot filter on JSON field `{name}`."))),
        FieldType::Encrypted => return Err(bad(format!("Cannot filter on encrypted field `{name}`."))),
        _ => {}
    }
    let typed = |raw: &str| {
        let j = match f.ty {
            FieldType::Varchar(_) | FieldType::Text | FieldType::DateTime | FieldType::Date | FieldType::Decimal(_) | FieldType::Uuid => {
                json!(raw)
            }
            _ if raw == "null" => JsonValue::Null,
            _ => serde_json::from_str(raw).unwrap_or(JsonValue::String(raw.to_string())),
        };
        from_json(f, &j).map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({ key: [e] }))).into_response())
    };
    let text = matches!(f.ty, FieldType::Varchar(_) | FieldType::Text);
    Ok(match op {
        "exact" => match typed(raw)? {
            Value::Null => Node::IsNull(f.name, false),
            v => Node::Cmp(f.name, "=", v, f.ty),
        },
        "ne" => match typed(raw)? {
            Value::Null => Node::IsNull(f.name, true),
            v => Node::Not(Box::new(Node::Cmp(f.name, "=", v, f.ty))),
        },
        "gt" | "gte" | "lt" | "lte" if f.ty != FieldType::Bool => {
            let sql_op = match op {
                "gt" => ">",
                "gte" => ">=",
                "lt" => "<",
                _ => "<=",
            };
            Node::Cmp(f.name, sql_op, typed(raw)?, f.ty)
        }
        "in" => {
            let vals = raw.split(',').filter(|v| !v.is_empty()).take(1000).map(|v| typed(v.trim())).collect::<Result<Vec<_>, _>>()?;
            Node::In(f.name, vals, f.ty)
        }
        "contains" | "icontains" if text => Node::Like(f.name, format!("%{}%", like_escape(raw))),
        "startswith" | "istartswith" if text => Node::Like(f.name, format!("{}%", like_escape(raw))),
        "isnull" => match raw {
            "true" | "1" => Node::IsNull(f.name, false),
            "false" | "0" => Node::IsNull(f.name, true),
            _ => return Err(bad(format!("`{key}` takes true or false."))),
        },
        _ => return Err(bad(format!("`{op}` doesn't apply to `{name}`. Lookups: {LOOKUPS} (contains and startswith on text only)."))),
    })
}

/// The rows a list request asks for and this user may see: access rules, `?field__lookup=`
/// filters and `?search=`. Shared by lists and aggregates, so a total never counts a hidden row.
fn filtered(meta: &'static ModelMeta, opts: &Options, user: &CurrentUser, p: &HashMap<String, String>) -> Result<Query, Response> {
    let mut q = Query::new(meta);
    q.filter.extend(restrict(opts, user));
    add_filters(meta, &mut q, p)?;
    Ok(q)
}

/// `?field__lookup=` filters and `?search=` onto `q` (the admin's "Ask" uses it too).
pub(crate) fn add_filters(meta: &'static ModelMeta, q: &mut Query, p: &HashMap<String, String>) -> Result<(), Response> {
    // `?field=value`, or `?field__lookup=value` (Django's lookups), each value typed by the field.
    for (k, raw) in p {
        if matches!(
            k.as_str(),
            "limit" | "offset" | "ordering" | "search" | "cursor" | "expand" | "sum" | "avg" | "min" | "max" | "group_by"
        ) {
            continue;
        }
        q.filter.push(lookup(meta, k, raw)?);
    }
    if let Some(term) = param(&p, "search").filter(|s| !s.is_empty() && !meta.search.is_empty()) {
        q.filter.push(orm::search_node(meta, term)); // the model's full-text index
    } else if let Some(term) = param(&p, "search").filter(|s| !s.is_empty()) {
        let pat = format!("%{}%", like_escape(term));
        let text: Vec<Node> = meta
            .fields
            .iter()
            .filter(|f| f.exposed() && matches!(f.ty, FieldType::Varchar(_) | FieldType::Text))
            .map(|f| Node::Like(f.name, pat.clone()))
            .collect();
        q.filter.push(Node::Or(text));
    }
    Ok(())
}

/// `GET /api/<table>/aggregate?sum=total&avg=total&group_by=status&paid=true`: counts and
/// totals over the rows a list with the same filters would return, so an agent asking "how much
/// did we sell" gets one number instead of paging through every order. `sum`/`avg` take number
/// fields, `min`/`max` numbers, text and dates; each takes a comma-separated list.
async fn aggregate(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    Path(table): Path<String>,
    UrlQuery(p): UrlQuery<HashMap<String, String>>,
) -> Reply {
    let (meta, opts) = endpoint(&site, &table, &user, &scope, false)?;
    let q = filtered(meta, opts, &user, &p)?;
    Ok(Json(aggregate_of(meta, &q, &p).await?).into_response())
}

/// The counts and totals `p` asks for over the rows `q` selects.
pub(crate) async fn aggregate_of(meta: &'static ModelMeta, q: &Query, p: &HashMap<String, String>) -> Result<JsonValue, Response> {
    let bad = |msg: String| problem(StatusCode::BAD_REQUEST, &msg);
    let field = |name: &str| meta.field(name).filter(|f| f.exposed()).ok_or_else(|| bad(format!("Unknown field `{name}`.")));
    let mut measures: Vec<(&str, &'static FieldMeta)> = vec![];
    for func in ["sum", "avg", "min", "max"] {
        for name in param(p, func).unwrap_or("").split(',').map(str::trim).filter(|n| !n.is_empty()) {
            let f = field(name)?;
            let ok = match func {
                "sum" | "avg" => matches!(f.ty, FieldType::Int | FieldType::Float | FieldType::Decimal(_)),
                _ => matches!(
                    f.ty,
                    FieldType::Int
                        | FieldType::Float
                        | FieldType::Decimal(_)
                        | FieldType::Varchar(_)
                        | FieldType::Text
                        | FieldType::Date
                        | FieldType::DateTime
                ),
            };
            if !ok {
                return Err(bad(format!("Cannot {func} `{name}`.")));
            }
            measures.push((func, f));
        }
    }
    let d = orm::db().dialect;
    let value = |func: &str, f: &FieldMeta, v: Value| match (func, v) {
        ("avg", Value::Float(x)) => json!(x / 10f64.powi(f.ty.decimal_places() as i32)),
        (_, v) => value_json(f, &v),
    };
    let Some(key) = param(p, "group_by") else {
        let mut out = Map::new();
        out.insert("count".into(), json!(q.count().await.map_err(fail)?));
        for (func, f) in &measures {
            let v = q.aggregate(&func.to_uppercase(), f.name, f.ty).await.map_err(fail)?;
            out.entry(*func).or_insert_with(|| json!({}))[f.name] = value(func, f, v);
        }
        return Ok(JsonValue::Object(out));
    };
    let k = field(key)?;
    if matches!(k.ty, FieldType::Json | FieldType::Encrypted | FieldType::Vector(_)) {
        return Err(bad(format!("Cannot group by `{key}`.")));
    }
    let int = if d == orm::Dialect::Mysql { "SIGNED" } else { "BIGINT" };
    // ponytail: one GROUP BY query per measure, merged here; capped at MAX_GROUPS groups.
    let counts = q.grouped(k.name, k.ty, &format!("CAST(COUNT(*) AS {int})"), FieldType::Int).await.map_err(fail)?;
    let truncated = counts.len() > MAX_GROUPS;
    let mut groups: Vec<JsonValue> = counts
        .into_iter()
        .take(MAX_GROUPS)
        .map(|(g, n)| json!({ k.name: value_json(k, &g), "count": match n { Value::Int(n) => n, _ => 0 } }))
        .collect();
    for (func, f) in &measures {
        let decode = if *func == "avg" { FieldType::Float } else { f.ty };
        let rows = q.grouped(k.name, k.ty, &orm::agg_expr(d, &func.to_uppercase(), f.name, f.ty), decode).await.map_err(fail)?;
        for (g, v) in rows {
            let gj = value_json(k, &g);
            if let Some(group) = groups.iter_mut().find(|x| x[k.name] == gj) {
                group.as_object_mut().unwrap().entry(*func).or_insert_with(|| json!({}))[f.name] = value(func, f, v);
            }
        }
    }
    Ok(json!({ "groups": groups, "truncated": truncated }))
}

const MAX_GROUPS: usize = 1000;

async fn list_rows(site: &Site, user: &CurrentUser, scope: &ScopeExt, table: &str, p: HashMap<String, String>) -> Reply {
    let (meta, opts) = endpoint(site, table, user, scope, false)?;
    let mut q = filtered(meta, opts, user, &p)?;
    if let Some(o) = param(&p, "ordering") {
        let (desc, name) = o.strip_prefix('-').map_or((false, o), |n| (true, n));
        let field = if name == "id" {
            "id"
        } else {
            meta.field(name)
                .filter(|f| f.exposed())
                .map(|f| f.name)
                .ok_or_else(|| problem(StatusCode::BAD_REQUEST, &format!("Cannot order by `{name}`.")))?
        };
        q.order = vec![(field, desc)];
    } else {
        q.order = vec![("id", false)];
    }
    let limit = param(&p, "limit").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    // `?cursor=` (empty for the first page): keyset pagination. No COUNT, no OFFSET, so page
    // 10,000 costs what page 1 does. It walks the id order, ascending or (`ordering=-id`) descending.
    if let Some(cursor) = param(&p, "cursor") {
        let desc = match q.order.as_slice() {
            [("id", desc)] => *desc,
            _ => return Err(problem(StatusCode::BAD_REQUEST, "Cursor pagination follows the id order: use ordering=id or ordering=-id.")),
        };
        if !cursor.is_empty() {
            let after: i64 = cursor.parse().map_err(|_| problem(StatusCode::BAD_REQUEST, "Invalid cursor."))?;
            q.filter.push(Node::Cmp("id", if desc { "<" } else { ">" }, Value::Int(after), FieldType::Int));
        }
        q.limit = Some(limit + 1); // one extra row says whether there is a next page
        let mut rows = q.rows().await.map_err(fail)?;
        let more = rows.len() as u64 > limit;
        rows.truncate(limit as usize);
        let mut results: Vec<JsonValue> = rows.iter().map(|(id, v)| to_json(meta, *id, v)).collect();
        attach_links(meta, &mut results).await?;
        expand(site, meta, user, scope, &mut results, param(&p, "expand")).await?;
        let next = more.then(|| {
            let mut pairs: Vec<(String, String)> = p.iter().filter(|(k, _)| *k != "cursor").map(|(k, v)| (k.clone(), v.clone())).collect();
            pairs.sort();
            pairs.push(("cursor".into(), rows.last().map_or(0, |(id, _)| *id).to_string()));
            format!("/api/{table}/?{}", serde_urlencoded::to_string(pairs).unwrap())
        });
        return Ok(Json(json!({ "next": next, "results": results })).into_response());
    }
    let offset: u64 = param(&p, "offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let count = q.count().await.map_err(fail)? as u64;
    q.few_search_matches(count as i64);
    q.limit = Some(limit);
    q.offset = Some(offset);
    let mut results: Vec<JsonValue> = q.rows().await.map_err(fail)?.iter().map(|(id, v)| to_json(meta, *id, v)).collect();
    attach_links(meta, &mut results).await?;
    expand(site, meta, user, scope, &mut results, param(&p, "expand")).await?;
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

/// Answers a prompt with text: any model you like (`App::llm`).
pub type LlmFn = dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<String>> + Send>> + Send + Sync;
pub(crate) static LLM: std::sync::OnceLock<Box<LlmFn>> = std::sync::OnceLock::new();

/// Turns text into the vectors a model's `#[field(vector = N)]` holds (`App::embed`).
pub type EmbedFn = dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<Vec<f32>>> + Send>> + Send + Sync;
pub(crate) static EMBED: std::sync::OnceLock<Box<EmbedFn>> = std::sync::OnceLock::new();

/// The model's vector field, if it has one.
pub(crate) fn vector_field(meta: &ModelMeta) -> Option<&'static FieldMeta> {
    meta.fields.iter().find(|f| matches!(f.ty, FieldType::Vector(_)))
}

/// GET /api/<table>/similar?text=...&k=10: the rows closest in meaning to `text`, through the
/// same access rules, owned_by and row permissions as a list. Needs `App::embed`.
async fn similar(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    Path(table): Path<String>,
    UrlQuery(p): UrlQuery<HashMap<String, String>>,
) -> Reply {
    let (meta, opts) = endpoint(&site, &table, &user, &scope, false)?;
    let (Some(field), Some(embed)) = (vector_field(meta), EMBED.get()) else {
        return Err(problem(StatusCode::NOT_FOUND, "Not found."));
    };
    let text = param(&p, "text").unwrap_or("").trim().to_string();
    if text.is_empty() || text.chars().count() > 2000 {
        return Err(problem(StatusCode::BAD_REQUEST, "Give `text`, up to 2000 characters."));
    }
    let k: usize = param(&p, "k").and_then(|v| v.parse().ok()).unwrap_or(10).clamp(1, 50);
    let to = crate::Vector(embed(text).await.map_err(fail)?);
    let FieldType::Vector(n) = field.ty else { unreachable!() };
    if to.0.len() != n as usize {
        eprintln!("rangoli: App::embed returned {} numbers but {}.{} holds {n}", to.0.len(), meta.table, field.name);
        return Err(problem(StatusCode::INTERNAL_SERVER_ERROR, "The embedding has the wrong size for this field."));
    }
    let mut q = Query::new(meta);
    q.filter.extend(restrict(opts, &user));
    let found = q.nearest(field.name, &to, k).await.map_err(fail)?;
    let mut results: Vec<JsonValue> = found
        .iter()
        .map(|(id, vals, dist)| {
            let mut row = to_json(meta, *id, vals);
            row["distance"] = json!(dist);
            row
        })
        .collect();
    attach_links(meta, &mut results).await?;
    Ok(Json(json!({ "results": results })).into_response())
}

async fn detail(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    Path((table, id)): Path<(String, i64)>,
    UrlQuery(p): UrlQuery<HashMap<String, String>>,
) -> Reply {
    let (meta, opts) = endpoint(&site, &table, &user, &scope, false)?;
    let mut one = [fetch(meta, id, restrict(opts, &user)).await?];
    expand(&site, meta, &user, &scope, &mut one, param(&p, "expand")).await?;
    let [row] = one;
    Ok(Json(row).into_response())
}

/// A token with a daily write budget spends one per write; when it's gone, 429 until midnight UTC.
async fn spend(scope: &ScopeExt, dry: bool) -> Result<(), Response> {
    let Some(axum::Extension(scope)) = scope else { return Ok(()) };
    if !dry && crate::auth::agents_paused().await.map_err(fail)? {
        return Err(problem(StatusCode::SERVICE_UNAVAILABLE, PAUSED));
    }
    if dry || crate::auth::spend_write_budget(scope).await.map_err(fail)? {
        return Ok(());
    }
    let until = 86_400 - crate::auth::now().rem_euclid(86_400);
    let body = Json(
        json!({ "detail": format!("This token's daily limit of {} changes is used up; it resets at midnight UTC.", scope.max_writes_per_day.unwrap_or(0)) }),
    );
    Err((StatusCode::TOO_MANY_REQUESTS, [(axum::http::header::RETRY_AFTER, until.to_string())], body).into_response())
}

pub(crate) const PAUSED: &str = "An admin has paused changes by AI agents and API tokens. Reading still works.";

/// `?dry_run=1`: do the write in a transaction, report it, roll it back.
fn dry_run(uri: &axum::http::Uri) -> bool {
    uri.query().is_some_and(|q| q.split('&').any(|kv| kv == "dry_run=1" || kv == "dry_run=true"))
}

/// Inside a dry run's transaction: capture the row as it would be saved, then abort.
async fn roll_back_if(dry: bool, meta: &'static ModelMeta, id: i64) -> crate::Result<()> {
    if !dry {
        return Ok(());
    }
    let row = fetch(meta, id, vec![]).await.map_err(|_| Error::NotFound)?;
    Err(Error::DryRun(json!({ "dry_run": true, "would_be": row })))
}

/// A rolled-back dry run is a 200 with what would have happened; anything else is an error.
fn dry_result(e: Error) -> Response {
    match e {
        Error::DryRun(report) => (StatusCode::OK, Json(report)).into_response(),
        e => fail(e),
    }
}

/// The model's whole-row rules (`App::validate`), as a 400 like field errors.
async fn row_rules(meta: &'static ModelMeta, id: Option<i64>, cols: &[(&'static str, Value, FieldType)]) -> Result<(), Response> {
    match crate::validators::check(meta, id, cols).await.map_err(fail)? {
        Ok(()) => Ok(()),
        Err(errors) => {
            let body: Map<String, JsonValue> =
                errors.into_iter().map(|(k, v)| (if k == "__all__" { "non_field_errors".to_string() } else { k }, json!([v]))).collect();
            Err((StatusCode::BAD_REQUEST, Json(JsonValue::Object(body))).into_response())
        }
    }
}

/// `?expand=author_id,editor_id`: embed each foreign key's row as `"author": {...}`, one query
/// per field. Only for related tables this caller may read through the API, with their access
/// rules, owned_by, row permissions and private fields, so expand never shows more than
/// fetching the row directly would. A row the caller can't see comes back as null.
async fn expand(
    site: &Site,
    meta: &'static ModelMeta,
    user: &CurrentUser,
    scope: &ScopeExt,
    rows: &mut [JsonValue],
    fields: Option<&str>,
) -> Result<(), Response> {
    let Some(fields) = fields.filter(|f| !f.is_empty()) else { return Ok(()) };
    for name in fields.split(',').map(str::trim).filter(|n| !n.is_empty()).take(10) {
        let bad = || {
            problem(
                StatusCode::BAD_REQUEST,
                &format!("Can't expand `{name}`: it must be a foreign key to a table you can read through the API."),
            )
        };
        let f = meta.field(name).filter(|f| f.exposed()).ok_or_else(bad)?;
        let target = f.fk.ok_or_else(bad)?;
        let (tmeta, topts) = endpoint(site, target, user, scope, false).map_err(|_| bad())?;
        let ids: Vec<Value> = rows.iter().filter_map(|r| r[name].as_i64()).map(Value::Int).collect();
        let mut q = Query::new(tmeta);
        q.filter.push(Node::In("id", ids, FieldType::Int));
        q.filter.extend(restrict(topts, user));
        let found: HashMap<i64, JsonValue> = q.rows().await.map_err(fail)?.iter().map(|(id, v)| (*id, to_json(tmeta, *id, v))).collect();
        let key = name.strip_suffix("_id").unwrap_or(name).to_string();
        for r in rows.iter_mut() {
            let related = r[name].as_i64().and_then(|id| found.get(&id).cloned()).unwrap_or(JsonValue::Null);
            if let Some(obj) = r.as_object_mut() {
                obj.insert(key.clone(), related);
            }
        }
    }
    Ok(())
}

/// How a write arrived, for the admin History. Set by the MCP server on its internal requests
/// (a request extension, so no client can claim it); plain API calls have none.
#[derive(Clone, Debug)]
pub(crate) struct Via(pub(crate) String);

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
/// The row as it is now, as JSON for undo: `{field: value}`, encrypted fields still encrypted.
async fn snapshot(meta: &'static ModelMeta, id: i64) -> crate::Result<String> {
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    let (_, vals) = q.rows().await?.pop().ok_or(Error::NotFound)?;
    let mut obj = Map::new();
    for (f, v) in meta.fields.iter().zip(vals) {
        let v = match (f.ty, v) {
            (FieldType::Encrypted, Value::Text(s)) => Value::Text(crate::crypt::encrypt(&s)),
            (_, v) => v,
        };
        obj.insert(f.name.into(), serde_json::to_value(v).unwrap_or(JsonValue::Null));
    }
    Ok(JsonValue::Object(obj).to_string())
}

async fn audit(
    user: &CurrentUser,
    meta: &'static ModelMeta,
    id: i64,
    action: i64,
    verb: &str,
    via: Option<Via>,
    before: Option<String>,
) -> crate::Result<()> {
    let Some(user_id) = user.0.as_ref().and_then(|u| u.id) else { return Ok(()) };
    let mut entry = crate::admin::LogEntry {
        id: None,
        user_id,
        table_name: meta.table.into(),
        object_id: id,
        object_repr: repr_of(meta, id).await?.chars().take(200).collect(),
        action,
        message: format!("{verb} via {}.", via.as_ref().map_or("the API", |v| v.0.as_str())),
        at: auth::now(),
        snapshot: before,
    };
    entry.save().await?;
    crate::webhooks::fire(&entry).await
}

/// For a token that needs approval, store this (already validated) write as a pending change
/// and answer 202; `None` means go ahead and write.
#[allow(clippy::too_many_arguments)]
async fn hold(
    scope: &ScopeExt,
    user: &CurrentUser,
    via: &Option<Via>,
    table: &str,
    action: &str,
    id: Option<i64>,
    body: Option<&JsonValue>,
) -> Result<Option<Response>, Response> {
    let (Some(axum::Extension(scope)), Some(u)) = (scope, &user.0) else { return Ok(None) };
    if !scope.needs_approval {
        return Ok(None);
    }
    let via = via.as_ref().map(|v| v.0.as_str());
    let pending = crate::approvals::queue(u, scope, via, table, action, id, body).await.map_err(fail)?;
    let detail = "Waiting for a person to approve this in the admin (Pending changes).";
    Ok(Some((StatusCode::ACCEPTED, Json(json!({ "pending": true, "approval_id": pending, "detail": detail }))).into_response()))
}

async fn create(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    via: ViaExt,
    Path(table): Path<String>,
    uri: axum::http::Uri,
    Json(body): Json<JsonValue>,
) -> Reply {
    let via = via.map(|v| v.0);
    let dry = dry_run(&uri);
    let (meta, opts) = endpoint(&site, &table, &user, &scope, true)?;
    if opts.owner.is_some() && user.0.is_none() {
        // An owned row needs an owner; with `write(Access::Public)` the body could otherwise name anyone.
        return Err(problem(StatusCode::UNAUTHORIZED, "Authentication credentials were not provided."));
    }
    spend(&scope, dry).await?;
    let body = claim(opts, &user, &body, true);
    let cols = columns(meta, &body, true, false).await?;
    row_rules(meta, None, &cols).await?;
    let links = body_links(meta, &body)?;
    if !dry {
        if let Some(held) = hold(&scope, &user, &via, &table, "create", None, Some(&body)).await? {
            return Ok(held);
        }
    }
    let id = crate::atomic(async {
        let id = orm::insert_row(meta.table, &cols).await?;
        for (through, ids) in &links {
            orm::set_links(through, id, ids.as_deref().unwrap_or_default()).await?;
        }
        still_visible(meta, opts, &user, id).await?;
        audit(&user, meta, id, 1, "Added", via, None).await?;
        roll_back_if(dry, meta, id).await?;
        Ok(id)
    })
    .await
    .map_err(dry_result)?;
    Ok((StatusCode::CREATED, Json(fetch(meta, id, vec![]).await?)).into_response())
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
    dry: bool,
) -> Reply {
    let (meta, opts) = endpoint(site, table, user, scope, true)?;
    fetch(meta, id, restrict(opts, user)).await?; // 404 before validation, like DRF (and for others' rows)
    spend(scope, dry).await?;
    let body = &claim(opts, user, body, false);
    let cols = columns(meta, body, false, partial).await?;
    row_rules(meta, Some(id), &cols).await?;
    let links = body_links(meta, body)?;
    if !dry {
        if let Some(held) = hold(scope, user, &via, table, if partial { "update" } else { "replace" }, Some(id), Some(body)).await? {
            return Ok(held);
        }
    }
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    crate::atomic(async {
        let before = snapshot(meta, id).await?;
        q.update(&cols).await?;
        for (through, ids) in &links {
            match ids {
                Some(ids) => orm::set_links(through, id, ids).await?,
                None if !partial => orm::set_links(through, id, &[]).await?,
                None => {}
            }
        }
        still_visible(meta, opts, user, id).await?;
        audit(user, meta, id, 2, "Changed", via, Some(before)).await?;
        roll_back_if(dry, meta, id).await?;
        Ok(())
    })
    .await
    .map_err(dry_result)?;
    Ok(Json(fetch(meta, id, vec![]).await?).into_response())
}

async fn replace(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    via: ViaExt,
    Path((table, id)): Path<(String, i64)>,
    uri: axum::http::Uri,
    Json(body): Json<JsonValue>,
) -> Reply {
    update(&site, &user, &scope, via.map(|v| v.0), &table, id, &body, false, dry_run(&uri)).await
}

async fn patch(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    via: ViaExt,
    Path((table, id)): Path<(String, i64)>,
    uri: axum::http::Uri,
    Json(body): Json<JsonValue>,
) -> Reply {
    update(&site, &user, &scope, via.map(|v| v.0), &table, id, &body, true, dry_run(&uri)).await
}

async fn remove(
    State(site): S,
    user: CurrentUser,
    scope: ScopeExt,
    via: ViaExt,
    Path((table, id)): Path<(String, i64)>,
    uri: axum::http::Uri,
) -> Reply {
    let (meta, opts) = endpoint(&site, &table, &user, &scope, true)?;
    let via = via.map(|v| v.0);
    let dry = dry_run(&uri);
    spend(&scope, dry).await?;
    if scope.as_ref().is_some_and(|s| s.needs_approval) {
        fetch(meta, id, restrict(opts, &user)).await?; // only queue a delete of a row the caller can see
    }
    if !dry {
        if let Some(held) = hold(&scope, &user, &via, &table, "delete", Some(id), None).await? {
            return Ok(held);
        }
    }
    let mut q = Query::new(meta);
    q.filter.push(by_id(id));
    q.filter.extend(restrict(opts, &user));
    // Logged first (it needs the row's name); a delete that finds nothing rolls the entry back.
    crate::atomic(async {
        let before = snapshot(meta, id).await?;
        audit(&user, meta, id, 3, "Deleted", via, Some(before)).await?;
        let row = fetch(meta, id, vec![]).await.ok();
        match q.delete().await? {
            0 => Err(Error::NotFound),
            _ if dry => Err(Error::DryRun(json!({ "dry_run": true, "would_delete": row }))),
            _ => Ok(()),
        }
    })
    .await
    .map_err(dry_result)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------- OpenAPI

pub(crate) fn field_schema(f: &FieldMeta) -> JsonValue {
    let mut s = match f.ty {
        FieldType::Opaque => {
            json!({ "type": "string", "readOnly": true, "description": "A value of a database type Rangoli shows as text" })
        }
        FieldType::Int => json!({ "type": "integer", "format": "int64" }),
        FieldType::Float => json!({ "type": "number", "format": "double" }),
        FieldType::Bool => json!({ "type": "boolean" }),
        FieldType::Varchar(n) => json!({ "type": "string", "maxLength": n }),
        FieldType::Text => json!({ "type": "string" }),
        FieldType::Encrypted => json!({ "type": "string", "description": "Stored encrypted; can't be searched or filtered" }),
        FieldType::Vector(n) => json!({ "type": "array", "items": { "type": "number" }, "minItems": n, "maxItems": n, "readOnly": true }),
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
    if let Some(h) = f.help {
        s["description"] = json!(match s["description"].as_str() {
            Some(d) => format!("{h} ({d})"),
            None => h.to_string(),
        });
    }
    s
}

/// OpenAPI 3.0 for every exposed model, generated from the model metadata.
async fn schema(State(site): S) -> Json<JsonValue> {
    let (mut paths, mut schemas) = (Map::new(), Map::new());
    let mut tables: Vec<_> = site.models.values().collect();
    tables.sort_by_key(|(m, _)| m.table);
    schemas.insert("Error".into(), json!({ "type": "object", "properties": { "detail": { "type": "string" } }, "required": ["detail"] }));
    schemas.insert(
        "ValidationErrors".into(),
        json!({ "type": "object", "additionalProperties": { "type": "array", "items": { "type": "string" } } }),
    );
    schemas.insert(
        "Pending".into(),
        json!({ "type": "object", "required": ["pending", "approval_id", "detail"], "properties": {
        "pending": { "type": "boolean" }, "approval_id": { "type": "integer" }, "detail": { "type": "string" } } }),
    );
    for (meta, _) in tables {
        let mut props = Map::new();
        props.insert("id".into(), json!({ "type": "integer", "format": "int64", "readOnly": true }));
        for f in meta.fields.iter().filter(|f| !f.private) {
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
            .filter(|f| !f.null && !f.is_auto() && !f.private && f.ty != FieldType::Bool && f.default.is_none())
            .map(|f| f.name)
            .collect();
        schemas.insert(format!("{}Patch", meta.name), json!({ "type": "object", "properties": props.clone() }));
        schemas.insert(meta.name.into(), json!({ "type": "object", "properties": props, "required": required }));
        let item = json!({ "$ref": format!("#/components/schemas/{}", meta.name) });
        let page = json!({ "type": "object", "properties": {
            "count": { "type": "integer" }, "next": { "type": "string", "nullable": true },
            "previous": { "type": "string", "nullable": true }, "results": { "type": "array", "items": item } } });
        let ok = |schema: JsonValue| json!({ "description": "OK", "content": { "application/json": { "schema": schema } } });
        let body = json!({ "required": true, "content": { "application/json": { "schema": item } } });
        let patch_body = json!({ "required": true, "content": { "application/json": {
            "schema": { "$ref": format!("#/components/schemas/{}Patch", meta.name) } } } });
        // Every error the handlers can return, so generated clients get typed errors too.
        let err = |d: &str, schema: &str| json!({ "description": d, "content": { "application/json": { "schema": { "$ref": format!("#/components/schemas/{schema}") } } } });
        let errors = |extra: &[(&str, JsonValue)]| {
            let mut r = Map::new();
            r.insert("401".into(), err("No or invalid credentials", "Error"));
            r.insert("403".into(), err("Not allowed (permissions, a read-only token, or a table outside its scope)", "Error"));
            r.insert("429".into(), err("Rate limited; see the Retry-After header", "Error"));
            for (k, v) in extra {
                r.insert((*k).into(), v.clone());
            }
            r
        };
        let invalid = err("Validation errors: field name to messages, or non_field_errors", "ValidationErrors");
        let missing = err("Not found", "Error");
        let pending = err("Queued for a person to approve (tokens that ask first)", "Pending");
        let responses = |ok_code: &str, ok_body: JsonValue, extra: &[(&str, JsonValue)]| {
            let mut r = errors(extra);
            r.insert(ok_code.into(), ok_body);
            JsonValue::Object(r)
        };
        let id_param = json!([{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer" } }]);
        let list_params = json!([
            { "name": "limit", "in": "query", "schema": { "type": "integer", "maximum": MAX_LIMIT } },
            { "name": "offset", "in": "query", "schema": { "type": "integer" } },
            { "name": "ordering", "in": "query", "schema": { "type": "string" }, "description": "Field name, `-` prefix for descending" },
            { "name": "search", "in": "query", "schema": { "type": "string" } },
            { "name": "expand", "in": "query", "schema": { "type": "string" }, "description": "Foreign keys to embed as whole rows, comma-separated (e.g. author_id)" },
            { "name": "cursor", "in": "query", "schema": { "type": "string" }, "description": "Keyset pagination by id: pass `next` from the previous page; no count is computed" }
        ]);
        paths.insert(format!("/api/{}/", meta.table), json!({
            "get": { "summary": format!("List {}", meta.name), "parameters": list_params, "responses": responses("200", ok(page), &[("400", invalid.clone())]) },
            "post": { "summary": format!("Create a {}", meta.name), "requestBody": body,
                "responses": responses("201", ok(item.clone()), &[("400", invalid.clone()), ("202", pending.clone())]) },
        }));
        let fields_param = |name: &str, d: &str| json!({ "name": name, "in": "query", "schema": { "type": "string" }, "description": d });
        paths.insert(format!("/api/{}/aggregate", meta.table), json!({
            "get": { "summary": format!("Count and total {} rows", meta.name),
                "description": "Takes the list's filters and search; totals cover exactly the rows that list would return.",
                "parameters": [fields_param("sum", "Number fields to add up, comma-separated"), fields_param("avg", "Number fields to average"),
                    fields_param("min", "Fields to find the smallest value of"), fields_param("max", "Fields to find the largest value of"),
                    fields_param("group_by", "One field: `groups`, each with its value, count and totals")],
                "responses": responses("200", ok(json!({ "type": "object", "properties": {
                    "count": { "type": "integer" }, "sum": { "type": "object" }, "avg": { "type": "object" }, "min": { "type": "object" }, "max": { "type": "object" },
                    "groups": { "type": "array", "items": { "type": "object" } }, "truncated": { "type": "boolean" } } })), &[("400", invalid.clone())]) },
        }));
        paths.insert(format!("/api/{}/{{id}}", meta.table), json!({
            "parameters": id_param,
            "get": { "summary": format!("Get a {}", meta.name), "responses": responses("200", ok(item.clone()), &[("404", missing.clone())]) },
            "put": { "summary": format!("Replace a {}", meta.name), "requestBody": body,
                "responses": responses("200", ok(item.clone()), &[("400", invalid.clone()), ("404", missing.clone()), ("202", pending.clone())]) },
            "patch": { "summary": format!("Update some fields of a {}", meta.name), "requestBody": patch_body,
                "responses": responses("200", ok(item.clone()), &[("400", invalid.clone()), ("404", missing.clone()), ("202", pending.clone())]) },
            "delete": { "summary": format!("Delete a {}", meta.name),
                "responses": responses("204", json!({ "description": "Deleted" }), &[("404", missing), ("202", pending)]) },
        }));
    }
    Json(json!({
        "openapi": "3.0.3",
        "info": { "title": "Rangoli API", "version": env!("CARGO_PKG_VERSION") },
        "paths": paths,
        "components": {
            "schemas": schemas,
            "securitySchemes": { "token": { "type": "http", "scheme": "bearer", "description": "An API token (`createtoken`), sent as `Authorization: Bearer rgl_...`" } },
        },
        "security": [{ "token": [] }, {}],
    }))
}

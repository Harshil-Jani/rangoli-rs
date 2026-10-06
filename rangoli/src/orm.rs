//! The ORM: typed columns, a lazy `QuerySet`, and metadata-level row
//! operations shared with the admin. One code path serves Postgres, MySQL and
//! SQLite through sqlx's `Any` driver; only SQL spelling differs per dialect.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sqlx::any::{AnyArguments, AnyPoolOptions, AnyQueryResult};
use sqlx::{Any, AnyPool, Row};
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::{BitAnd, BitOr, Not};
use std::sync::OnceLock;

pub use sqlx::any::AnyRow;

// ---------------------------------------------------------------- metadata

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Int,
    Float,
    Bool,
    Varchar(u32),
    Text,
}

#[derive(Debug)]
pub struct FieldMeta {
    pub name: &'static str,
    pub ty: FieldType,
    pub null: bool,
    pub unique: bool,
    /// Stored as an argon2 hash; the admin never displays it.
    pub password: bool,
    /// Table this column references (always its `id`).
    pub fk: Option<&'static str>,
    /// `ON DELETE CASCADE` instead of the default, which protects referenced rows.
    pub cascade: bool,
}

#[derive(Debug)]
pub struct ModelMeta {
    pub name: &'static str,
    pub table: &'static str,
    /// Field used to label rows in the admin (`__str__` in Django).
    pub display: Option<&'static str>,
    /// Every column except `id`, which every model has.
    pub fields: &'static [FieldMeta],
}

impl ModelMeta {
    pub fn field(&self, name: &str) -> Option<&'static FieldMeta> {
        self.fields.iter().find(|f| f.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_owned())
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

pub trait FromValue: Sized {
    fn from_value(v: Value) -> Result<Self>;
}

fn mismatch<T>(v: Value, want: &str) -> Result<T> {
    Err(Error::Decode(format!("expected {want}, got {v:?}")))
}

impl FromValue for i64 {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Int(i) => Ok(i),
            v => mismatch(v, "integer"),
        }
    }
}
impl FromValue for f64 {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Float(f) => Ok(f),
            Value::Int(i) => Ok(i as f64),
            v => mismatch(v, "float"),
        }
    }
}
impl FromValue for bool {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Bool(b) => Ok(b),
            Value::Int(i) => Ok(i != 0),
            v => mismatch(v, "bool"),
        }
    }
}
impl FromValue for String {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Text(s) => Ok(s),
            v => mismatch(v, "text"),
        }
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Null => Ok(None),
            v => T::from_value(v).map(Some),
        }
    }
}

/// Rust types a column can hold; maps to the type used when binding NULLs.
pub trait Kind {
    const KIND: FieldType;
}
impl Kind for i64 {
    const KIND: FieldType = FieldType::Int;
}
impl Kind for f64 {
    const KIND: FieldType = FieldType::Float;
}
impl Kind for bool {
    const KIND: FieldType = FieldType::Bool;
}
impl Kind for String {
    const KIND: FieldType = FieldType::Text;
}

// ---------------------------------------------------------------- database

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Mysql,
    Sqlite,
}

impl Dialect {
    pub fn from_url(url: &str) -> Result<Self> {
        match url.split(':').next().unwrap_or("") {
            "postgres" | "postgresql" => Ok(Dialect::Postgres),
            "mysql" | "mariadb" => Ok(Dialect::Mysql),
            "sqlite" => Ok(Dialect::Sqlite),
            _ => Err(Error::Config(format!("unsupported database URL `{url}` (use postgres://, mysql:// or sqlite://)"))),
        }
    }

    pub fn quote(self, ident: &str) -> String {
        match self {
            Dialect::Mysql => format!("`{ident}`"),
            _ => format!("\"{ident}\""),
        }
    }

    /// Placeholder for the `n`th (1-based) bound parameter.
    pub fn ph(self, n: usize) -> String {
        match self {
            Dialect::Postgres => format!("${n}"),
            _ => "?".into(),
        }
    }
}

pub struct Db {
    pub pool: AnyPool,
    pub dialect: Dialect,
}

static DB: OnceLock<Db> = OnceLock::new();

/// Connect the process-wide pool. Later calls return the existing pool.
pub async fn connect(url: &str) -> Result<&'static Db> {
    if let Some(db) = DB.get() {
        return Ok(db);
    }
    sqlx::any::install_default_drivers();
    let dialect = Dialect::from_url(url)?;
    let pool = AnyPoolOptions::new()
        .max_connections(10)
        .after_connect(move |conn, _| {
            Box::pin(async move {
                // SQLite ships with foreign key enforcement off; turn it on like Django does.
                if dialect == Dialect::Sqlite {
                    sqlx::query("PRAGMA foreign_keys = ON").execute(conn).await?;
                }
                Ok(())
            })
        })
        .connect(url)
        .await?;
    // ponytail: one global pool per process, like Django's default alias; multi-db routing when someone needs it.
    let _ = DB.set(Db { pool, dialect });
    Ok(DB.get().unwrap())
}

pub fn db() -> &'static Db {
    DB.get().expect("rangoli: database not connected (App::run connects it; in tests call rangoli::orm::connect)")
}

pub(crate) type Params = Vec<(Value, FieldType)>;
type AnyQuery<'q> = sqlx::query::Query<'q, Any, AnyArguments<'q>>;

fn bind<'q>(q: AnyQuery<'q>, v: &Value, ty: FieldType) -> AnyQuery<'q> {
    match v {
        Value::Null => match ty {
            FieldType::Int => q.bind(None::<i64>),
            FieldType::Float => q.bind(None::<f64>),
            FieldType::Bool => q.bind(None::<bool>),
            FieldType::Varchar(_) | FieldType::Text => q.bind(None::<String>),
        },
        Value::Bool(b) => q.bind(*b),
        Value::Int(i) => q.bind(*i),
        Value::Float(f) => q.bind(*f),
        Value::Text(s) => q.bind(s.clone()),
    }
}

fn build<'q>(sql: &'q str, params: &Params) -> AnyQuery<'q> {
    params.iter().fold(sqlx::query(sql), |q, (v, t)| bind(q, v, *t))
}

pub(crate) async fn fetch_all(sql: &str, params: &Params) -> Result<Vec<AnyRow>> {
    Ok(build(sql, params).fetch_all(&db().pool).await?)
}

pub(crate) async fn execute(sql: &str, params: &Params) -> Result<AnyQueryResult> {
    Ok(build(sql, params).execute(&db().pool).await?)
}

/// Read one column as a `Value`, tolerating how each driver reports ints and bools.
pub fn read(row: &AnyRow, col: &str, ty: FieldType) -> Result<Value> {
    fn int(row: &AnyRow, col: &str) -> Result<Option<i64>> {
        if let Ok(v) = row.try_get::<Option<i64>, _>(col) {
            return Ok(v);
        }
        if let Ok(v) = row.try_get::<Option<i32>, _>(col) {
            return Ok(v.map(i64::from));
        }
        Ok(row.try_get::<Option<i16>, _>(col)?.map(i64::from))
    }
    Ok(match ty {
        FieldType::Int => int(row, col)?.map_or(Value::Null, Value::Int),
        FieldType::Float => match row.try_get::<Option<f64>, _>(col) {
            Ok(v) => v.map_or(Value::Null, Value::Float),
            Err(_) => int(row, col)?.map_or(Value::Null, |i| Value::Float(i as f64)),
        },
        FieldType::Bool => match row.try_get::<Option<bool>, _>(col) {
            Ok(v) => v.map_or(Value::Null, Value::Bool),
            Err(_) => int(row, col)?.map_or(Value::Null, |i| Value::Bool(i != 0)),
        },
        FieldType::Varchar(_) | FieldType::Text => match row.try_get::<Option<String>, _>(col) {
            Ok(v) => v.map_or(Value::Null, Value::Text),
            // MySQL TEXT columns surface as BLOB through the Any driver.
            Err(_) => match row.try_get::<Option<Vec<u8>>, _>(col)? {
                Some(b) => Value::Text(String::from_utf8(b).map_err(|e| Error::Decode(e.to_string()))?),
                None => Value::Null,
            },
        },
    })
}

// ---------------------------------------------------------------- expressions

#[derive(Clone, Debug)]
pub(crate) enum Node {
    Cmp(&'static str, &'static str, Value, FieldType),
    In(&'static str, Vec<Value>, FieldType),
    IsNull(&'static str, bool),
    /// Case-insensitive LIKE; the pattern is already escaped with `!`.
    Like(&'static str, String),
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
}

impl Node {
    pub(crate) fn render(&self, d: Dialect, sql: &mut String, p: &mut Params) {
        let mut push = |sql: &mut String, v: Value, t: FieldType| {
            p.push((v, t));
            sql.push_str(&d.ph(p.len()));
        };
        match self {
            Node::Cmp(c, op, v, t) => {
                sql.push_str(&format!("{} {op} ", d.quote(c)));
                push(sql, v.clone(), *t);
            }
            Node::In(_, vs, _) if vs.is_empty() => sql.push_str("1=0"),
            Node::In(c, vs, t) => {
                sql.push_str(&format!("{} IN (", d.quote(c)));
                for (i, v) in vs.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    push(sql, v.clone(), *t);
                }
                sql.push(')');
            }
            Node::IsNull(c, negate) => sql.push_str(&format!("{} IS {}NULL", d.quote(c), if *negate { "NOT " } else { "" })),
            Node::Like(c, pat) => {
                sql.push_str(&format!("LOWER({}) LIKE ", d.quote(c)));
                push(sql, Value::Text(pat.to_lowercase()), FieldType::Text);
                sql.push_str(" ESCAPE '!'");
            }
            Node::And(ns) | Node::Or(ns) if ns.is_empty() => sql.push_str(if matches!(self, Node::And(_)) { "1=1" } else { "1=0" }),
            Node::And(ns) | Node::Or(ns) => {
                let joiner = if matches!(self, Node::And(_)) { " AND " } else { " OR " };
                sql.push('(');
                for (i, n) in ns.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(joiner);
                    }
                    n.render(d, sql, p);
                }
                sql.push(')');
            }
            Node::Not(n) => {
                sql.push_str("NOT (");
                n.render(d, sql, p);
                sql.push(')');
            }
        }
    }
}

pub(crate) fn like_escape(s: &str) -> String {
    s.replace('!', "!!").replace('%', "!%").replace('_', "!_")
}

/// A filter over model `M`. Combine with `&`, `|` and `!`; mixing models is a type error.
pub struct Expr<M> {
    pub(crate) node: Node,
    _m: PhantomData<fn() -> M>,
}

impl<M> Expr<M> {
    fn new(node: Node) -> Self {
        Expr { node, _m: PhantomData }
    }
}
impl<M> BitAnd for Expr<M> {
    type Output = Expr<M>;
    fn bitand(self, rhs: Self) -> Self {
        Expr::new(Node::And(vec![self.node, rhs.node]))
    }
}
impl<M> BitOr for Expr<M> {
    type Output = Expr<M>;
    fn bitor(self, rhs: Self) -> Self {
        Expr::new(Node::Or(vec![self.node, rhs.node]))
    }
}
impl<M> Not for Expr<M> {
    type Output = Expr<M>;
    fn not(self) -> Self {
        Expr::new(Node::Not(Box::new(self.node)))
    }
}

pub struct Order<M>(pub(crate) &'static str, pub(crate) bool, PhantomData<fn() -> M>);
pub struct Assign<M>(pub(crate) &'static str, pub(crate) Value, pub(crate) FieldType, PhantomData<fn() -> M>);

/// A typed column handle, generated by `#[derive(Model)]` as `Post::TITLE`.
pub struct Col<M, T> {
    name: &'static str,
    _p: PhantomData<fn() -> (M, T)>,
}

impl<M, T> Clone for Col<M, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<M, T> Copy for Col<M, T> {}

impl<M, T> Col<M, T> {
    pub const fn new(name: &'static str) -> Self {
        Col { name, _p: PhantomData }
    }
    pub fn name(self) -> &'static str {
        self.name
    }
    pub fn is_null(self) -> Expr<M> {
        Expr::new(Node::IsNull(self.name, false))
    }
    pub fn is_not_null(self) -> Expr<M> {
        Expr::new(Node::IsNull(self.name, true))
    }
    pub fn asc(self) -> Order<M> {
        Order(self.name, false, PhantomData)
    }
    pub fn desc(self) -> Order<M> {
        Order(self.name, true, PhantomData)
    }
}

impl<M, T: Kind + Into<Value>> Col<M, T> {
    fn cmp(self, op: &'static str, v: T) -> Expr<M> {
        Expr::new(Node::Cmp(self.name, op, v.into(), T::KIND))
    }
    pub fn eq(self, v: impl Into<T>) -> Expr<M> {
        self.cmp("=", v.into())
    }
    pub fn ne(self, v: impl Into<T>) -> Expr<M> {
        self.cmp("<>", v.into())
    }
    pub fn gt(self, v: impl Into<T>) -> Expr<M> {
        self.cmp(">", v.into())
    }
    pub fn gte(self, v: impl Into<T>) -> Expr<M> {
        self.cmp(">=", v.into())
    }
    pub fn lt(self, v: impl Into<T>) -> Expr<M> {
        self.cmp("<", v.into())
    }
    pub fn lte(self, v: impl Into<T>) -> Expr<M> {
        self.cmp("<=", v.into())
    }
    pub fn is_in<I: Into<T>>(self, vs: impl IntoIterator<Item = I>) -> Expr<M> {
        Expr::new(Node::In(self.name, vs.into_iter().map(|v| v.into().into()).collect(), T::KIND))
    }
    /// For `update()`: `Post::PUBLISHED.set(true)`. Pass `None` through `set_null`.
    pub fn set(self, v: impl Into<T>) -> Assign<M> {
        Assign(self.name, v.into().into(), T::KIND, PhantomData)
    }
    pub fn set_null(self) -> Assign<M> {
        Assign(self.name, Value::Null, T::KIND, PhantomData)
    }
}

impl<M> Col<M, String> {
    /// Case-insensitive substring match (Django's `icontains`).
    pub fn contains(self, s: &str) -> Expr<M> {
        Expr::new(Node::Like(self.name, format!("%{}%", like_escape(s))))
    }
    /// Case-insensitive prefix match (Django's `istartswith`).
    pub fn starts_with(self, s: &str) -> Expr<M> {
        Expr::new(Node::Like(self.name, format!("{}%", like_escape(s))))
    }
}

// ---------------------------------------------------------------- untyped query

/// Metadata-level query used by `QuerySet` and by the admin.
#[derive(Clone)]
pub(crate) struct Query {
    pub table: &'static str,
    pub fields: &'static [FieldMeta],
    pub filter: Vec<Node>,
    pub order: Vec<(&'static str, bool)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

impl Query {
    pub fn new(meta: &'static ModelMeta) -> Self {
        Query { table: meta.table, fields: meta.fields, filter: vec![], order: vec![], limit: None, offset: None }
    }

    fn push_where(&self, d: Dialect, sql: &mut String, p: &mut Params) {
        if !self.filter.is_empty() {
            sql.push_str(" WHERE ");
            match &self.filter[..] {
                [one] => one.render(d, sql, p),
                many => Node::And(many.to_vec()).render(d, sql, p),
            }
        }
    }

    pub fn select_sql(&self, d: Dialect, p: &mut Params) -> String {
        let cols: Vec<String> = std::iter::once("id").chain(self.fields.iter().map(|f| f.name)).map(|c| d.quote(c)).collect();
        let mut sql = format!("SELECT {} FROM {}", cols.join(", "), d.quote(self.table));
        self.push_where(d, &mut sql, p);
        if !self.order.is_empty() {
            let parts: Vec<String> =
                self.order.iter().map(|(c, desc)| format!("{}{}", d.quote(c), if *desc { " DESC" } else { "" })).collect();
            sql.push_str(&format!(" ORDER BY {}", parts.join(", ")));
        }
        match (self.limit, self.offset) {
            (Some(l), Some(o)) => sql.push_str(&format!(" LIMIT {l} OFFSET {o}")),
            (Some(l), None) => sql.push_str(&format!(" LIMIT {l}")),
            // All three databases need a LIMIT before OFFSET.
            (None, Some(o)) => sql.push_str(&format!(" LIMIT {} OFFSET {o}", i64::MAX)),
            (None, None) => {}
        }
        sql
    }

    pub async fn fetch(&self) -> Result<Vec<AnyRow>> {
        let mut p = vec![];
        let sql = self.select_sql(db().dialect, &mut p);
        fetch_all(&sql, &p).await
    }

    /// `(id, values-in-field-order)` per row, for code that works from metadata.
    pub async fn rows(&self) -> Result<Vec<(i64, Vec<Value>)>> {
        self.fetch()
            .await?
            .iter()
            .map(|r| {
                let id = i64::from_value(read(r, "id", FieldType::Int)?)?;
                let vals = self.fields.iter().map(|f| read(r, f.name, f.ty)).collect::<Result<_>>()?;
                Ok((id, vals))
            })
            .collect()
    }

    pub async fn count(&self) -> Result<i64> {
        let d = db().dialect;
        let (mut p, mut sql) = (vec![], format!("SELECT COUNT(*) AS n FROM {}", d.quote(self.table)));
        self.push_where(d, &mut sql, &mut p);
        let rows = fetch_all(&sql, &p).await?;
        i64::from_value(read(&rows[0], "n", FieldType::Int)?)
    }

    pub async fn delete(&self) -> Result<u64> {
        let d = db().dialect;
        let (mut p, mut sql) = (vec![], format!("DELETE FROM {}", d.quote(self.table)));
        self.push_where(d, &mut sql, &mut p);
        Ok(execute(&sql, &p).await?.rows_affected())
    }

    pub async fn update(&self, sets: &[(&'static str, Value, FieldType)]) -> Result<u64> {
        if sets.is_empty() {
            return Ok(0);
        }
        let d = db().dialect;
        let mut p: Params = vec![];
        let assigns: Vec<String> = sets
            .iter()
            .map(|(c, v, t)| {
                p.push((v.clone(), *t));
                format!("{} = {}", d.quote(c), d.ph(p.len()))
            })
            .collect();
        let mut sql = format!("UPDATE {} SET {}", d.quote(self.table), assigns.join(", "));
        self.push_where(d, &mut sql, &mut p);
        Ok(execute(&sql, &p).await?.rows_affected())
    }
}

/// Insert a row from `(column, value, type)` triples; returns the new id.
pub(crate) async fn insert_row(table: &str, cols: &[(&str, Value, FieldType)]) -> Result<i64> {
    let d = db().dialect;
    let names: Vec<String> = cols.iter().map(|(c, ..)| d.quote(c)).collect();
    let phs: Vec<String> = (1..=cols.len()).map(|i| d.ph(i)).collect();
    let body = match (cols.is_empty(), d) {
        (true, Dialect::Mysql) => "() VALUES ()".to_string(),
        (true, _) => "DEFAULT VALUES".to_string(),
        _ => format!("({}) VALUES ({})", names.join(", "), phs.join(", ")),
    };
    let params: Params = cols.iter().map(|(_, v, t)| (v.clone(), *t)).collect();
    let mut sql = format!("INSERT INTO {} {body}", d.quote(table));
    if d == Dialect::Mysql {
        let res = execute(&sql, &params).await?;
        return res.last_insert_id().ok_or_else(|| Error::Decode("MySQL returned no insert id".into()));
    }
    sql.push_str(&format!(" RETURNING {}", d.quote("id")));
    let rows = fetch_all(&sql, &params).await?;
    i64::from_value(read(&rows[0], "id", FieldType::Int)?)
}

pub(crate) fn by_id(id: i64) -> Node {
    Node::Cmp("id", "=", Value::Int(id), FieldType::Int)
}

// ---------------------------------------------------------------- typed API

pub trait Model: Sized + Send + Sync + Unpin + 'static {
    const TABLE: &'static str;
    fn meta() -> &'static ModelMeta;
    fn from_row(row: &AnyRow) -> Result<Self>;
    fn pk(&self) -> Option<i64>;
    fn set_pk(&mut self, id: i64);
    /// Column values in `meta().fields` order.
    fn values(&self) -> Vec<Value>;

    fn objects() -> QuerySet<Self> {
        QuerySet { q: Query::new(Self::meta()), _m: PhantomData }
    }

    /// Fetch by primary key; `Error::NotFound` (a 404 in handlers) if missing.
    fn get(id: i64) -> impl Future<Output = Result<Self>> + Send {
        let mut qs = Self::objects();
        qs.q.filter.push(by_id(id));
        qs.get()
    }

    /// Fetch many rows by id in one query: the explicit cure for N+1 loops.
    fn in_bulk(ids: impl IntoIterator<Item = i64>) -> impl Future<Output = Result<HashMap<i64, Self>>> + Send {
        let ids: Vec<Value> = ids.into_iter().map(Value::Int).collect();
        let mut qs = Self::objects();
        qs.q.filter.push(Node::In("id", ids, FieldType::Int));
        async move { Ok(qs.all().await?.into_iter().map(|m| (m.pk().unwrap(), m)).collect()) }
    }

    /// INSERT when unsaved (and set `id`), otherwise UPDATE every column.
    fn save(&mut self) -> impl Future<Output = Result<()>> + Send {
        async move {
            let meta = Self::meta();
            let cols: Vec<_> = meta.fields.iter().zip(self.values()).map(|(f, v)| (f.name, v, f.ty)).collect();
            match self.pk() {
                Some(id) => {
                    let mut q = Query::new(meta);
                    q.filter.push(by_id(id));
                    // MySQL reports 0 affected rows for no-op updates, so re-check before calling it missing.
                    if q.update(&cols).await? == 0 && q.count().await? == 0 {
                        return Err(Error::NotFound);
                    }
                }
                None => {
                    let id = insert_row(meta.table, &cols).await?;
                    self.set_pk(id);
                }
            }
            Ok(())
        }
    }

    fn delete(&self) -> impl Future<Output = Result<()>> + Send {
        let id = self.pk();
        async move {
            let mut q = Query::new(Self::meta());
            q.filter.push(by_id(id.ok_or(Error::NotFound)?));
            q.delete().await?;
            Ok(())
        }
    }
}

/// Lazy, chainable query over `M`. Nothing runs until an async terminal method.
pub struct QuerySet<M> {
    pub(crate) q: Query,
    _m: PhantomData<fn() -> M>,
}

impl<M: Model> QuerySet<M> {
    pub fn filter(mut self, e: Expr<M>) -> Self {
        self.q.filter.push(e.node);
        self
    }
    pub fn exclude(mut self, e: Expr<M>) -> Self {
        self.q.filter.push(Node::Not(Box::new(e.node)));
        self
    }
    pub fn order_by(mut self, o: Order<M>) -> Self {
        self.q.order.push((o.0, o.1));
        self
    }
    pub fn limit(mut self, n: u64) -> Self {
        self.q.limit = Some(n);
        self
    }
    pub fn offset(mut self, n: u64) -> Self {
        self.q.offset = Some(n);
        self
    }

    pub async fn all(self) -> Result<Vec<M>> {
        self.q.fetch().await?.iter().map(M::from_row).collect()
    }
    pub async fn first(self) -> Result<Option<M>> {
        Ok(self.limit(1).all().await?.into_iter().next())
    }
    /// Exactly one row, like Django's `get()`.
    pub async fn get(self) -> Result<M> {
        let mut rows = self.limit(2).all().await?;
        match rows.len() {
            0 => Err(Error::NotFound),
            1 => Ok(rows.remove(0)),
            _ => Err(Error::MultipleObjectsReturned),
        }
    }
    pub async fn count(self) -> Result<i64> {
        self.q.count().await
    }
    pub async fn exists(self) -> Result<bool> {
        Ok(self.limit(1).q.fetch().await?.len() == 1)
    }
    pub async fn delete(self) -> Result<u64> {
        self.q.delete().await
    }
    pub async fn update(self, sets: impl IntoIterator<Item = Assign<M>>) -> Result<u64> {
        let sets: Vec<_> = sets.into_iter().map(|a| (a.0, a.1, a.2)).collect();
        self.q.update(&sets).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct M;

    #[test]
    fn renders_dialect_specific_sql() {
        let title: Col<M, String> = Col::new("title");
        let n: Col<M, i64> = Col::new("n");
        let e = (title.contains("50%_off") | n.gt(3)) & !n.is_in([1, 2]);
        let q = Query { table: "t", fields: &[], filter: vec![e.node], order: vec![("n", true)], limit: Some(5), offset: None };

        let mut p = vec![];
        let pg = q.select_sql(Dialect::Postgres, &mut p);
        assert_eq!(
            pg,
            r#"SELECT "id" FROM "t" WHERE ((LOWER("title") LIKE $1 ESCAPE '!' OR "n" > $2) AND NOT ("n" IN ($3, $4))) ORDER BY "n" DESC LIMIT 5"#
        );
        assert_eq!(p[0].0, Value::Text("%50!%!_off%".into()));

        let mut p = vec![];
        let my = q.select_sql(Dialect::Mysql, &mut p);
        assert!(my.starts_with("SELECT `id` FROM `t` WHERE ((LOWER(`title`) LIKE ? ESCAPE '!'"), "{my}");
        assert_eq!(p.len(), 4);
    }

    #[test]
    fn empty_in_matches_nothing() {
        let n: Col<M, i64> = Col::new("n");
        let (mut sql, mut p) = (String::new(), vec![]);
        n.is_in(Vec::<i64>::new()).node.render(Dialect::Sqlite, &mut sql, &mut p);
        assert_eq!(sql, "1=0");
    }
}

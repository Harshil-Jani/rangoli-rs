//! Migrations without the merge dance.
//!
//! Django chains migrations through `dependencies`, so two branches that each
//! add a migration produce a conflict that needs a "merge migration" even when
//! they touch unrelated tables. Here migrations are timestamped JSON files and
//! the database records the *set* of applied names: unrelated migrations from
//! different branches simply both apply. A real conflict (two branches editing
//! the same column) is caught when the files are replayed, naming both files.

use crate::orm::{db, Dialect, FieldType, ModelMeta, Value};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: FieldType,
    #[serde(default, skip_serializing_if = "is_false")]
    pub null: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unique: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fk: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cascade: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unique: bool,
}

/// A table's schema: its columns (without the implicit `id`) and indexes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<Column>,
    pub indexes: Vec<Index>,
}

/// Index names must be stable forever (they live in migration files) and fit
/// every database's identifier limit (63 on Postgres), so long ones end in an FNV-1a hash.
pub fn index_name(table: &str, columns: &[&str], unique: bool) -> String {
    let full = format!("{table}_{}_{}", columns.join("_"), if unique { "uniq" } else { "idx" });
    if full.len() <= 60 {
        return full;
    }
    let hash = full.bytes().fold(0x811c9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x01000193));
    format!("{}_{hash:08x}", &full[..51])
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    CreateTable {
        table: String,
        columns: Vec<Column>,
    },
    DropTable {
        table: String,
    },
    /// `default` fills existing rows when the column is NOT NULL.
    AddColumn {
        table: String,
        column: Column,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<Value>,
    },
    DropColumn {
        table: String,
        column: String,
    },
    AlterColumn {
        table: String,
        column: Column,
    },
    AddIndex {
        table: String,
        index: Index,
    },
    DropIndex {
        table: String,
        name: String,
    },
    /// Escape hatch (Django's RunSQL). Not reflected in model state.
    Sql {
        sql: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Migration {
    #[serde(skip)]
    pub name: String,
    pub operations: Vec<Op>,
}

/// Table name -> schema.
pub type State = BTreeMap<String, Table>;

pub fn columns_of(meta: &ModelMeta) -> Vec<Column> {
    meta.fields
        .iter()
        .map(|f| Column { name: f.name.into(), ty: f.ty, null: f.null, unique: f.unique, fk: f.fk.map(Into::into), cascade: f.cascade })
        .collect()
}

fn indexes_of(meta: &ModelMeta) -> Vec<Index> {
    meta.fields
        .iter()
        .filter(|f| f.index)
        .map(|f| Index { name: index_name(meta.table, &[f.name], false), columns: vec![f.name.into()], unique: false })
        .collect()
}

/// The join table behind a many-to-many relation: one row per pair, each pair once.
pub fn through_table(source: &str, target: &str) -> Table {
    let side = |name: &str, table: &str| Column {
        name: name.into(),
        ty: FieldType::Int,
        null: false,
        unique: false,
        fk: Some(table.into()),
        cascade: true,
    };
    Table {
        columns: vec![side("source_id", source), side("target_id", target)],
        indexes: vec![Index { name: String::new(), columns: vec!["source_id".into(), "target_id".into()], unique: true }],
    }
}

pub fn model_state(models: &[&'static ModelMeta]) -> State {
    let mut state = State::new();
    for m in models {
        state.insert(m.table.to_string(), Table { columns: columns_of(m), indexes: indexes_of(m) });
        for rel in m.m2m {
            let mut t = through_table(m.table, rel.target);
            t.indexes[0].name = index_name(rel.through, &["source_id", "target_id"], true);
            state.insert(rel.through.to_string(), t);
        }
    }
    state
}

/// Apply `op` to `state`, rejecting operations that contradict it.
fn apply(state: &mut State, op: &Op) -> std::result::Result<(), String> {
    let missing = |t: &str| format!("table `{t}` does not exist");
    match op {
        Op::CreateTable { table, columns } => {
            if state.insert(table.clone(), Table { columns: columns.clone(), indexes: vec![] }).is_some() {
                return Err(format!("table `{table}` already exists"));
            }
        }
        Op::DropTable { table } => {
            state.remove(table).ok_or(missing(table))?;
        }
        Op::AddColumn { table, column, .. } => {
            let t = state.get_mut(table).ok_or(missing(table))?;
            if t.columns.iter().any(|c| c.name == column.name) {
                return Err(format!("column `{table}.{}` already exists", column.name));
            }
            t.columns.push(column.clone());
        }
        Op::DropColumn { table, column } => {
            let t = state.get_mut(table).ok_or(missing(table))?;
            let i = t.columns.iter().position(|c| &c.name == column).ok_or(format!("column `{table}.{column}` does not exist"))?;
            if let Some(ix) = t.indexes.iter().find(|ix| ix.columns.contains(column)) {
                return Err(format!("column `{table}.{column}` is still used by index `{}`", ix.name));
            }
            t.columns.remove(i);
        }
        Op::AlterColumn { table, column } => {
            let t = state.get_mut(table).ok_or(missing(table))?;
            let c =
                t.columns.iter_mut().find(|c| c.name == column.name).ok_or(format!("column `{table}.{}` does not exist", column.name))?;
            *c = column.clone();
        }
        Op::AddIndex { table, index } => {
            let t = state.get_mut(table).ok_or(missing(table))?;
            if t.indexes.iter().any(|ix| ix.name == index.name) {
                return Err(format!("index `{}` already exists", index.name));
            }
            if let Some(c) = index.columns.iter().find(|c| !t.columns.iter().any(|col| &col.name == *c)) {
                return Err(format!("index `{}` uses unknown column `{table}.{c}`", index.name));
            }
            t.indexes.push(index.clone());
        }
        Op::DropIndex { table, name } => {
            let t = state.get_mut(table).ok_or(missing(table))?;
            let i = t.indexes.iter().position(|ix| &ix.name == name).ok_or(format!("index `{name}` does not exist"))?;
            t.indexes.remove(i);
        }
        Op::Sql { .. } => {}
    }
    Ok(())
}

/// Operations that turn `from` into `to`: index drops first, then tables (foreign key
/// targets before the tables that reference them) and columns, then new indexes.
pub fn diff(from: &State, to: &State) -> Vec<Op> {
    let (mut drops, mut ops, mut adds) = (vec![], vec![], vec![]);
    let mut pending: Vec<&String> = to.keys().filter(|t| !from.contains_key(*t)).collect();
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|t| to[*t].columns.iter().all(|c| c.fk.as_ref().is_none_or(|f| f == *t || !pending.contains(&f))))
            .unwrap_or(0); // a foreign key cycle: emit anyway, the database will report it
        let t = pending.remove(ready);
        ops.push(Op::CreateTable { table: t.clone(), columns: to[t].columns.clone() });
        adds.extend(to[t].indexes.iter().map(|ix| Op::AddIndex { table: t.clone(), index: ix.clone() }));
    }
    for (t, new) in to.iter().filter(|(t, _)| from.contains_key(*t)) {
        let old = &from[t];
        for ix in old.indexes.iter().filter(|ix| !new.indexes.contains(ix)) {
            drops.push(Op::DropIndex { table: t.clone(), name: ix.name.clone() });
        }
        for c in &new.columns {
            match old.columns.iter().find(|o| o.name == c.name) {
                None => {
                    // Existing rows need a value. For a datetime the epoch would read as bad data,
                    // so use the moment the migration was made (Django's usual `timezone.now`).
                    let default = (!c.null).then(|| match c.ty {
                        FieldType::DateTime => Value::from(crate::DateTime::now()),
                        _ => zero(c.ty),
                    });
                    ops.push(Op::AddColumn { table: t.clone(), column: c.clone(), default });
                }
                Some(o) if o != c => ops.push(Op::AlterColumn { table: t.clone(), column: c.clone() }),
                Some(_) => {}
            }
        }
        for o in old.columns.iter().filter(|o| !new.columns.iter().any(|c| c.name == o.name)) {
            ops.push(Op::DropColumn { table: t.clone(), column: o.name.clone() });
        }
        for ix in new.indexes.iter().filter(|ix| !old.indexes.contains(ix)) {
            adds.push(Op::AddIndex { table: t.clone(), index: ix.clone() });
        }
    }
    // Drop referencing tables (join tables) before the tables they point to.
    let mut gone: Vec<&String> = from.keys().filter(|t| !to.contains_key(*t)).collect();
    gone.sort_by_key(|t| std::cmp::Reverse(from[*t].columns.iter().filter(|c| c.fk.is_some()).count()));
    ops.extend(gone.into_iter().map(|t| Op::DropTable { table: t.clone() }));
    drops.into_iter().chain(ops).chain(adds).collect()
}

pub(crate) fn zero(ty: FieldType) -> Value {
    match ty {
        FieldType::Int | FieldType::DateTime => Value::Int(0),
        FieldType::Float => Value::Float(0.0),
        FieldType::Bool => Value::Bool(false),
        FieldType::Varchar(_) | FieldType::Text => Value::Text(String::new()),
    }
}

// ---------------------------------------------------------------- files

/// Framework tables ship as built-in migrations that sort first. They are frozen
/// JSON, like any shipped migration: change the schema with a new file, never an edit.
pub fn builtin() -> Vec<Migration> {
    [
        ("00000000000000_rangoli_builtin", include_str!("migrations/00000000000000_rangoli_builtin.json")),
        ("00000000000001_rangoli_admin_log", include_str!("migrations/00000000000001_rangoli_admin_log.json")),
    ]
    .into_iter()
    .map(|(name, json)| Migration { name: name.into(), ..serde_json::from_str(json).expect("built-in migration") })
    .collect()
}

/// Built-in migrations followed by `dir/*.json` in name order.
pub fn load(dir: &Path) -> Result<Vec<Migration>> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "json")).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.into()),
    };
    files.sort();
    let mut out = builtin();
    for path in files {
        let text = std::fs::read_to_string(&path)?;
        let mut m: Migration = serde_json::from_str(&text).map_err(|e| Error::Migration(format!("{}: {e}", path.display())))?;
        m.name = path.file_stem().unwrap().to_string_lossy().into_owned();
        out.push(m);
    }
    Ok(out)
}

/// Replay every migration into a schema state, naming the file that broke it.
pub fn replay(migrations: &[Migration]) -> Result<State> {
    let mut state = State::new();
    let mut touched: BTreeMap<String, String> = BTreeMap::new();
    for m in migrations {
        for op in &m.operations {
            if let Err(e) = apply(&mut state, op) {
                let hint = target(op)
                    .and_then(|t| touched.get(&t))
                    .map(|prev| format!(" (last changed by `{prev}`; branches probably diverged)"))
                    .unwrap_or_default();
                return Err(Error::Migration(format!("`{}`: {e}{hint}", m.name)));
            }
            if let Some(t) = target(op) {
                touched.insert(t, m.name.clone());
            }
        }
    }
    Ok(state)
}

fn target(op: &Op) -> Option<String> {
    match op {
        Op::CreateTable { table, .. } | Op::DropTable { table } => Some(table.clone()),
        Op::AddColumn { table, column, .. } | Op::AlterColumn { table, column } => Some(format!("{table}.{}", column.name)),
        Op::DropColumn { table, column } => Some(format!("{table}.{column}")),
        Op::AddIndex { table, index } => Some(format!("{table}#{}", index.name)),
        Op::DropIndex { table, name } => Some(format!("{table}#{name}")),
        Op::Sql { .. } => None,
    }
}

/// Write a migration for model changes; `Ok(None)` when already up to date.
pub fn make(dir: &Path, models: &[&'static ModelMeta], name: Option<&str>) -> Result<Option<PathBuf>> {
    let ops = diff(&replay(&load(dir)?)?, &model_state(models));
    if ops.is_empty() {
        return Ok(None);
    }
    let label = name.map(str::to_owned).unwrap_or_else(|| match &ops[0] {
        Op::CreateTable { table, .. } => format!("create_{table}"),
        Op::DropTable { table } => format!("drop_{table}"),
        Op::AddColumn { table, column, .. } => format!("add_{table}_{}", column.name),
        Op::DropColumn { table, column } => format!("drop_{table}_{column}"),
        Op::AlterColumn { table, column } => format!("alter_{table}_{}", column.name),
        Op::AddIndex { index, .. } => format!("add_{}", index.name),
        Op::DropIndex { name, .. } => format!("drop_{name}"),
        Op::Sql { .. } => "sql".into(),
    });
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}_{label}.json", utc_stamp(secs)));
    let body = serde_json::to_string_pretty(&Migration { name: String::new(), operations: ops }).unwrap();
    std::fs::write(&path, body + "\n")?;
    Ok(Some(path))
}

/// `YYYYMMDDHHMMSS` in UTC.
pub fn utc_stamp(secs: u64) -> String {
    let (y, m, d, h, mi, s) = crate::DateTime::from_unix(secs as i64).parts();
    format!("{y:04}{m:02}{d:02}{h:02}{mi:02}{s:02}")
}

// ---------------------------------------------------------------- SQL

fn col_type(d: Dialect, ty: FieldType) -> String {
    match (ty, d) {
        (FieldType::Int | FieldType::DateTime, Dialect::Sqlite) => "INTEGER".into(),
        (FieldType::Int | FieldType::DateTime, _) => "BIGINT".into(),
        (FieldType::Float, Dialect::Postgres) => "DOUBLE PRECISION".into(),
        (FieldType::Float, Dialect::Mysql) => "DOUBLE".into(),
        (FieldType::Float, Dialect::Sqlite) => "REAL".into(),
        (FieldType::Bool, Dialect::Sqlite) => "INTEGER".into(),
        // MySQL's BOOLEAN is TINYINT, which sqlx's Any driver can't map.
        (FieldType::Bool, Dialect::Mysql) => "SMALLINT".into(),
        (FieldType::Bool, Dialect::Postgres) => "BOOLEAN".into(),
        (FieldType::Varchar(n), _) => format!("VARCHAR({n})"),
        (FieldType::Text, _) => "TEXT".into(),
    }
}

fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => (if *b { "1" } else { "0" }).into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

fn col_def(d: Dialect, c: &Column, default: Option<&Value>) -> String {
    let mut s = format!("{} {}", d.quote(&c.name), col_type(d, c.ty));
    if let Some(v) = default {
        // Postgres has a real boolean type and rejects 0/1 literals.
        let lit = match (d, v) {
            (Dialect::Postgres, Value::Bool(b)) => b.to_string().to_uppercase(),
            _ => literal(v),
        };
        s.push_str(&format!(" DEFAULT {lit}"));
    }
    if !c.null {
        s.push_str(" NOT NULL");
    }
    if c.unique {
        s.push_str(" UNIQUE");
    }
    s
}

fn on_delete(c: &Column) -> &'static str {
    if c.cascade {
        " ON DELETE CASCADE"
    } else {
        ""
    }
}

fn fk_clause(d: Dialect, c: &Column) -> Option<String> {
    c.fk.as_ref().map(|t| format!("FOREIGN KEY ({}) REFERENCES {} ({}){}", d.quote(&c.name), d.quote(t), d.quote("id"), on_delete(c)))
}

fn create_table(d: Dialect, table: &str, cols: &[Column], defaults: &BTreeMap<&str, &Value>) -> String {
    let pk = match d {
        Dialect::Postgres => "\"id\" BIGSERIAL PRIMARY KEY".into(),
        Dialect::Mysql => "`id` BIGINT AUTO_INCREMENT PRIMARY KEY".into(),
        Dialect::Sqlite => "\"id\" INTEGER PRIMARY KEY AUTOINCREMENT".into(),
    };
    let defs = std::iter::once(pk)
        .chain(cols.iter().map(|c| col_def(d, c, defaults.get(c.name.as_str()).copied())))
        .chain(cols.iter().filter_map(|c| fk_clause(d, c)));
    format!("CREATE TABLE {} ({})", d.quote(table), defs.collect::<Vec<_>>().join(", "))
}

/// SQLite can't alter columns in place: build the new table, copy rows, swap.
fn sqlite_rebuild(table: &str, old: &[Column], new: &[Column], indexes: &[Index], defaults: &BTreeMap<&str, &Value>) -> Vec<String> {
    let d = Dialect::Sqlite;
    let tmp = format!("_rangoli_new_{table}");
    let keep: Vec<String> = std::iter::once("id".to_string())
        .chain(new.iter().filter(|c| old.iter().any(|o| o.name == c.name)).map(|c| c.name.clone()))
        .map(|c| d.quote(&c))
        .collect();
    vec![
        create_table(d, &tmp, new, defaults),
        format!("INSERT INTO {} ({k}) SELECT {k} FROM {}", d.quote(&tmp), d.quote(table), k = keep.join(", ")),
        format!("DROP TABLE {}", d.quote(table)),
        format!("ALTER TABLE {} RENAME TO {}", d.quote(&tmp), d.quote(table)),
    ]
    .into_iter()
    // Dropping the old table dropped its indexes too.
    .chain(indexes.iter().map(|ix| create_index(d, table, ix)))
    .collect()
}

fn create_index(d: Dialect, table: &str, ix: &Index) -> String {
    let cols: Vec<String> = ix.columns.iter().map(|c| d.quote(c)).collect();
    format!("CREATE {}INDEX {} ON {} ({})", if ix.unique { "UNIQUE " } else { "" }, d.quote(&ix.name), d.quote(table), cols.join(", "))
}

/// SQL for one operation, given the schema state *before* it.
pub fn op_sql(d: Dialect, state: &State, op: &Op) -> Result<Vec<String>> {
    let q = |s: &str| d.quote(s);
    let none = BTreeMap::new();
    Ok(match op {
        Op::CreateTable { table, columns } => vec![create_table(d, table, columns, &none)],
        Op::DropTable { table } => vec![format!("DROP TABLE {}", q(table))],
        Op::Sql { sql } => vec![sql.clone()],
        Op::AddIndex { table, index } => {
            let cols = &state[table].columns;
            if d == Dialect::Mysql && index.columns.iter().any(|c| cols.iter().any(|col| &col.name == c && col.ty == FieldType::Text)) {
                return Err(Error::Migration(format!("MySQL can't index TEXT column(s) of `{}`; use a max_length field", index.name)));
            }
            vec![create_index(d, table, index)]
        }
        Op::DropIndex { table, name } => match d {
            Dialect::Mysql => vec![format!("DROP INDEX {} ON {}", q(name), q(table))],
            _ => vec![format!("DROP INDEX {}", q(name))],
        },
        Op::AddColumn { table, column, default } => {
            let defaults: BTreeMap<&str, &Value> = default.iter().map(|v| (column.name.as_str(), v)).collect();
            match d {
                Dialect::Sqlite if column.unique => {
                    let old = &state[table];
                    let mut new = old.columns.clone();
                    new.push(column.clone());
                    sqlite_rebuild(table, &old.columns, &new, &old.indexes, &defaults)
                }
                _ => {
                    let mut sql = format!("ALTER TABLE {} ADD COLUMN {}", q(table), col_def(d, column, default.as_ref()));
                    match (d, &column.fk) {
                        (Dialect::Sqlite, Some(t)) => sql.push_str(&format!(" REFERENCES {} ({}){}", q(t), q("id"), on_delete(column))),
                        (_, Some(_)) => sql.push_str(&format!(", ADD {}", fk_clause(d, column).unwrap())),
                        _ => {}
                    }
                    vec![sql]
                }
            }
        }
        Op::DropColumn { table, column } => match d {
            Dialect::Sqlite => {
                let old = &state[table];
                let new: Vec<Column> = old.columns.iter().filter(|c| &c.name != column).cloned().collect();
                sqlite_rebuild(table, &old.columns, &new, &old.indexes, &none)
            }
            Dialect::Mysql if state[table].columns.iter().any(|c| &c.name == column && c.fk.is_some()) => {
                return Err(Error::Migration(format!(
                    "dropping foreign key column `{table}.{column}` on MySQL needs the constraint dropped first; add an `sql` operation"
                )))
            }
            _ => vec![format!("ALTER TABLE {} DROP COLUMN {}", q(table), q(column))],
        },
        Op::AlterColumn { table, column } => {
            let old = state[table].columns.iter().find(|c| c.name == column.name).unwrap();
            if d != Dialect::Sqlite && (old.unique != column.unique || old.fk != column.fk || old.cascade != column.cascade) {
                return Err(Error::Migration(format!(
                    "changing unique/foreign key on `{table}.{}` is not automated yet; add an `sql` operation",
                    column.name
                )));
            }
            match d {
                Dialect::Sqlite => {
                    let old = &state[table];
                    let new: Vec<Column> =
                        old.columns.iter().map(|c| if c.name == column.name { column.clone() } else { c.clone() }).collect();
                    sqlite_rebuild(table, &old.columns, &new, &old.indexes, &none)
                }
                Dialect::Postgres => {
                    let ty = col_type(d, column.ty);
                    let null = if column.null { "DROP NOT NULL" } else { "SET NOT NULL" };
                    vec![format!(
                        "ALTER TABLE {t} ALTER COLUMN {c} TYPE {ty} USING {c}::{ty}, ALTER COLUMN {c} {null}",
                        t = q(table),
                        c = q(&column.name)
                    )]
                }
                Dialect::Mysql => {
                    let mut c = column.clone();
                    c.unique = false; // MODIFY would add a second unique index
                    vec![format!("ALTER TABLE {} MODIFY COLUMN {}", q(table), col_def(d, &c, None))]
                }
            }
        }
    })
}

// ---------------------------------------------------------------- runner

const LEDGER: &str = "rangoli_migrations";

async fn ensure_ledger() -> Result<HashSet<String>> {
    let d = db().dialect;
    let sql = format!("CREATE TABLE IF NOT EXISTS {} (name VARCHAR(255) PRIMARY KEY, applied_at BIGINT NOT NULL)", d.quote(LEDGER));
    sqlx::query(&sql).execute(&db().pool).await?;
    let rows = sqlx::query(&format!("SELECT name FROM {}", d.quote(LEDGER))).fetch_all(&db().pool).await?;
    Ok(rows.iter().map(|r| sqlx::Row::get::<String, _>(r, "name")).collect())
}

/// Names of migrations not yet applied.
pub async fn pending(dir: &Path) -> Result<Vec<String>> {
    let applied = ensure_ledger().await?;
    Ok(load(dir)?.into_iter().map(|m| m.name).filter(|n| !applied.contains(n)).collect())
}

/// Apply every unapplied migration; each one runs in its own transaction.
pub async fn run(dir: &Path) -> Result<Vec<String>> {
    let d = db().dialect;
    let applied = ensure_ledger().await?;
    let migrations = load(dir)?;
    replay(&migrations)?; // validate everything before touching the database
    let mut state = State::new();
    let mut done = vec![];
    let mut conn = db().pool.acquire().await?;
    for m in &migrations {
        let mut sqls = vec![];
        for op in &m.operations {
            sqls.extend(op_sql(d, &state, op)?);
            apply(&mut state, op).map_err(Error::Migration)?;
        }
        if applied.contains(&m.name) {
            continue;
        }
        // SQLite table rebuilds must not trip foreign keys mid-swap; this pragma is ignored inside transactions.
        if d == Dialect::Sqlite {
            sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await?;
        }
        // ponytail: MySQL auto-commits DDL, so a failing MySQL migration can leave partial changes (Django has the same limit).
        let mut tx = conn.begin().await?;
        for sql in &sqls {
            sqlx::query(sql).execute(&mut *tx).await.map_err(|e| Error::Migration(format!("`{}`: {e}\n  while running: {sql}", m.name)))?;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let ledger = format!("INSERT INTO {} (name, applied_at) VALUES ({}, {})", d.quote(LEDGER), d.ph(1), d.ph(2));
        sqlx::query(&ledger).bind(m.name.clone()).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        if d == Dialect::Sqlite {
            sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await?;
        }
        done.push(m.name.clone());
    }
    drop(conn);
    if !done.is_empty() {
        // Pooled connections prepared statements against the old schema. On SQLite a stale
        // connection even reads an unknown "column" as a string literal, so recycle them all.
        while let Some(c) = db().pool.try_acquire() {
            c.close().await.ok();
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: FieldType) -> Column {
        Column { name: name.into(), ty, null: false, unique: false, fk: None, cascade: false }
    }

    #[test]
    fn builtin_migrations_match_framework_models() {
        use crate::orm::Model;
        let models = [crate::auth::User::meta(), crate::auth::Session::meta(), crate::admin::LogEntry::meta()];
        assert_eq!(replay(&builtin()).unwrap(), model_state(&models), "add a new built-in migration file for this change");
    }

    #[test]
    fn stamp_is_utc() {
        assert_eq!(utc_stamp(0), "19700101000000");
        assert_eq!(utc_stamp(1_700_000_000), "20231114221320");
        assert_eq!(utc_stamp(951_782_400), "20000229000000"); // leap day
    }

    #[test]
    fn diff_orders_fk_targets_first_and_adds_defaults() {
        let mut post = col("author_id", FieldType::Int);
        post.fk = Some("author".into());
        let table = |columns: Vec<Column>| Table { columns, indexes: vec![] };
        let to: State = [("post".to_string(), table(vec![post])), ("author".to_string(), table(vec![col("name", FieldType::Text)]))].into();
        let ops = diff(&State::new(), &to);
        assert!(matches!(&ops[0], Op::CreateTable { table, .. } if table == "author"));
        assert!(matches!(&ops[1], Op::CreateTable { table, .. } if table == "post"));

        let mut grown = to.clone();
        grown.get_mut("author").unwrap().columns.push(col("active", FieldType::Bool));
        assert_eq!(
            diff(&to, &grown),
            vec![Op::AddColumn { table: "author".into(), column: col("active", FieldType::Bool), default: Some(Value::Bool(false)) }]
        );
    }

    #[test]
    fn indexes_diff_in_safe_order() {
        let ix = Index { name: index_name("t", &["a"], false), columns: vec!["a".into()], unique: false };
        let with = |cols: Vec<Column>, ixs: Vec<Index>| -> State { [("t".to_string(), Table { columns: cols, indexes: ixs })].into() };
        let before = with(vec![col("a", FieldType::Int)], vec![ix.clone()]);
        let after = with(vec![], vec![]);
        let ops = diff(&before, &after);
        assert!(matches!(&ops[0], Op::DropIndex { .. }) && matches!(&ops[1], Op::DropColumn { .. }), "{ops:?}");
        let mut state = before.clone();
        ops.iter().for_each(|op| apply(&mut state, op).unwrap());
        assert_eq!(state, after);
        assert!(apply(&mut before.clone(), &Op::DropColumn { table: "t".into(), column: "a".into() }).unwrap_err().contains("still used"));
        let ops = diff(&after, &before);
        assert!(matches!(&ops[..], [Op::AddColumn { .. }, Op::AddIndex { .. }]), "{ops:?}");
    }

    #[test]
    fn index_names_are_stable_and_short() {
        assert_eq!(index_name("blog_post", &["title"], false), "blog_post_title_idx");
        let long = index_name("blog_post_with_a_really_long_relation_name_tags", &["source_id", "target_id"], true);
        assert_eq!(
            long, "blog_post_with_a_really_long_relation_name_tags_sou_16c3e9cd",
            "FNV-1a must never change: these names are in migration files"
        );
        assert!(long.len() <= 60);
    }

    #[test]
    fn replay_names_the_conflicting_branch() {
        let add = |n: &str| Migration {
            name: n.into(),
            operations: vec![Op::AddColumn { table: "t".into(), column: col("x", FieldType::Int), default: None }],
        };
        let create = Migration { name: "1_create".into(), operations: vec![Op::CreateTable { table: "t".into(), columns: vec![] }] };
        let err = replay(&[create, add("2_branch_a"), add("3_branch_b")]).unwrap_err().to_string();
        assert!(err.contains("3_branch_b") && err.contains("2_branch_a"), "{err}");
    }

    #[test]
    fn migration_json_round_trips() {
        let m = Migration {
            name: String::new(),
            operations: vec![Op::AddColumn {
                table: "t".into(),
                column: col("n", FieldType::Varchar(20)),
                default: Some(Value::Text("".into())),
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"operations":[{"op":"add_column","table":"t","column":{"name":"n","type":{"varchar":20}},"default":""}]}"#);
        assert_eq!(serde_json::from_str::<Migration>(&json).unwrap().operations, m.operations);
    }
}

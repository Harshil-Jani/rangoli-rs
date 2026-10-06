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
}

fn is_false(b: &bool) -> bool {
    !*b
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

/// Table name -> columns (excluding the implicit `id`).
pub type State = BTreeMap<String, Vec<Column>>;

pub fn columns_of(meta: &ModelMeta) -> Vec<Column> {
    meta.fields.iter().map(|f| Column { name: f.name.into(), ty: f.ty, null: f.null, unique: f.unique, fk: f.fk.map(Into::into) }).collect()
}

pub fn model_state(models: &[&'static ModelMeta]) -> State {
    models.iter().map(|m| (m.table.to_string(), columns_of(m))).collect()
}

/// Apply `op` to `state`, rejecting operations that contradict it.
fn apply(state: &mut State, op: &Op) -> std::result::Result<(), String> {
    match op {
        Op::CreateTable { table, columns } => {
            if state.insert(table.clone(), columns.clone()).is_some() {
                return Err(format!("table `{table}` already exists"));
            }
        }
        Op::DropTable { table } => {
            state.remove(table).ok_or(format!("table `{table}` does not exist"))?;
        }
        Op::AddColumn { table, column, .. } => {
            let cols = state.get_mut(table).ok_or(format!("table `{table}` does not exist"))?;
            if cols.iter().any(|c| c.name == column.name) {
                return Err(format!("column `{table}.{}` already exists", column.name));
            }
            cols.push(column.clone());
        }
        Op::DropColumn { table, column } => {
            let cols = state.get_mut(table).ok_or(format!("table `{table}` does not exist"))?;
            let i = cols.iter().position(|c| &c.name == column).ok_or(format!("column `{table}.{column}` does not exist"))?;
            cols.remove(i);
        }
        Op::AlterColumn { table, column } => {
            let cols = state.get_mut(table).ok_or(format!("table `{table}` does not exist"))?;
            let c = cols.iter_mut().find(|c| c.name == column.name).ok_or(format!("column `{table}.{}` does not exist", column.name))?;
            *c = column.clone();
        }
        Op::Sql { .. } => {}
    }
    Ok(())
}

/// Operations that turn `from` into `to`, with new tables ordered so foreign key targets come first.
pub fn diff(from: &State, to: &State) -> Vec<Op> {
    let mut ops = vec![];
    let mut pending: Vec<&String> = to.keys().filter(|t| !from.contains_key(*t)).collect();
    while !pending.is_empty() {
        let ready =
            pending.iter().position(|t| to[*t].iter().all(|c| c.fk.as_ref().is_none_or(|f| f == *t || !pending.contains(&f)))).unwrap_or(0); // a foreign key cycle: emit anyway, the database will report it
        let t = pending.remove(ready);
        ops.push(Op::CreateTable { table: t.clone(), columns: to[t].clone() });
    }
    for (t, new) in to.iter().filter(|(t, _)| from.contains_key(*t)) {
        let old = &from[t];
        for c in new {
            match old.iter().find(|o| o.name == c.name) {
                None => {
                    let default = (!c.null).then(|| zero(c.ty));
                    ops.push(Op::AddColumn { table: t.clone(), column: c.clone(), default });
                }
                Some(o) if o != c => ops.push(Op::AlterColumn { table: t.clone(), column: c.clone() }),
                Some(_) => {}
            }
        }
        for o in old.iter().filter(|o| !new.iter().any(|c| c.name == o.name)) {
            ops.push(Op::DropColumn { table: t.clone(), column: o.name.clone() });
        }
    }
    for t in from.keys().filter(|t| !to.contains_key(*t)) {
        ops.push(Op::DropTable { table: t.clone() });
    }
    ops
}

fn zero(ty: FieldType) -> Value {
    match ty {
        FieldType::Int => Value::Int(0),
        FieldType::Float => Value::Float(0.0),
        FieldType::Bool => Value::Bool(false),
        FieldType::Varchar(_) | FieldType::Text => Value::Text(String::new()),
    }
}

// ---------------------------------------------------------------- files

/// Framework tables (users, sessions) ship as a built-in migration that sorts first.
pub fn builtin() -> Vec<Migration> {
    use crate::orm::Model;
    let operations = [crate::auth::User::meta(), crate::auth::Session::meta()]
        .iter()
        .map(|m| Op::CreateTable { table: m.table.into(), columns: columns_of(m) })
        .collect();
    vec![Migration { name: "00000000000000_rangoli_builtin".into(), operations }]
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
        Op::Sql { .. } => "sql".into(),
    });
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}_{label}.json", utc_stamp(secs)));
    let body = serde_json::to_string_pretty(&Migration { name: String::new(), operations: ops }).unwrap();
    std::fs::write(&path, body + "\n")?;
    Ok(Some(path))
}

/// `YYYYMMDDHHMMSS` in UTC (Howard Hinnant's civil-from-days).
pub fn utc_stamp(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ---------------------------------------------------------------- SQL

fn col_type(d: Dialect, ty: FieldType) -> String {
    match (ty, d) {
        (FieldType::Int, Dialect::Sqlite) => "INTEGER".into(),
        (FieldType::Int, _) => "BIGINT".into(),
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

fn fk_clause(d: Dialect, c: &Column) -> Option<String> {
    c.fk.as_ref().map(|t| format!("FOREIGN KEY ({}) REFERENCES {} ({})", d.quote(&c.name), d.quote(t), d.quote("id")))
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
fn sqlite_rebuild(table: &str, old: &[Column], new: &[Column], defaults: &BTreeMap<&str, &Value>) -> Vec<String> {
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
}

/// SQL for one operation, given the schema state *before* it.
pub fn op_sql(d: Dialect, state: &State, op: &Op) -> Result<Vec<String>> {
    let q = |s: &str| d.quote(s);
    let none = BTreeMap::new();
    Ok(match op {
        Op::CreateTable { table, columns } => vec![create_table(d, table, columns, &none)],
        Op::DropTable { table } => vec![format!("DROP TABLE {}", q(table))],
        Op::Sql { sql } => vec![sql.clone()],
        Op::AddColumn { table, column, default } => {
            let defaults: BTreeMap<&str, &Value> = default.iter().map(|v| (column.name.as_str(), v)).collect();
            match d {
                Dialect::Sqlite if column.unique => {
                    let old = &state[table];
                    let mut new = old.clone();
                    new.push(column.clone());
                    sqlite_rebuild(table, old, &new, &defaults)
                }
                _ => {
                    let mut sql = format!("ALTER TABLE {} ADD COLUMN {}", q(table), col_def(d, column, default.as_ref()));
                    match (d, &column.fk) {
                        (Dialect::Sqlite, Some(t)) => sql.push_str(&format!(" REFERENCES {} ({})", q(t), q("id"))),
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
                let new: Vec<Column> = old.iter().filter(|c| &c.name != column).cloned().collect();
                sqlite_rebuild(table, old, &new, &none)
            }
            Dialect::Mysql if state[table].iter().any(|c| &c.name == column && c.fk.is_some()) => {
                return Err(Error::Migration(format!(
                    "dropping foreign key column `{table}.{column}` on MySQL needs the constraint dropped first; add an `sql` operation"
                )))
            }
            _ => vec![format!("ALTER TABLE {} DROP COLUMN {}", q(table), q(column))],
        },
        Op::AlterColumn { table, column } => {
            let old = state[table].iter().find(|c| c.name == column.name).unwrap();
            if d != Dialect::Sqlite && (old.unique != column.unique || old.fk != column.fk) {
                return Err(Error::Migration(format!(
                    "changing unique/foreign key on `{table}.{}` is not automated yet; add an `sql` operation",
                    column.name
                )));
            }
            match d {
                Dialect::Sqlite => {
                    let old_cols = &state[table];
                    let new: Vec<Column> =
                        old_cols.iter().map(|c| if c.name == column.name { column.clone() } else { c.clone() }).collect();
                    sqlite_rebuild(table, old_cols, &new, &none)
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
        Column { name: name.into(), ty, null: false, unique: false, fk: None }
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
        let to: State = [("post".to_string(), vec![post]), ("author".to_string(), vec![col("name", FieldType::Text)])].into();
        let ops = diff(&State::new(), &to);
        assert!(matches!(&ops[0], Op::CreateTable { table, .. } if table == "author"));
        assert!(matches!(&ops[1], Op::CreateTable { table, .. } if table == "post"));

        let mut grown = to.clone();
        grown.get_mut("author").unwrap().push(col("active", FieldType::Bool));
        assert_eq!(
            diff(&to, &grown),
            vec![Op::AddColumn { table: "author".into(), column: col("active", FieldType::Bool), default: Some(Value::Bool(false)) }]
        );
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

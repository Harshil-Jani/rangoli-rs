//! Point Rangoli at an existing Django database and get the admin, a JSON API and a guarded MCP
//! server for AI agents, without rewriting the Django app:
//!
//! ```sh
//! RANGOLI_DATABASE_URL=postgres://.../your_django_db cargo run -p rangoli-django
//! ```
//!
//! Tables are read from the database's own catalog, so there's nothing to declare. Django's
//! internal tables and its auth tables are left out; columns that look sensitive (`password`,
//! `token`, `secret`, `api_key`...) are private, so they never reach the API or an agent. Everything
//! is read-only unless `RANGOLI_DJANGO_WRITES=1`. Staff sign in with their Django username and
//! password (Django's `pbkdf2_sha256` hashes are checked directly).
//!
//! Rangoli adds its own tables (`rangoli_user`, `rangoli_api_token`, ...) to the same database;
//! it never alters Django's.

use crate::orm::{self, Dialect, FieldMeta, FieldType, ModelMeta, Value};
use crate::{Error, Result};
use sqlx::Row;
use std::collections::{BTreeMap, HashMap};

/// Django's own tables, which a person or an agent has no business editing through Rangoli.
const SKIP_EXACT: [&str; 9] = [
    "django_migrations",
    "django_session",
    "django_content_type",
    "django_admin_log",
    "django_site",
    "auth_permission",
    "auth_group",
    "auth_group_permissions",
    "auth_user",
];
const SKIP_PREFIX: [&str; 3] = ["rangoli_", "auth_user_", "sqlite_"];

/// Column names that look like secrets: kept private (never in API or MCP output).
const SENSITIVE: [&str; 12] =
    ["password", "passwd", "secret", "token", "api_key", "apikey", "private_key", "salt", "otp", "ssn", "card_number", "iban"];

pub fn looks_sensitive(column: &str) -> bool {
    let c = column.to_ascii_lowercase();
    SENSITIVE.iter().any(|s| c.contains(s)) || c.ends_with("_key")
}

/// What introspection found.
#[derive(Debug, Default)]
pub struct Schema {
    pub models: Vec<&'static ModelMeta>,
    /// Tables left out, with why.
    pub skipped: Vec<(String, String)>,
}

struct Column {
    name: String,
    ty: FieldType,
    null: bool,
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// Map a catalog type name to a Rangoli field type. Anything else is shown read-only as text.
fn field_type(d: Dialect, raw: &str, len: Option<i64>) -> FieldType {
    let t = raw.trim().to_ascii_lowercase();
    let n = |default: u32| len.and_then(|l| u32::try_from(l).ok()).filter(|l| *l > 0).unwrap_or(default);
    let paren = || t.split_once('(').and_then(|(_, r)| r.trim_end_matches(')').split(',').next()?.trim().parse::<u32>().ok());
    match d {
        Dialect::Postgres => match t.as_str() {
            "smallint" | "integer" | "bigint" => FieldType::Int,
            "boolean" => FieldType::Bool,
            "character varying" | "character" => FieldType::Varchar(n(255)),
            "text" => FieldType::Text,
            "double precision" | "real" => FieldType::Float,
            _ => FieldType::Opaque,
        },
        Dialect::Mysql => {
            if t.starts_with("tinyint(1)") {
                FieldType::Bool
            } else if ["tinyint", "smallint", "mediumint", "int", "bigint"].iter().any(|i| t.starts_with(i)) {
                FieldType::Int
            } else if t.starts_with("varchar") || t.starts_with("char") {
                FieldType::Varchar(paren().unwrap_or(255))
            } else if t.ends_with("text") {
                FieldType::Text
            } else if t.starts_with("double") || t.starts_with("float") || t.starts_with("real") {
                FieldType::Float
            } else {
                FieldType::Opaque
            }
        }
        Dialect::Sqlite => {
            if t == "bool" || t == "boolean" {
                FieldType::Bool
            } else if t.contains("int") {
                FieldType::Int
            } else if t.starts_with("varchar") || t.starts_with("char") {
                FieldType::Varchar(paren().unwrap_or(255))
            } else if t == "text" {
                FieldType::Text
            } else if t == "real" || t.starts_with("double") || t.starts_with("float") {
                FieldType::Float
            } else {
                FieldType::Opaque // datetime, date, decimal, ... as Django stores them in SQLite
            }
        }
    }
}

async fn rows(sql: &str, args: &[&str]) -> Result<Vec<sqlx::any::AnyRow>> {
    let mut q = sqlx::query(sql);
    for a in args {
        q = q.bind(a.to_string());
    }
    Ok(q.fetch_all(&orm::db().pool).await?)
}

/// A catalog string. MySQL hands some back as blobs (`column_type` is LONGTEXT), so take bytes too.
fn text(r: &sqlx::any::AnyRow, col: &str) -> String {
    r.try_get::<String, _>(col)
        .or_else(|_| r.try_get::<Vec<u8>, _>(col).map(|b| String::from_utf8_lossy(&b).into_owned()))
        .unwrap_or_default()
}

/// Table name to its columns, in order.
type Tables = BTreeMap<String, Vec<Column>>;
/// (table, column) to the table its foreign key points at.
type ForeignKeys = HashMap<(String, String), String>;

/// Columns and foreign keys of every table, from the database's catalog.
async fn catalog() -> Result<(Tables, ForeignKeys)> {
    let d = orm::db().dialect;
    let (mut tables, mut fks): (Tables, ForeignKeys) = Default::default();
    match d {
        Dialect::Postgres => {
            let cols = rows(
                "SELECT c.table_name::text AS t, c.column_name::text AS c, c.data_type::text AS ty, c.is_nullable::text AS n,
                        c.character_maximum_length::bigint AS len
                 FROM information_schema.columns c
                 JOIN information_schema.tables tb ON tb.table_name = c.table_name AND tb.table_schema = c.table_schema
                 WHERE c.table_schema = current_schema() AND tb.table_type = 'BASE TABLE'
                 ORDER BY c.table_name, c.ordinal_position",
                &[],
            )
            .await?;
            for r in &cols {
                let len = r.try_get::<Option<i64>, _>("len").ok().flatten();
                tables.entry(text(r, "t")).or_default().push(Column {
                    name: text(r, "c"),
                    ty: field_type(d, &text(r, "ty"), len),
                    null: text(r, "n") == "YES",
                });
            }
            let keys = rows(
                "SELECT kcu.table_name::text AS t, kcu.column_name::text AS c, ccu.table_name::text AS r
                 FROM information_schema.table_constraints tc
                 JOIN information_schema.key_column_usage kcu ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema
                 JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = tc.constraint_name AND ccu.table_schema = tc.table_schema
                 WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = current_schema()",
                &[],
            )
            .await?;
            for r in &keys {
                fks.insert((text(r, "t"), text(r, "c")), text(r, "r"));
            }
        }
        Dialect::Mysql => {
            // CONVERT, not CAST: the catalog's strings come back flagged binary, which decodes as bytes.
            let cols = rows(
                "SELECT CONVERT(table_name USING utf8mb4) AS t, CONVERT(column_name USING utf8mb4) AS c, CONVERT(column_type USING utf8mb4) AS ty,
                        CONVERT(is_nullable USING utf8mb4) AS n
                 FROM information_schema.columns WHERE table_schema = DATABASE() ORDER BY table_name, ordinal_position",
                &[],
            )
            .await?;
            for r in &cols {
                tables.entry(text(r, "t")).or_default().push(Column {
                    name: text(r, "c"),
                    ty: field_type(d, &text(r, "ty"), None),
                    null: text(r, "n") == "YES",
                });
            }
            let keys = rows(
                "SELECT CONVERT(table_name USING utf8mb4) AS t, CONVERT(column_name USING utf8mb4) AS c, CONVERT(referenced_table_name USING utf8mb4) AS r
                 FROM information_schema.key_column_usage WHERE table_schema = DATABASE() AND referenced_table_name IS NOT NULL",
                &[],
            )
            .await?;
            for r in &keys {
                fks.insert((text(r, "t"), text(r, "c")), text(r, "r"));
            }
        }
        Dialect::Sqlite => {
            let names = rows("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name", &[]).await?;
            for n in names.iter().map(|r| text(r, "name")) {
                let cols = rows(r#"SELECT name, type, "notnull" AS nn FROM pragma_table_info(?)"#, &[&n]).await?;
                let parsed = cols
                    .iter()
                    .map(|r| Column {
                        name: text(r, "name"),
                        ty: field_type(d, &text(r, "type"), None),
                        null: r.try_get::<i64, _>("nn").unwrap_or(0) == 0,
                    })
                    .collect();
                tables.insert(n.clone(), parsed);
                for r in rows(r#"SELECT "from" AS c, "table" AS r FROM pragma_foreign_key_list(?)"#, &[&n]).await? {
                    fks.insert((n.clone(), text(&r, "c")), text(&r, "r"));
                }
            }
        }
    }
    Ok((tables, fks))
}

/// `blog_blogpost` -> `Blogpost`, `shop_order_item` -> `OrderItem`: Django's model name, roughly.
fn model_name(table: &str) -> String {
    let rest = table.split_once('_').map_or(table, |(_, r)| r);
    rest.split('_').filter(|w| !w.is_empty()).map(|w| w[..1].to_uppercase() + &w[1..]).collect()
}

/// Read the database and describe each usable table as a model Rangoli can serve.
pub async fn introspect() -> Result<Schema> {
    let (tables, fks) = catalog().await?;
    let mut schema = Schema::default();
    let usable: Vec<&String> =
        tables.keys().filter(|t| !SKIP_EXACT.contains(&t.as_str()) && !SKIP_PREFIX.iter().any(|p| t.starts_with(p))).collect();
    for table in tables.keys().filter(|t| !usable.contains(t)) {
        if !table.starts_with("rangoli_") && !table.starts_with("sqlite_") {
            schema.skipped.push((table.clone(), "Django's own table".into()));
        }
    }
    for table in &usable {
        let cols = &tables[*table];
        match cols.iter().find(|c| c.name == "id") {
            Some(c) if c.ty == FieldType::Int => {}
            _ => {
                schema.skipped.push(((*table).clone(), "no integer `id` primary key".into()));
                continue;
            }
        }
        let fields: Vec<FieldMeta> = cols
            .iter()
            .filter(|c| c.name != "id")
            .map(|c| {
                let fk = fks.get(&((*table).clone(), c.name.clone())).filter(|t| usable.contains(t)).map(|t| leak(t));
                FieldMeta {
                    name: leak(&c.name),
                    ty: c.ty,
                    null: c.null,
                    unique: false,
                    password: false,
                    private: looks_sensitive(&c.name),
                    fk: if c.ty == FieldType::Int { fk } else { None },
                    cascade: false,
                    auto_now: false,
                    auto_now_add: false,
                    index: false,
                    choices: None,
                    default: None,
                }
            })
            .collect();
        let display = ["name", "title", "username", "email", "slug", "label", "code"]
            .iter()
            .find(|d| fields.iter().any(|f| f.name == **d && matches!(f.ty, FieldType::Varchar(_) | FieldType::Text) && !f.private))
            .map(|d| leak(d));
        let meta: &'static ModelMeta = Box::leak(Box::new(ModelMeta {
            name: leak(&model_name(table)),
            table: leak(table),
            display,
            fields: Box::leak(fields.into_boxed_slice()),
            m2m: &[],
            indexes: &[],
            search: &[],
            unique_together: &[],
        }));
        schema.models.push(meta);
    }
    Ok(schema)
}

// ---------------------------------------------------------------- Django passwords

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let (mut ipad, mut opad) = ([0x36u8; 64], [0x5cu8; 64]);
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new().chain_update(ipad).chain_update(msg).finalize();
    Sha256::new().chain_update(opad).chain_update(inner).finalize().into()
}

/// PBKDF2-HMAC-SHA256 with one 32-byte block: what Django's default hasher computes.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = salt.to_vec();
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &block);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (o, b) in out.iter_mut().zip(u) {
            *o ^= b;
        }
    }
    out
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Check a password against a Django hash like `pbkdf2_sha256$870000$salt$hash`. Other hashers
/// (argon2, bcrypt) aren't supported: those users sign in with a Rangoli account instead.
pub fn check_django_password(password: &str, encoded: &str) -> bool {
    let mut parts = encoded.splitn(4, '$');
    let (Some("pbkdf2_sha256"), Some(iters), Some(salt), Some(hash)) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let Ok(iters) = iters.parse::<u32>() else { return false };
    if iters == 0 || iters > 10_000_000 {
        return false;
    }
    let computed = base64(&pbkdf2_sha256(password.as_bytes(), salt.as_bytes(), iters));
    // Compare without stopping at the first difference.
    computed.len() == hash.len() && computed.bytes().zip(hash.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Django's `auth_user` row for this username, if the password matches.
async fn django_login(username: String, password: String) -> Result<Option<crate::auth::ExternalUser>> {
    let d = orm::db().dialect;
    // Booleans as integers: Django's SQLite `bool` columns can't be decoded as declared.
    let int = if d == Dialect::Mysql { "SIGNED" } else { "INTEGER" };
    let sql = format!(
        "SELECT {pw} AS pw, CAST({st} AS {int}) AS st, CAST({su} AS {int}) AS su, CAST({ac} AS {int}) AS ac FROM {t} WHERE {u} = {p}",
        pw = d.quote("password"),
        st = d.quote("is_staff"),
        su = d.quote("is_superuser"),
        ac = d.quote("is_active"),
        t = d.quote("auth_user"),
        u = d.quote("username"),
        p = d.ph(1)
    );
    let params = vec![(Value::Text(username.clone()), FieldType::Text)];
    let found = match orm::fetch_all(&sql, &params).await {
        Ok(found) => found,
        Err(e) => {
            eprintln!("rangoli-django: can't read auth_user: {e}");
            return Ok(None);
        }
    };
    let Some(row) = found.first() else { return Ok(None) };
    let flag = |c: &str| orm::read(row, c, FieldType::Bool).ok() == Some(Value::Bool(true));
    let hash: String = row.try_get("pw").unwrap_or_default();
    let ok = tokio::task::spawn_blocking(move || check_django_password(&password, &hash)).await.unwrap_or(false);
    Ok(ok.then(|| crate::auth::ExternalUser { username, is_staff: flag("st"), is_superuser: flag("su"), is_active: flag("ac") }))
}

/// Build the app for a Django database: every usable table in the admin, the API and MCP.
pub async fn app() -> Result<crate::App> {
    let s = crate::settings();
    orm::connect(&s.database_url).await?;
    // Rangoli's own tables only: an empty directory means no app migrations.
    let none = std::env::temp_dir().join("rangoli-django-no-app-migrations");
    crate::migrate::run(&none).await?;
    let schema = introspect().await?;
    if schema.models.is_empty() {
        return Err(Error::Config("found no usable tables (each needs an integer `id` primary key)".into()));
    }
    let writes = std::env::var("RANGOLI_DJANGO_WRITES").is_ok_and(|v| v == "1");
    crate::auth::set_external_login(|u, p| Box::pin(django_login(u, p)));
    let mut app = crate::App::new();
    for meta in &schema.models {
        app = app.dynamic_model(meta, writes);
    }
    eprintln!(
        "rangoli-django: {} table(s) ready ({}), {} skipped; changes {}",
        schema.models.len(),
        schema.models.iter().map(|m| m.table).collect::<Vec<_>>().join(", "),
        schema.skipped.len(),
        if writes { "allowed for staff" } else { "off (RANGOLI_DJANGO_WRITES=1 to allow)" }
    );
    Ok(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_django_pbkdf2_hashes() {
        // Made by Django 6.1: make_password("django-pass", salt="rangoli", hasher=PBKDF2PasswordHasher(iterations=1000)).
        let hash = "pbkdf2_sha256$1000$rangoli$b25uwZU5YUpcAgAuLYZqh/twCO6//0gD0l9udxTs7zw=";
        assert!(check_django_password("django-pass", hash));
        assert!(!check_django_password("wrong", hash));
        assert!(!check_django_password("django-pass", "argon2$argon2id$v=19$..."), "other hashers aren't checked");
        assert!(!check_django_password("x", "pbkdf2_sha256$0$s$h"));
    }

    #[test]
    fn names_and_secrets() {
        assert_eq!(model_name("shop_order_item"), "OrderItem");
        assert!(looks_sensitive("stripe_api_key") && looks_sensitive("reset_token") && looks_sensitive("signing_key"));
        assert!(!looks_sensitive("title") && !looks_sensitive("monkey_name"));
    }
}

//! Embeddings that keep themselves current: `App::embed_fields(Note::EMBEDDING, [&Note::TITLE, &Note::BODY])`.
//!
//! When a write changes any of the source fields, the same UPDATE clears the vector, so a stale
//! embedding never survives an edit: `vec = CASE WHEN <sources unchanged> THEN vec ELSE NULL END`,
//! evaluated against the row as it was (written first, which also suits MySQL's left-to-right
//! SET). A background task then fills every NULL vector in batches, writing only that column, so
//! it can't set itself off again. Writes that leave the text alone (publishing, a view count)
//! don't cost an embedding call.

use crate::orm::{FieldType, Value};
use std::sync::{LazyLock, RwLock};

pub(crate) struct Auto {
    pub table: &'static str,
    pub vector: &'static str,
    pub sources: Vec<&'static str>,
    pub fields: &'static [crate::orm::FieldMeta],
}

pub(crate) static AUTO: LazyLock<RwLock<Vec<Auto>>> = LazyLock::new(Default::default);

fn find<T>(table: &str, f: impl FnOnce(&Auto) -> T) -> Option<T> {
    AUTO.read().unwrap_or_else(std::sync::PoisonError::into_inner).iter().find(|a| a.table == table).map(f)
}

/// The vector column, its `CASE` expression (`{}` for each placeholder) and the values for them.
pub(crate) type Guard = (&'static str, String, Vec<(Value, FieldType)>);

/// For an UPDATE of `table` with `sets`: the assignment that clears the vector when a source
/// changes, as `(column, SQL with {} for the placeholders in order, values)`. `None` when the
/// table has no auto-embedding or the update touches no source.
pub(crate) fn guard(table: &str, sets: &[(&'static str, Value, FieldType)], d: crate::orm::Dialect) -> Option<Guard> {
    let (vector, sources) = find(table, |a| (a.vector, a.sources.clone()))?;
    let written: Vec<&(&'static str, Value, FieldType)> = sets.iter().filter(|(c, ..)| sources.contains(c)).collect();
    if written.is_empty() {
        return None;
    }
    let q = |c: &str| d.quote(c);
    let mut params = vec![];
    let same: Vec<String> = written
        .iter()
        .map(|(c, v, t)| {
            if matches!(v, Value::Null) {
                format!("{} IS NULL", q(c))
            } else {
                params.push((v.clone(), *t));
                format!("{} = {{}}", q(c))
            }
        })
        .collect();
    // Keep what the caller passed for the vector when the text is unchanged, else the stored one.
    let keep = match sets.iter().find(|(c, ..)| *c == vector) {
        Some((_, v, t)) => {
            params.push((v.clone(), *t));
            "{}".to_string()
        }
        None => q(vector),
    };
    Some((vector, format!("CASE WHEN {} THEN {keep} ELSE NULL END", same.join(" AND ")), params))
}

/// The background job: fill missing vectors, a batch at a time.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Fill {
    pub table: String,
}

const BATCH: u64 = 100;

impl crate::tasks::Task for Fill {
    const NAME: &'static str = "rangoli.embed";
    const MAX_ATTEMPTS: i64 = 5;

    async fn run(self) -> crate::Result<()> {
        let Some((table, vector, sources, fields)) = find(&self.table, |a| (a.table, a.vector, a.sources.clone(), a.fields)) else {
            return Ok(());
        };
        let Some(embed) = crate::api::EMBED.get() else { return Ok(()) };
        let mut q = crate::orm::Query { table, fields, filter: vec![], order: vec![("id", false)], limit: Some(BATCH), offset: None };
        q.filter.push(crate::orm::Node::IsNull(vector, false));
        let rows = q.rows().await?;
        for (id, vals) in &rows {
            let text: Vec<String> = sources
                .iter()
                .filter_map(|s| fields.iter().position(|f| f.name == *s))
                .filter_map(|i| match &vals[i] {
                    Value::Text(t) if !t.trim().is_empty() => Some(t.clone()),
                    _ => None,
                })
                .collect();
            let v = crate::Vector(embed(text.join("\n")).await?);
            let ty = fields.iter().find(|f| f.name == vector).map_or(FieldType::Vector(0), |f| f.ty);
            let mut one =
                crate::orm::Query { table, fields, filter: vec![crate::orm::by_id(*id)], order: vec![], limit: None, offset: None };
            one.filter.push(crate::orm::Node::IsNull(vector, false)); // only if still missing
            one.update(&[(vector, Value::from(v), ty)]).await?;
        }
        if rows.len() as u64 == BATCH {
            requeue(table).await?; // more to do
        }
        Ok(())
    }
}

/// Queue a fill for `table` unless one is already waiting.
pub(crate) async fn requeue(table: &str) -> crate::Result<()> {
    let job = Fill { table: table.to_string() };
    crate::tasks::Task::enqueue_unique(&job, table).await.map(drop)
}

/// After a write to `table`: if it has auto-embedding, make sure a fill is queued. Boxed,
/// because queueing is itself a write (the task row).
pub(crate) fn after_write(table: &str) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + '_>> {
    Box::pin(async move {
        if find(table, |_| ()).is_some() {
            requeue(table).await?;
        }
        Ok(())
    })
}

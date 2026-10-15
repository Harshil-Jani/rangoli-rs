//! Rules about a whole row, checked before every write (Django's `Model.clean()`):
//!
//! ```ignore
//! App::new().validate::<Booking>(|b| {
//!     if b.ends_at <= b.starts_at {
//!         return Err(vec![("ends_at", "Must be after the start.".into())]);
//!     }
//!     Ok(())
//! })
//! ```
//!
//! Runs for admin adds and changes, API creates, replaces and partial updates (merged with the
//! stored row first, so the rule sees the whole row) and CSV imports. Errors come back like
//! field errors: a 400 with `{field: [message]}` from the API, inline in the admin.

use crate::orm::{FieldType, Model, ModelMeta, Value};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

/// What a rule returns: field name and message pairs (use `"__all__"` for the row as a whole).
pub type Errors = Vec<(&'static str, String)>;

type Rule = Arc<dyn Fn(Option<i64>, Vec<Value>) -> Result<(), Errors> + Send + Sync>;

static RULES: LazyLock<RwLock<Vec<(&'static str, Rule)>>> = LazyLock::new(Default::default);

pub(crate) fn register<M: Model>(rule: impl Fn(&M) -> Result<(), Errors> + Send + Sync + 'static) {
    let erased: Rule = Arc::new(move |id, vals| match M::from_values(id, vals) {
        Ok(row) => rule(&row),
        Err(e) => Err(vec![("__all__", e.to_string())]),
    });
    RULES.write().unwrap_or_else(PoisonError::into_inner).push((M::TABLE, erased));
}

/// Check a write of `cols` to `meta` (row `id` when updating), against the row as it would be
/// saved: the stored values with `cols` on top, or the type's zero value for a new row.
pub(crate) async fn check(meta: &'static ModelMeta, id: Option<i64>, cols: &[(&'static str, Value, FieldType)]) -> crate::Result<Result<(), HashMap<String, String>>> {
    let rules: Vec<Rule> =
        RULES.read().unwrap_or_else(PoisonError::into_inner).iter().filter(|(t, _)| *t == meta.table).map(|(_, r)| r.clone()).collect();
    if rules.is_empty() {
        return Ok(Ok(()));
    }
    let mut vals: Vec<Value> = match id {
        Some(id) => {
            let mut q = crate::orm::Query::new(meta);
            q.filter.push(crate::orm::by_id(id));
            q.rows().await?.pop().map(|(_, v)| v).unwrap_or_default()
        }
        None => vec![],
    };
    if vals.len() != meta.fields.len() {
        vals = meta.fields.iter().map(|f| if f.null { Value::Null } else { crate::migrate::zero(f.ty) }).collect();
    }
    for (name, v, _) in cols {
        if let Some(i) = meta.fields.iter().position(|f| f.name == *name) {
            vals[i] = v.clone();
        }
    }
    let mut errors = HashMap::new();
    for rule in rules {
        if let Err(errs) = rule(id, vals.clone()) {
            for (field, msg) in errs {
                errors.entry(field.to_string()).or_insert(msg);
            }
        }
    }
    Ok(if errors.is_empty() { Ok(()) } else { Err(errors) })
}

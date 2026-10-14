//! The admin, modeled on Django's. The admin log model and the `ModelAdmin` settings
//! are always available (the tables exist whichever features an app enables, so
//! switching a feature never changes the schema); the pages need the `admin` feature.

use crate::orm::{FieldType, Model, QuerySet};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

#[cfg(feature = "admin")]
mod csv;
#[cfg(feature = "admin")]
mod views;
#[cfg(feature = "admin")]
pub use views::router;

tokio::task_local! {
    /// The staff user running the current admin action.
    pub(crate) static ACTING: crate::auth::User;
}

/// Who is running the admin action in progress, for actions that record a decision.
pub fn acting_user() -> Option<crate::auth::User> {
    ACTING.try_with(Clone::clone).ok()
}

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
    /// The row before an API or MCP change or delete, as JSON (`{field: value}`), so History can
    /// undo it. Encrypted fields stay encrypted in here.
    #[field(text)]
    pub snapshot: Option<String>,
}

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
    pub(crate) list_display: Option<Vec<&'static str>>,
    pub(crate) search_fields: Option<Vec<&'static str>>,
    pub(crate) list_filter: Option<Vec<&'static str>>,
    pub(crate) ordering: Option<(&'static str, bool)>,
    pub(crate) readonly_fields: Vec<&'static str>,
    pub(crate) exclude: Vec<&'static str>,
    pub(crate) list_per_page: Option<u64>,
    pub(crate) no_add: bool,
    pub(crate) actions: Vec<Action>,
    pub(crate) inlines: Vec<Inline>,
    /// Fields shown only while another field has one of some values: (field, controller, values).
    pub(crate) show_when: Vec<(&'static str, &'static str, Vec<String>)>,
    /// Which rows each staff member may see and change (`ModelAdmin::rows`).
    pub(crate) rows: Option<crate::orm::Policy>,
}

/// Child rows edited on the parent's page: the child model and its foreign key to the parent.
/// Without the `admin` feature there are no pages to show it on.
#[cfg_attr(not(feature = "admin"), allow(dead_code))]
#[derive(Clone, Debug)]
pub(crate) struct Inline {
    pub(crate) meta: &'static crate::orm::ModelMeta,
    pub(crate) fk: &'static str,
    /// Blank rows for adding.
    pub(crate) extra: usize,
}

type ActionFn = dyn Fn(Vec<i64>) -> Pin<Box<dyn Future<Output = crate::Result<()>> + Send>> + Send + Sync;

/// A custom changelist action, type-erased: (name, label, run on the selected ids).
/// Without the `admin` feature there are no pages to run it from, so only `name` is read.
#[cfg_attr(not(feature = "admin"), allow(dead_code))]
#[derive(Clone)]
pub(crate) struct Action {
    pub(crate) name: &'static str,
    pub(crate) label: &'static str,
    pub(crate) run: Arc<ActionFn>,
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Action({})", self.name)
    }
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

    /// A changelist action, Django's `ModelAdmin.actions`: `run` gets a queryset of the selected
    /// rows. It runs in one transaction (an `Err` undoes it), each row gets a History entry, and
    /// the admin says "Applied “label” to N posts.". `name` is the form value, so keep it stable.
    ///
    /// ```ignore
    /// .action("publish", "Mark selected as published", |selected| async move {
    ///     selected.update([Post::PUBLISHED.set(true)]).await.map(drop)
    /// })
    /// ```
    pub fn action<F, Fut>(mut self, name: &'static str, label: &'static str, run: F) -> Self
    where
        F: Fn(QuerySet<M>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<()>> + Send + 'static,
    {
        let run = Arc::new(run);
        let erased: Arc<ActionFn> = Arc::new(move |ids: Vec<i64>| {
            let run = run.clone();
            Box::pin(async move { run(M::objects().filter(crate::orm::Col::<M, i64>::new("id").is_in(ids))).await })
        });
        self.opts.actions.retain(|a| a.name != name);
        self.opts.actions.push(Action { name, label, run: erased });
        self
    }

    /// Leave fields out of the add and change forms entirely (Django's `exclude`): not shown,
    /// not editable, existing values kept. For secrets staff shouldn't see either.
    pub fn exclude<const N: usize>(mut self, cols: [&dyn Field<M>; N]) -> Self {
        self.opts.exclude = Self::names(cols);
        self
    }

    /// Row-level permissions in the admin: the rows each staff member may list, open, change,
    /// delete and run actions on, as a filter (the same closure shape as `Api::rows`). Other
    /// rows are a 404; a save that would move a row outside them is refused.
    pub fn rows(mut self, policy: impl Fn(Option<&crate::auth::User>) -> Option<crate::orm::Expr<M>> + Send + Sync + 'static) -> Self
    where
        M: 'static,
    {
        self.opts.rows = Some(crate::orm::Policy::new(policy));
        self
    }

    /// Show `field` only while `controller` holds one of `values`: a "Delivery" choice that
    /// reveals the address, a checkbox that reveals a reason. Booleans compare as `"true"` and
    /// `"false"`. A hidden field isn't validated or changed by the save (it keeps its value, or
    /// its default when adding), so give it a default or make it nullable.
    pub fn show_when<const N: usize>(mut self, field: &dyn Field<M>, controller: &dyn Field<M>, values: [&str; N]) -> Self {
        let (f, c) = (field.name(), controller.name());
        self.opts.show_when.retain(|(x, ..)| *x != f);
        self.opts.show_when.push((f, c, values.iter().map(|v| v.to_string()).collect()));
        self
    }

    /// Edit `C` rows that point at this model on its change page (Django's inlines): an order's
    /// line items under the order. `fk` is `C`'s foreign key to this model. Parent and children
    /// save in one transaction; three blank rows let you add more.
    pub fn inline<C: Model>(mut self, fk: crate::orm::Col<C, i64>) -> Self {
        self.opts.inlines.push(Inline { meta: C::meta(), fk: fk.name(), extra: 3 });
        self
    }

    /// No "Add" in the admin (Django's `has_add_permission` returning False), for rows made
    /// elsewhere: API tokens, task runs, imports. Change and delete still work.
    pub fn no_add(mut self) -> Self {
        self.opts.no_add = true;
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
        for i in &self.opts.inlines {
            if i.meta.field(i.fk).and_then(|f| f.fk) != Some(meta.table) {
                return Err(format!("{}: inline {} needs `{}` to be a foreign key to {}", meta.name, i.meta.name, i.fk, meta.table));
            }
        }
        for n in self.opts.search_fields.iter().flatten() {
            if !field(n).is_some_and(|f| matches!(f.ty, FieldType::Varchar(_) | FieldType::Text) && !f.password) {
                return Err(format!("{}: search_fields can only use text columns, not `{n}`", meta.name));
            }
        }
        for n in self.opts.list_filter.iter().flatten() {
            if !field(n).is_some_and(|f| {
                matches!(f.ty, FieldType::Bool | FieldType::DateTime | FieldType::Date) || f.fk.is_some() || f.choices.is_some()
            }) {
                return Err(format!("{}: list_filter supports boolean, date, choice and foreign key columns, not `{n}`", meta.name));
            }
        }
        Ok(())
    }

    /// Settings that work but will get slow as the table grows, printed at startup.
    pub(crate) fn warnings(&self) -> Vec<String> {
        let meta = M::meta();
        let indexed = |n: &str| {
            n == "id"
                || meta.field(n).is_some_and(|f| f.index || f.unique || f.fk.is_some())
                || meta.indexes.iter().any(|ix| ix.first() == Some(&n))
        };
        match self.opts.ordering {
            Some((n, _)) if !indexed(n) => vec![format!(
                "{}: the admin orders by `{n}`, which has no index, so every changelist page sorts the whole table \
                 (86 ms per page at 1M rows in our measurement). Add #[field(index)] to `{n}`.",
                meta.name
            )],
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::User;

    #[test]
    fn admin_checks_reject_unusable_settings() {
        assert!(ModelAdmin::<User>::new().search_fields([&User::USERNAME]).list_filter([&User::IS_STAFF]).check().is_ok());
        let err = ModelAdmin::<User>::new().search_fields([&User::PASSWORD]).check().unwrap_err();
        assert!(err.contains("search_fields"), "{err}");
        let err = ModelAdmin::<User>::new().list_filter([&User::USERNAME]).check().unwrap_err();
        assert!(err.contains("list_filter"), "{err}");
        assert!(ModelAdmin::<User>::new().ordering(User::USERNAME.asc()).warnings().is_empty(), "unique columns are indexed");
        let warned = ModelAdmin::<User>::new().ordering(User::IS_STAFF.desc()).warnings();
        assert!(warned[0].contains("no index"), "{warned:?}");
    }
}

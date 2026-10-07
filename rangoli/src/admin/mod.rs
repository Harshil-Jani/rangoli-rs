//! The admin, modeled on Django's. The admin log model and the `ModelAdmin` settings
//! are always available (the tables exist whichever features an app enables, so
//! switching a feature never changes the schema); the pages need the `admin` feature.

use crate::orm::{FieldType, Model};
use std::marker::PhantomData;

#[cfg(feature = "admin")]
mod views;
#[cfg(feature = "admin")]
pub use views::router;

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
    pub(crate) list_per_page: Option<u64>,
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

    pub fn list_per_page(mut self, n: u64) -> Self {
        self.opts.list_per_page = Some(n.max(1));
        self
    }

    /// Reject settings that can't work, at startup (Django's admin checks).
    pub(crate) fn check(&self) -> std::result::Result<(), String> {
        let meta = M::meta();
        let field = |n: &str| meta.field(n);
        for n in self.opts.search_fields.iter().flatten() {
            if !field(n).is_some_and(|f| matches!(f.ty, FieldType::Varchar(_) | FieldType::Text) && !f.password) {
                return Err(format!("{}: search_fields can only use text columns, not `{n}`", meta.name));
            }
        }
        for n in self.opts.list_filter.iter().flatten() {
            if !field(n).is_some_and(|f| matches!(f.ty, FieldType::Bool | FieldType::DateTime) || f.fk.is_some() || f.choices.is_some()) {
                return Err(format!("{}: list_filter supports boolean, date, choice and foreign key columns, not `{n}`", meta.name));
            }
        }
        Ok(())
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
    }
}

//! Your own pages: templates (with Django 6.0 style partials), and model forms.
//!
//! ```ignore
//! async fn posts(Query(p): Query<Vec<(String, String)>>) -> rangoli::Result<Html<String>> {
//!     let posts = Post::objects().all().await?;
//!     // `posts.html#rows` renders only the `rows` block: perfect for an htmx swap.
//!     let name = if p.iter().any(|(k, _)| k == "partial") { "posts.html#rows" } else { "posts.html" };
//!     rangoli::web::render(name, context! { posts })
//! }
//! ```

use crate::admin::Field;
use crate::orm::{Model, Value};
use crate::{Error, Result};
use axum::response::Html;
use minijinja::Environment;
use serde::Serialize;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::OnceLock;

pub use minijinja::context;

static DIR: OnceLock<PathBuf> = OnceLock::new();
static CACHED: OnceLock<Environment<'static>> = OnceLock::new();

pub(crate) fn set_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_loader(minijinja::path_loader(DIR.get().cloned().unwrap_or_else(|| crate::settings().templates.clone())));
    env
}

fn template_error(e: minijinja::Error) -> Error {
    Error::Template(format!("{e:#}"))
}

/// Render a template from the templates directory (`RANGOLI_TEMPLATES`, default `templates`).
///
/// `"page.html#block"` renders only that block, like Django 6.0's template partials.
/// HTML templates are auto-escaped. With `RANGOLI_DEBUG=1` templates are re-read on every
/// request; otherwise they are parsed once and cached.
pub fn render(name: &str, ctx: impl Serialize) -> Result<Html<String>> {
    let fresh;
    let env = if crate::settings().debug {
        fresh = environment();
        &fresh
    } else {
        CACHED.get_or_init(environment)
    };
    let (file, block) = name.split_once('#').map_or((name, None), |(f, b)| (f, Some(b)));
    let tmpl = env.get_template(file).map_err(template_error)?;
    let html = match block {
        // ponytail: renders the page once to get the block's state; fine until pages get heavy.
        Some(b) => tmpl.render_captured(ctx).and_then(|mut page| page.with_state_mut(|state| state.render_block(b))),
        None => tmpl.render(ctx),
    }
    .map_err(template_error)?;
    Ok(Html(html))
}

/// Field name -> message, ready to show next to each input.
pub type FormErrors = HashMap<String, String>;

/// Django's `ModelForm`: validate submitted form data with the same rules as the admin
/// and get a typed model back.
///
/// ```ignore
/// async fn signup(Form(data): Form<Vec<(String, String)>>) -> rangoli::Result<Response> {
///     match ModelForm::<Author>::new().fields([&Author::NAME, &Author::EMAIL]).validate(&data).await {
///         Ok(mut author) => { author.save().await?; Ok(Redirect::to("/thanks").into_response()) }
///         Err(errors) => Ok(render("signup.html", context! { errors, data })?.into_response()),
///     }
/// }
/// ```
pub struct ModelForm<M> {
    only: Option<Vec<&'static str>>,
    instance: Option<M>,
    _m: PhantomData<fn() -> M>,
}

impl<M: Model> Default for ModelForm<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Model> ModelForm<M> {
    pub fn new() -> Self {
        ModelForm { only: None, instance: None, _m: PhantomData }
    }

    /// Accept only these fields from the form; the rest keep their current or default values.
    pub fn fields<const N: usize>(mut self, cols: [&dyn Field<M>; N]) -> Self {
        self.only = Some(cols.iter().map(|c| c.name()).collect());
        self
    }

    /// Edit an existing object instead of creating a new one.
    pub fn instance(mut self, m: M) -> Self {
        self.instance = Some(m);
        self
    }

    /// Validate `data` (form pairs, as from `Form<Vec<(String, String)>>`). Nothing is saved.
    pub async fn validate(&self, data: &[(String, String)]) -> std::result::Result<M, FormErrors> {
        let meta = M::meta();
        let form: HashMap<String, String> = data.iter().cloned().collect();
        let adding = self.instance.is_none();
        // Fields outside `fields(...)` are treated as read-only: whatever was posted is ignored.
        let readonly: Vec<&str> = match &self.only {
            Some(only) => meta.fields.iter().map(|f| f.name).filter(|n| !only.contains(n)).collect(),
            None => vec![],
        };
        let cols = crate::admin::validate(meta, &form, adding, &readonly).await?;
        let mut values = match &self.instance {
            Some(m) => m.values(),
            None => meta
                .fields
                .iter()
                .map(|f| match f.default {
                    Some(d) => d.value(),
                    None if f.null => Value::Null,
                    None => crate::migrate::zero(f.ty),
                })
                .collect(),
        };
        for (name, v, _) in cols.into_iter().filter(|(n, ..)| !readonly.contains(n)) {
            if let Some(i) = meta.fields.iter().position(|f| f.name == name) {
                values[i] = v;
            }
        }
        let id = self.instance.as_ref().and_then(Model::pk);
        M::from_values(id, values).map_err(|e| [("__all__".to_string(), e.to_string())].into())
    }
}

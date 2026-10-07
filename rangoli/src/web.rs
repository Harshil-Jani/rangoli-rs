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

use crate::{Error, Result};
use axum::response::Html;
use minijinja::Environment;
use serde::Serialize;
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

pub use crate::forms::{FormErrors, ModelForm};

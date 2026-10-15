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
    // Translations: {{ t("key") }}, {{ t("hello", name=user) }}, {{ t_n("cart.items", n) }}, {{ lang() }}.
    env.add_function("t", |key: String, kwargs: minijinja::value::Kwargs| -> String {
        let mut s = crate::i18n::t(&key);
        for name in kwargs.args() {
            if let Ok(v) = kwargs.get::<minijinja::Value>(name) {
                s = s.replace(&format!("{{{name}}}"), &v.to_string());
            }
        }
        s
    });
    env.add_function("t_n", |key: String, count: i64| crate::i18n::t_n(&key, count));
    env.add_function("lang", crate::i18n::language);
    filters::add(&mut env);
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

/// Django's handy template filters: `{{ post.created_at|date("%d %b %Y") }}`,
/// `{{ post.created_at|timesince }}`, `{{ views|intcomma }}`, `{{ n }} item{{ n|pluralize }}`,
/// `{{ body|truncatewords(30) }}`, `{{ note|linebreaksbr }}`, `{{ size|filesizeformat }}`.
mod filters {
    use crate::DateTime;
    use minijinja::value::Value;
    use minijinja::Environment;

    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

    fn when(v: &Value) -> Option<DateTime> {
        v.as_str().and_then(DateTime::parse).or_else(|| i64::try_from(v.clone()).ok().map(DateTime::from_unix))
    }

    /// strftime's common codes: %Y %m %d %e %H %M %S %b %B %a %A %%.
    pub(super) fn date(v: Value, fmt: Option<String>) -> String {
        let Some(t) = when(&v) else { return v.to_string() };
        let (y, mo, d, h, mi, sec) = t.parts();
        let wd = (t.unix().div_euclid(86_400) + 4).rem_euclid(7) as usize;
        let month = MONTHS[(mo - 1) as usize];
        let mut out = String::new();
        let mut chars = fmt.as_deref().unwrap_or("%d %b %Y").chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('Y') => out.push_str(&y.to_string()),
                Some('m') => out.push_str(&format!("{mo:02}")),
                Some('d') => out.push_str(&format!("{d:02}")),
                Some('e') => out.push_str(&d.to_string()),
                Some('H') => out.push_str(&format!("{h:02}")),
                Some('M') => out.push_str(&format!("{mi:02}")),
                Some('S') => out.push_str(&format!("{sec:02}")),
                Some('b') => out.push_str(&month[..3]),
                Some('B') => out.push_str(month),
                Some('a') => out.push_str(&DAYS[wd][..3]),
                Some('A') => out.push_str(DAYS[wd]),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        }
        out
    }

    /// "3 hours ago", "2 days ago", "just now".
    pub(super) fn timesince(v: Value) -> String {
        let Some(t) = when(&v) else { return v.to_string() };
        let secs = (DateTime::now().unix() - t.unix()).max(0);
        let (n, unit) = match secs {
            s if s < 60 => return "just now".into(),
            s if s < 3_600 => (s / 60, "minute"),
            s if s < 86_400 => (s / 3_600, "hour"),
            s if s < 30 * 86_400 => (s / 86_400, "day"),
            s if s < 365 * 86_400 => (s / (30 * 86_400), "month"),
            s => (s / (365 * 86_400), "year"),
        };
        format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
    }

    /// 1234567 -> "1,234,567".
    pub(super) fn intcomma(v: Value) -> String {
        let s = v.to_string();
        let (sign, rest) = s.strip_prefix('-').map_or(("", s.as_str()), |r| ("-", r));
        let (int, frac) = rest.split_once('.').map_or((rest, None), |(i, f)| (i, Some(f)));
        if !int.bytes().all(|b| b.is_ascii_digit()) {
            return s;
        }
        let mut out = String::new();
        for (i, ch) in int.chars().enumerate() {
            if i > 0 && (int.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        format!("{sign}{out}{}", frac.map(|f| format!(".{f}")).unwrap_or_default())
    }

    /// "" for 1, "s" otherwise; `pluralize("y,ies")` for other endings.
    pub(super) fn pluralize(v: Value, forms: Option<String>) -> String {
        let n = i64::try_from(v).unwrap_or(0);
        let forms = forms.unwrap_or_else(|| "s".into());
        let (one, many) = forms.split_once(',').unwrap_or(("", &forms));
        (if n == 1 { one } else { many }).to_string()
    }

    pub(super) fn truncatewords(v: String, n: usize) -> String {
        let words: Vec<&str> = v.split_whitespace().collect();
        if words.len() <= n {
            v
        } else {
            format!("{} …", words[..n].join(" "))
        }
    }

    /// Line breaks as <br>, with the text itself escaped first.
    pub(super) fn linebreaksbr(v: String) -> Value {
        let escaped = v.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;");
        Value::from_safe_string(escaped.replace("\r\n", "\n").replace('\n', "<br>"))
    }

    /// 1536 -> "1.5 KB".
    pub(super) fn filesizeformat(v: Value) -> String {
        let n = i64::try_from(v).unwrap_or(0) as f64;
        let units = ["bytes", "KB", "MB", "GB", "TB"];
        let mut size = n;
        let mut u = 0;
        while size >= 1024.0 && u < units.len() - 1 {
            size /= 1024.0;
            u += 1;
        }
        if u == 0 { format!("{n} bytes") } else { format!("{size:.1} {}", units[u]) }
    }

    pub(super) fn add(env: &mut Environment<'static>) {
        env.add_filter("date", date);
        env.add_filter("timesince", timesince);
        env.add_filter("intcomma", intcomma);
        env.add_filter("pluralize", pluralize);
        env.add_filter("truncatewords", truncatewords);
        env.add_filter("linebreaksbr", linebreaksbr);
        env.add_filter("filesizeformat", filesizeformat);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn filters() {
            assert_eq!(date(Value::from("2027-01-15T08:00:00Z"), Some("%a %e %B %Y, %H:%M".into())), "Fri 15 January 2027, 08:00");
            assert_eq!(intcomma(Value::from(1234567)), "1,234,567");
            assert_eq!(intcomma(Value::from(-1234.5)), "-1,234.5");
            assert_eq!((pluralize(Value::from(1), None), pluralize(Value::from(2), Some("y,ies".into()))), ("".to_string(), "ies".to_string()));
            assert_eq!(truncatewords("one two three".into(), 2), "one two …");
            assert_eq!(linebreaksbr("a<b>\nc".into()).to_string(), "a&lt;b&gt;<br>c");
            assert_eq!(filesizeformat(Value::from(1536)), "1.5 KB");
        }
    }
}

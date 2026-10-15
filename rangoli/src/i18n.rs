//! Translations: `App::locales("locales")` loads `locales/en.json`, `locales/hi.json`, ...
//!
//! ```json
//! { "cart": { "empty": "Your cart is empty", "items": { "one": "{count} item", "other": "{count} items" } },
//!   "hello": "Hello, {name}!" }
//! ```
//!
//! ```ignore
//! i18n::t("cart.empty");                              // in the request's language
//! i18n::tr("hello", &[("name", &user.username)]);
//! i18n::t_n("cart.items", 3);                         // "3 items"
//! ```
//!
//! In templates: `{{ t("cart.empty") }}`, `{{ t("hello", name=user) }}`, `{{ t_n("cart.items", n) }}`
//! and `{{ lang() }}`. Each request's language comes from `?lang=xx` (remembered in a cookie),
//! then that cookie, then the browser's `Accept-Language`, then the default (`RANGOLI_LANGUAGE`,
//! or the first file alphabetically). A missing text falls back to the default language, then
//! to the key itself, so a gap is visible but never an error.

use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use std::collections::HashMap;
use std::sync::OnceLock;

struct Catalogs {
    default: String,
    langs: HashMap<String, HashMap<String, String>>,
}

static CATALOGS: OnceLock<Catalogs> = OnceLock::new();
const COOKIE: &str = "rangoli_lang";

tokio::task_local! {
    static LANG: String;
}

fn flatten(prefix: &str, v: &serde_json::Value, out: &mut HashMap<String, String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, v, out);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

/// Load every `<lang>.json` in `dir`. `App::locales` calls this.
pub fn load(dir: &std::path::Path) -> crate::Result<()> {
    let mut langs = HashMap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let lang = path.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
            let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)
                .map_err(|e| crate::Error::Config(format!("{}: {e}", path.display())))?;
            let mut flat = HashMap::new();
            flatten("", &json, &mut flat);
            langs.insert(lang, flat);
        }
    }
    let mut names: Vec<&String> = langs.keys().collect();
    names.sort();
    let default = std::env::var("RANGOLI_LANGUAGE")
        .ok()
        .map(|l| l.to_lowercase())
        .filter(|l| langs.contains_key(l))
        .or_else(|| names.first().map(|s| s.to_string()))
        .ok_or_else(|| crate::Error::Config(format!("no <lang>.json files in {}", dir.display())))?;
    let _ = CATALOGS.set(Catalogs { default, langs });
    Ok(())
}

/// The languages loaded, sorted.
pub fn languages() -> Vec<String> {
    let mut v: Vec<String> = CATALOGS.get().map(|c| c.langs.keys().cloned().collect()).unwrap_or_default();
    v.sort();
    v
}

/// The current request's language (or the default outside a request).
pub fn language() -> String {
    LANG.try_with(Clone::clone).ok().or_else(|| CATALOGS.get().map(|c| c.default.clone())).unwrap_or_else(|| "en".into())
}

/// Run `body` in `lang`, e.g. to render an email in its recipient's language from a task.
pub async fn with_language<T>(lang: &str, body: impl std::future::Future<Output = T>) -> T {
    LANG.scope(lang.to_lowercase(), body).await
}

fn lookup(key: &str) -> Option<String> {
    let c = CATALOGS.get()?;
    c.langs.get(&language()).and_then(|m| m.get(key)).or_else(|| c.langs.get(&c.default).and_then(|m| m.get(key))).cloned()
}

/// The text for `key` in the current language.
pub fn t(key: &str) -> String {
    lookup(key).unwrap_or_else(|| key.to_string())
}

/// The text for `key` with `{name}` placeholders filled in.
pub fn tr(key: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut s = t(key);
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), &value.to_string());
    }
    s
}

/// `key.one` for 1, else `key.other`, with `{count}` filled in.
/// ponytail: English-style one/other; languages with more plural forms need CLDR rules.
pub fn t_n(key: &str, count: i64) -> String {
    let form = if count == 1 { "one" } else { "other" };
    let text = lookup(&format!("{key}.{form}")).or_else(|| lookup(key)).unwrap_or_else(|| key.to_string());
    text.replace("{count}", &count.to_string())
}

/// The best loaded language for an `Accept-Language` header (`hi-IN,hi;q=0.9,en;q=0.8`).
fn from_accept(header: &str, have: &HashMap<String, HashMap<String, String>>) -> Option<String> {
    let mut wanted: Vec<(f32, String)> = header
        .split(',')
        .filter_map(|part| {
            let mut bits = part.trim().split(';');
            let tag = bits.next()?.trim().to_lowercase();
            let q = bits.find_map(|b| b.trim().strip_prefix("q=").and_then(|q| q.parse().ok())).unwrap_or(1.0);
            (!tag.is_empty() && q > 0.0).then_some((q, tag))
        })
        .collect();
    wanted.sort_by(|a, b| b.0.total_cmp(&a.0));
    wanted.into_iter().find_map(|(_, tag)| {
        let base = tag.split('-').next().unwrap_or(&tag).to_string();
        [tag, base].into_iter().find(|l| have.contains_key(l))
    })
}

pub(crate) async fn middleware(req: Request, next: Next) -> Response {
    let Some(c) = CATALOGS.get() else { return next.run(req).await };
    let asked = req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("lang="))).map(str::to_lowercase).filter(|l| c.langs.contains_key(l));
    let cookie = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| crate::auth::cookie_value(h, COOKIE))
        .map(str::to_lowercase)
        .filter(|l| c.langs.contains_key(l));
    let accept = req.headers().get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).and_then(|h| from_accept(h, &c.langs));
    let lang = asked.clone().or(cookie).or(accept).unwrap_or_else(|| c.default.clone());
    let mut res = LANG.scope(lang.clone(), next.run(req)).await;
    if let Some(l) = asked {
        if let Ok(v) = format!("{COOKIE}={l}; Path=/; Max-Age=31536000; SameSite=Lax").parse() {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    res.headers_mut().insert(header::CONTENT_LANGUAGE, lang.parse().unwrap_or(header::HeaderValue::from_static("en")));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_language_picks_the_best_loaded() {
        let have: HashMap<String, HashMap<String, String>> = [("en".to_string(), HashMap::new()), ("hi".to_string(), HashMap::new())].into();
        assert_eq!(from_accept("hi-IN,hi;q=0.9,en;q=0.8", &have).as_deref(), Some("hi"), "a region falls back to its language");
        assert_eq!(from_accept("fr;q=1,en;q=0.5", &have).as_deref(), Some("en"), "skips what isn't loaded");
        assert_eq!(from_accept("en;q=0.2,hi;q=0.9", &have).as_deref(), Some("hi"), "by weight, not order");
        assert_eq!(from_accept("de", &have), None);
        let mut flat = HashMap::new();
        flatten("", &serde_json::json!({ "cart": { "items": { "one": "{count} item" } } }), &mut flat);
        assert_eq!(flat["cart.items.one"], "{count} item");
    }
}

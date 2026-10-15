//! Translations: language from ?lang=, cookie and Accept-Language; fallbacks; plurals.

use axum::routing::get;
use axum::Router;
use rangoli::{i18n, testing, App};

#[tokio::test(flavor = "multi_thread")]
async fn each_request_in_its_language() {
    let dir = std::env::temp_dir().join(format!("rangoli-locales-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("en.json"), r#"{"hello": "Hello, {name}!", "cart": {"items": {"one": "{count} item", "other": "{count} items"}}, "only_en": "English only"}"#).unwrap();
    std::fs::write(dir.join("hi.json"), r#"{"hello": "नमस्ते, {name}!", "cart": {"items": {"one": "{count} चीज़", "other": "{count} चीज़ें"}}}"#).unwrap();
    let app = App::new().locales(&dir).routes(Router::new().route(
        "/greet",
        get(|| async { format!("{} | {} | {} | {}", i18n::tr("hello", &[("name", &"Asha")]), i18n::t_n("cart.items", 3), i18n::t("only_en"), i18n::t("missing.key")) }),
    ));
    testing::database(&app).await.unwrap();
    let c = testing::Client::new(&app);

    assert_eq!(c.get("/greet").await.body, "Hello, Asha! | 3 items | English only | missing.key", "the default, and a visible fallback");
    let hi = c.get("/greet?lang=hi").await;
    assert_eq!(hi.body, "नमस्ते, Asha! | 3 चीज़ें | English only | missing.key", "?lang= wins; gaps fall back to the default language");
    assert!(hi.headers["set-cookie"].to_str().unwrap().starts_with("rangoli_lang=hi"), "and is remembered");
    assert_eq!(hi.headers["content-language"], "hi");
    let mut req = c.get("/greet?lang=xx").await;
    assert!(req.body.starts_with("Hello"), "an unknown language is ignored");
    req = c.request(axum::http::Method::GET, "/greet", None, String::new()).await;
    assert!(req.body.starts_with("Hello"));
    std::fs::remove_dir_all(&dir).ok();
}

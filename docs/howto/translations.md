# Translations

```text
locales/en.json   {"hello": "Hello, {name}!", "cart": {"items": {"one": "{count} item", "other": "{count} items"}}}
locales/hi.json   {"hello": "नमस्ते, {name}!", "cart": {"items": {"one": "{count} चीज़", "other": "{count} चीज़ें"}}}
```

```rust
App::new().locales("locales")

i18n::tr("hello", &[("name", &user.username)]);   // in Rust
i18n::t_n("cart.items", n);
```

```jinja
{{ t("hello", name=user.username) }}  {{ t_n("cart.items", n) }}  <html lang="{{ lang() }}">
```

- **The language** comes from `?lang=hi` (remembered in a cookie), then that cookie, then the browser's `Accept-Language` (`hi-IN` uses `hi`), then `RANGOLI_LANGUAGE` or the first file.
- **A missing text** falls back to the default language, then to the key, so gaps show instead of failing.
- **Outside a request,** for example an email sent from a task, use `i18n::with_language("hi", async { ... })`.
- **Plurals** are one/other; languages with more plural forms aren't handled yet.

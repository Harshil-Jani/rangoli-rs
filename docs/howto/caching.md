# Caching that clears itself

```rust
use rangoli::cache;

let top = cache::cached("home:top-posts", &["blog_post"], Duration::from_secs(300), || async {
    Post::objects().order_by(Post::VIEWS.desc()).limit(10).all().await
}).await?;
```

Name the tables the value is built from. Any change to them through Rangoli (your code, the admin, the API, an AI agent) clears it:

- **On this server,** straight away.
- **On every other server,** over Postgres `LISTEN/NOTIFY`, with no Redis. Measured: 7 ms.
- **After commit,** for changes inside a transaction, so nobody caches the old rows in between.

A value computed while its tables changed isn't stored, so a slow page can't put stale data back. To drop a value by hand, use `cache::delete("key")` or `cache::clear_table("blog_post")`.

**Limits:** values live in each process's memory, up to 10,000 entries. Changes made outside Rangoli, such as raw SQL or another app, aren't seen; use a TTL for those.

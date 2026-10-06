# Rangoli

**A batteries-included, Django-style web framework for Rust.** Typed models, automatic migrations, a generated admin, and auth in a single binary, with fixes for the design gaps Django has carried for years.

> Django is named after a jazz guitarist. Rangoli is named after the Indian art of building one pattern from many small colorful pieces, which is what a batteries-included framework does.

![The Rangoli admin](docs/admin.png)

**Status: early (0.1).** The core works end to end and is tested on Postgres, MySQL and SQLite. Read [What's not done yet](#whats-not-done-yet) before building anything real on it.

## Quickstart

```toml
[dependencies]
rangoli = { git = "https://github.com/Harshil-Jani/rangoli-rs" }
tokio = { version = "1", features = ["full"] }
```

```rust
use rangoli::prelude::*;

#[derive(Model, Clone, Debug)]
#[model(table = "blog_author", display = "name")]
pub struct Author {
    pub id: Option<i64>,
    #[field(max_length = 100)]
    pub name: String,
    #[field(max_length = 254, unique)]
    pub email: String,
}

#[derive(Model, Clone, Debug)]
#[model(table = "blog_post", display = "title")]
pub struct Post {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub title: String,
    #[field(text)]
    pub body: String,
    pub published: bool,
    #[field(fk = Author)]
    pub author_id: i64,
    pub rating: Option<f64>, // Option = nullable
}

#[tokio::main]
async fn main() -> rangoli::Result<()> {
    App::new().admin::<Author>().admin::<Post>().run().await
}
```

Your binary is also your `manage.py`:

```sh
cargo run -- makemigrations          # writes migrations/<timestamp>_create_blog_author.json
cargo run -- migrate
cargo run -- createsuperuser admin
RANGOLI_DEBUG=1 cargo run            # runserver; admin at http://127.0.0.1:8000/admin/
```

A full example lives in [`examples/blog`](examples/blog/src/main.rs).

## Queries are checked by the compiler

```rust
let posts = Post::objects()
    .filter(Post::PUBLISHED.eq(true) & (Post::TITLE.contains("rust") | Post::RATING.gte(4.5)))
    .exclude(Post::AUTHOR_ID.eq(7))
    .order_by(Post::ID.desc())
    .limit(20)
    .all()
    .await?;

let n = Post::objects().filter(Post::RATING.is_null()).count().await?;
Post::objects().filter(Post::PUBLISHED.eq(false)).update([Post::PUBLISHED.set(true)]).await?;

let mut post = Post::get(id).await?;      // Error::NotFound becomes a 404 in handlers
post.title = "New title".into();
post.save().await?;

let authors = Author::in_bulk(posts.iter().map(|p| p.author_id)).await?; // one query, no N+1

// Django's transaction.atomic: every ORM call inside uses one transaction. Ok commits, Err rolls back.
rangoli::atomic(async {
    order.save().await?;
    Stock::objects().filter(Stock::ID.eq(item)).update([Stock::COUNT.set(left - 1)]).await?;
    Ok(())
})
.await?;
```

### Many-to-many

```rust
#[derive(Model)]
#[model(table = "blog_post", m2m(tags = Tag))]
pub struct Post { /* ... */ }

post.tags().add(&[&rust, &web]).await?;               // link (already-linked ones are skipped)
post.tags().set_ids([web_id]).await?;                 // make it exactly these, in one transaction
let tags = post.tags().all().await?;                  // or .query() to filter further
Post::objects().filter(Post::TAGS.has(rust_id));      // posts with a tag
Post::TAGS.reverse(&rust);                            // the other direction
```

The join table (`blog_post_tags`, one row per pair, a unique index so each pair appears once) is created by `makemigrations`, its rows go away with either side, the admin shows the relation as a multi-select, and the API reads and writes it as a list of ids.

`Post::TITLE` is a `Col<Post, String>`. A typo, a string compared to an integer column, or a column from another model in the filter **does not compile**. In Django, `filter(titel__icontains=...)` only fails at runtime.

## Django gaps this closes

| Django pain | Rangoli |
|---|---|
| Lookups are strings (`title__icontains`); typos fail at runtime | Typed column constants; mistakes are compile errors |
| Two branches that each add a migration conflict and need a "merge migration", even for unrelated tables | Migrations are timestamped files and the database records the *set* applied. Unrelated migrations from different branches both apply. A real conflict (two branches changing the same column) is caught and names both files |
| Lazy relation access hides N+1 queries | No lazy loading: a foreign key is an id. `in_bulk` fetches related rows in one query, and the admin batches FK labels the same way |
| ORM is sync-first; async needs `sync_to_async` | Async everywhere, built on tokio and sqlx |
| `SECRET_KEY` must be kept secret and rotated, and leaks are common | There is no `SECRET_KEY`: sessions are random keys stored in the database, and CSRF protection needs no token |
| CSRF tokens in every form, plus `csrf_exempt` footguns | Every unsafe request is checked with the browser's `Sec-Fetch-Site`/`Origin` headers. No tokens, and no way to forget them |
| CSP arrived in Django 6.0 as opt-in middleware | A strict CSP, `nosniff`, `X-Frame-Options` and `Referrer-Policy` are on by default (handlers can override them) |
| No built-in login throttling | After 5 failed logins a username is locked for 15 minutes. Unknown usernames take the same time to reject, so you can't tell which usernames exist |
| `startproject` ships with `DEBUG = True` | Debug is off unless `RANGOLI_DEBUG=1` |
| `get_object_or_404` boilerplate | `Error::NotFound` renders as 404 |
| Settings are an importable, mutable Python module | Typed `Settings` read from the environment (`RANGOLI_DATABASE_URL`, `RANGOLI_DEBUG`, `RANGOLI_BIND`, `RANGOLI_MIGRATIONS`) |
| Deploying means WSGI/ASGI, gunicorn, `collectstatic` and WhiteNoise | One binary. Admin templates, CSS and htmx are compiled in |
| Admin JS is jQuery-era | Server-rendered with htmx: live search, boosted navigation, and it still works without JavaScript |

## The admin

Modeled on Django's admin, page for page: the blue header, breadcrumbs, the app-grouped index with Recent actions, the nav sidebar, and the same wording. Registering a model with `.admin::<M>()` gives you:

- **Changelist**: "Select post to change", sortable columns, search with result counts (live as you type), boolean and date filters (Today, Past 7 days, This month, This year), pagination, an **Action** menu with "0 of N selected", and Django's confirmation page before a bulk delete
- **Change form**: labels on the left, `Please correct the error below.`, foreign keys as selects, unique and foreign-key violations reported on the form, and the Save / Save and add another / Save and continue editing / Delete row
- **Delete confirmation** that lists the protected related objects blocking the delete, or the related rows that cascade with it
- **History** for every object and **Recent actions** on the index, recorded in an admin log (Django's `LogEntry`), with change messages like "Changed title and body."
- Success messages like 'The post "X" was added successfully.'
- user management (password hashes are never shown; leave the field blank to keep the current password), **change password**, login and logout for staff only
- square edges throughout, light and dark themes, and a mobile layout

### Customizing a model's admin

Django's `ModelAdmin`, with typed columns: a misspelled field, or a field from another model, does not compile.

```rust
App::new().admin_with::<Post>(
    ModelAdmin::new()
        .list_display([&Post::TITLE, &Post::AUTHOR_ID, &Post::PUBLISHED, &Post::CREATED_AT])
        .search_fields([&Post::TITLE, &Post::BODY])
        .list_filter([&Post::PUBLISHED, &Post::AUTHOR_ID, &Post::CREATED_AT]) // bool, date and foreign key filters
        .ordering(Post::CREATED_AT.desc())
        .readonly_fields([&Post::RATING])
        .list_per_page(50),
)
```

Settings that can't work (searching a non-text column, filtering on an unsupported type) stop the app at startup, like Django's admin checks.

## Your own pages

```rust
App::new()
    .templates("templates")          // or RANGOLI_TEMPLATES; re-read every request in debug
    .static_files("/static", "static")
    .routes(Router::new().route("/", get(home)))

async fn home(Query(p): Query<Vec<(String, String)>>) -> rangoli::Result<Html<String>> {
    let posts = Post::objects().filter(Post::PUBLISHED.eq(true)).all().await?;
    let tags = Post::TAGS.prefetch(&posts).await?;          // Django's prefetch_related: 2 queries
    // `home.html#posts` renders only that block (Django 6.0 template partials): an htmx search
    // box can swap just the results without a second template.
    let page = if p.iter().any(|(k, _)| k == "partial") { "home.html#posts" } else { "home.html" };
    render(page, context! { posts, tags })
}
```

Templates are Jinja2 syntax (minijinja), auto-escaped for `.html`. Static files are served with content types and can't escape their directory. `ModelForm` validates submitted data with the admin's rules and hands back a typed, unsaved model:

```rust
match ModelForm::<Author>::new().fields([&Author::NAME, &Author::EMAIL]).validate(&data).await {
    Ok(mut author) => author.save().await?,             // typed Author
    Err(errors) => return render("signup.html", context! { errors }),  // field -> message
}
```

Fields left out of `fields(...)` ignore whatever is posted, so a form can't change them (`instance(obj)` edits an existing object). If you use htmx with the default CSP, set `<meta name="htmx-config" content='{"includeIndicatorStyles": false}'>` so it doesn't try to inject inline styles.

## Background tasks

Django 6.0 added a tasks interface but no production worker, so it still needs Celery plus Redis or RabbitMQ. Rangoli queues tasks in your database and runs them in the same binary:

```rust
#[derive(Serialize, Deserialize)]
struct SendWelcome { user_id: i64 }

impl Task for SendWelcome {
    const NAME: &'static str = "send_welcome";
    const MAX_ATTEMPTS: i64 = 5;                      // default 3
    async fn run(self) -> rangoli::Result<()> { /* ... */ Ok(()) }
}

App::new().task::<SendWelcome>();
SendWelcome { user_id: 7 }.enqueue().await?;          // or .enqueue_in(Duration::from_secs(60))
```

`runserver` runs `RANGOLI_WORKERS` task loops in-process (default 1, `0` turns them off); `cargo run -- worker` runs a dedicated worker. Runs are claimed with a compare-and-set update, so two workers never run the same one. Failures retry with exponential backoff (10s, 20s, 40s, up to an hour), panics are caught and recorded, and runs left `running` by a crashed worker are retried after a ten-minute lease. Every run is visible in the admin under **Task records**, filterable by status.

## JSON API

What Django needs Django REST framework and drf-spectacular for is one line:

```rust
App::new().api::<Post>(Api::new().read(Access::Public).write(Access::Staff))
```

| Request | Does |
|---|---|
| `GET /api/blog_post/?limit=20&offset=40&ordering=-created_at&search=rust&published=true` | Paginated list (`count`, `next`, `previous`, `results`), typed exact-match filters |
| `POST /api/blog_post/` | Create; `201` with the object |
| `GET /api/blog_post/7` | Detail; `404 {"detail": "Not found."}` |
| `PUT` / `PATCH /api/blog_post/7` | Replace / partial update |
| `DELETE /api/blog_post/7` | `204` |
| `GET /api/schema.json` | OpenAPI 3 document for every exposed model |

Validation uses the same rules as the admin and answers like DRF: `400 {"title": ["This field may not be blank."]}`. Access levels are `Public`, `Authenticated`, `Staff` (the default) and `Nobody`. Password fields are write-only, datetimes are ISO 8601, `auto_now` fields are filled in. Browser requests are covered by the same cross-origin protection as the admin.

## Databases

Postgres, MySQL and SQLite share one code path through sqlx's `Any` driver. Only the SQL spelling differs per dialect. CI runs the same end-to-end test on all three.

Field types: `i64`, `f64`, `bool`, `String` (`VARCHAR`, or `TEXT` with `#[field(text)]`), `DateTime`, `Json` and choice enums, each optionally wrapped in `Option` for a nullable column.

Choices are real Rust enums instead of Django's string tuples, so `Post::STATUS.eq(Status::Publisehd)` doesn't compile:

```rust
#[derive(Choices, Clone, Copy, Debug, PartialEq)]
pub enum Status { Draft, Published, #[choice(value = "old", label = "Archived for good")] Archived }

#[derive(Model)]
pub struct Post {
    // ...
    #[field(choices, default = "draft")]
    pub status: Status,     // VARCHAR, a dropdown and a filter in the admin, an `enum` in OpenAPI
    pub extra: Option<Json>, // any JSON document; validated in the admin, real JSON in the API
}
```

A stored value longer than the column's `max_length` is a compile error. `#[field(default = ...)]` takes a literal and is used for existing rows when `makemigrations` adds the column, as the initial value of new admin forms, and for fields an API client leaves out. `DateTime` is UTC, stored as Unix seconds so it behaves identically on every database, and serializes as ISO 8601. `#[field(auto_now_add)]` and `#[field(auto_now)]` fill `created_at`/`updated_at` style fields on save, and the admin keeps them read-only. Field attributes: `max_length`, `text`, `unique`, `index`, `choices`, `default = literal`, `password`, `fk = Model`, `auto_now`, `auto_now_add`, and `cascade` (`ON DELETE CASCADE`; foreign keys protect referenced rows by default).

## Migrations

```json
{
  "operations": [
    { "op": "add_column", "table": "blog_post", "column": { "name": "views", "type": "int" }, "default": 0 }
  ]
}
```

Operations: `create_table`, `drop_table`, `add_column`, `drop_column`, `alter_column`, `add_index`, `drop_index`, and `sql` (the equivalent of Django's RunSQL). Each migration runs in its own transaction. MySQL commits DDL automatically, just as it does under Django. `makemigrations --check` and `migrate --check` exit non-zero, for CI.

## What's not done yet

This is the roadmap, in rough order. Nothing here is implemented yet:

1. **More field types**: date, decimal, uuid; model-level composite indexes
2. Savepoints for nested `atomic` blocks (today a nested block joins the outer one)
3. **Relations**: `select_related`-style joins, reverse foreign key accessors, composite indexes from models
4. **More admin customization**: inlines, custom actions, fieldsets, per-model permissions, groups
5. **Form rendering helpers** (widgets from model metadata); template filters for dates and choices
6. **API tokens** for non-browser API clients, and per-object API permissions
7. **Scheduled (cron-style) tasks** on top of the task queue
8. **WebSockets** (what Channels does)
9. Password reset, groups, email, caching, i18n, embedding migrations in the binary, multi-database routing
10. **WebAssembly**, explored later: the same validation rules running in the browser and on the server, and sandboxed WASM plugins

Known limits today: login lockouts are per process, the admin's foreign-key select loads at most 1000 rows, and changing `unique`/`fk` on an existing column needs an `sql` operation on Postgres and MySQL.

## Development

```sh
cargo test                                                     # unit tests + end-to-end on SQLite
RANGOLI_TEST_DATABASE_URL=postgres://... cargo test --test e2e
RANGOLI_TEST_DATABASE_URL=mysql://...    cargo test --test e2e
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

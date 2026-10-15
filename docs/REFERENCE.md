# Rangoli reference

Everything the framework does, with how it's tested. The [README](../README.md) is the short version.

## Quickstart

```toml
[dependencies]
rangoli = "0.2"
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

A full example lives in [`examples/blog`](../examples/blog/src/main.rs).

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

// Foreign keys: `#[field(fk = Author)] author_id` also generates `Post::AUTHOR` and `post.author()`.
let author = post.author().await?;                       // one query
let authors = Post::AUTHOR.load(&posts).await?;          // one query for any number of posts: authors[&p.author_id]
let rows = Post::objects().filter(Post::PUBLISHED.eq(true)).select_related(Post::AUTHOR).await?; // one JOIN: Vec<(Post, Option<Author>)>
let theirs = Post::AUTHOR.reverse(&author).filter(Post::PUBLISHED.eq(true)); // Django's author.post_set
let by_author = Post::AUTHOR.prefetch_reverse(&authors_list).await?;          // one query: author id -> Vec<Post>

// Django's transaction.atomic: every ORM call inside uses one transaction. Ok commits, Err rolls back.
// A nested atomic is a savepoint: its Err undoes only its own writes.
rangoli::atomic(async {
    order.save().await?;
    Stock::objects().filter(Stock::ID.eq(item)).update([Stock::COUNT.set(left - 1)]).await?;
    Ok(())
})
.await?;
```

### Bulk inserts and upserts

```rust
Author::bulk_create(&mut authors).await?;                 // one INSERT per batch; ids filled in
Author::bulk_upsert(&mut authors, Author::EMAIL).await?;  // insert, or update the row with that email
author.upsert(Author::EMAIL).await?;
```

Upserts key on a unique column (anything else is refused), update everything but the key and creation times, and give every row its id, new or existing. 1,000 rows took 26 ms with `bulk_create` against 870 ms as a `save()` loop on Postgres over TCP (33×), and 3 ms against 586 ms on SQLite (best of 3).

### Totals, averages and GROUP BY

```rust
let revenue = Order::objects().filter(Order::PAID.eq(true)).sum(Order::TOTAL).await?;   // Option<Decimal<2>>, exact
let rating = Post::objects().avg(Post::RATING).await?;                                     // Option<f64>
let newest = Post::objects().max(Post::CREATED_AT).await?;                                 // Option<DateTime>
let per_author = Post::objects().group_by(Post::AUTHOR_ID).count().await?;                // Vec<(Option<i64>, i64)>, one query
let views = Post::objects().group_by(Post::AUTHOR_ID).sum(Post::VIEWS).await?;
```

Typed like everything else: summing a text column doesn't compile, and a `Decimal` total stays exact. Results are cast so all three databases hand back the same types (Postgres's `SUM(bigint)` is `numeric` otherwise). The e2e test checks every aggregate against the same numbers computed in Rust from the rows.

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
| Lazy relation access hides N+1 queries | No lazy loading: a foreign key is an id, and following it is an explicit `await`. `select_related` is a single JOIN, and `Post::AUTHOR.load(&posts)` and `prefetch_reverse` are one query each however many rows (asserted with `count_queries` in the test suite). For 50 posts with their authors, the JOIN took 593 µs against 1,066 µs for two queries on Postgres over TCP, and 159 against 187 µs on SQLite (best of 5 × 2,000 runs), and the admin batches FK labels the same way |
| ORM is sync-first; async needs `sync_to_async` | Async everywhere, built on tokio and sqlx |
| `SECRET_KEY` must be kept secret and rotated, and leaks are common | There is no `SECRET_KEY`: sessions are random keys stored in the database, and CSRF protection needs no token |
| CSRF tokens in every form, plus `csrf_exempt` footguns | Every unsafe request is checked with the browser's `Sec-Fetch-Site`/`Origin` headers. No tokens, and no way to forget them |
| CSP arrived in Django 6.0 as opt-in middleware | A strict CSP, `nosniff`, `X-Frame-Options` and `Referrer-Policy` are on by default (handlers can override them) |
| No built-in login throttling | After 5 failed logins a username is locked for 15 minutes, counted in the database so every instance and restart shares one count. Unknown usernames take the same time to reject and lock the same way, so you can't tell which usernames exist. Each attempt is counted with one atomic UPDATE before its password is checked, so of 50 parallel guesses at most 5 reach the check (counting after the check let all 50 through, which an independent review caught), and none of the 50 is lost (a read-then-write counter recorded 5 of 20 on Postgres) |
| `startproject` ships with `DEBUG = True` | Debug is off unless `RANGOLI_DEBUG=1` |
| `get_object_or_404` boilerplate | `Error::NotFound` renders as 404 |
| Settings are an importable, mutable Python module | Typed `Settings` read from the environment (`RANGOLI_DATABASE_URL`, `RANGOLI_DEBUG`, `RANGOLI_BIND`, `RANGOLI_MIGRATIONS`) |
| Deploying means WSGI/ASGI, gunicorn, `collectstatic` and WhiteNoise | One binary. Admin templates, CSS, htmx and (with `embed_migrations`) your migrations are compiled in |
| Admin JS is jQuery-era | Server-rendered with htmx: live search, boosted navigation, and it still works without JavaScript |

## The admin

Modeled on Django's admin, page for page: the blue header, breadcrumbs, the app-grouped index with Recent actions, the nav sidebar, and the same wording. Registering a model with `.admin::<M>()` gives you:

- **Changelist**: "Select post to change", sortable columns, search with result counts (live as you type), boolean and date filters (Today, Past 7 days, This month, This year), pagination, an **Action** menu with "0 of N selected", and Django's confirmation page before a bulk delete
- **Change form**: labels on the left, `Please correct the error below.`, foreign keys as selects, unique and foreign-key violations reported on the form, and the Save / Save and add another / Save and continue editing / Delete row
- **Delete confirmation** that lists the protected related objects blocking the delete, or the related rows that cascade with it
- **History** for every object and **Recent actions** on the index, recorded in an admin log (Django's `LogEntry`), with change messages like "Changed title and body."
- Success messages like 'The post "X" was added successfully.'
- **two-factor sign-in**: each staff member can turn on authenticator-app codes (RFC 6238) under **Two-factor**; from then on signing in needs the code. A code works once, and wrong codes count toward the lockout even with the right password, so knowing a password doesn't let anyone guess codes. (Django needs django-otp or allauth, [new-features #12](https://github.com/django/new-features/issues/12).)
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

Custom actions join "Delete selected" in the changelist's Action menu (Django's `ModelAdmin.actions`):

```rust
.action("publish", "Mark selected as published", |selected| async move {
    selected.update([Post::PUBLISHED.set(true)]).await.map(drop)   // `selected` is a typed QuerySet<Post>
})
```

An action runs in one transaction with a History entry for every row it touched, so an `Err` undoes both, and the admin reports "Applied “Mark selected as published” to 2 posts." or that it failed and nothing changed. The message is built from the registered label, so a crafted link can't make the admin display other text (the e2e test tries).

Child rows can be edited on the parent's page (Django's inlines; nested inline editing is [Trac #9025](https://code.djangoproject.com/ticket/9025), its most-followed open ticket): `.inline(LineItem::ORDER_ID)` lists an order's line items under the order, with blank rows to add more and a box to delete. The parent and every child row save in one transaction, so one bad row saves nothing; each child gets its own History entry, and a row id from another parent is refused. `.exclude([...])` leaves fields out of the forms entirely.

Fields can depend on other fields without writing any JavaScript (Wagtail's most-voted form request, wagtail#2172): `.show_when(&Order::ADDRESS, &Order::DELIVERY, ["courier", "post"])` shows the address only for those choices, and `.show_when(&Post::RATING, &Post::PUBLISHED, ["true"])` follows a checkbox. They toggle live as you edit. A hidden field is skipped on save, so it is neither validated nor changed: switching back keeps what was typed earlier. The browser check drives this in Chrome on every push.

`.no_add()` removes "Add" for rows made elsewhere (Django's `has_add_permission` returning `False`): the links disappear and the add URLs answer 403. API tokens use it: they appear in the admin under **API tokens** by name and prefix, where you can rename one, set an expiry or delete it to revoke it, but not create one, since the token itself is shown only once by `createtoken`.

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
    // In tests: let (_, n) = rangoli::orm::count_queries(load()).await; assert_eq!(n, 3);  (assertNumQueries)
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

`runserver` runs `RANGOLI_WORKERS` task loops in-process (default 1, `0` turns them off); `cargo run -- worker` runs a dedicated worker. Runs are claimed with a compare-and-set update, so two workers never run the same one. Failures retry with exponential backoff (10s, 20s, 40s, up to an hour), panics are caught and recorded, and runs left `running` by a crashed worker are retried after a ten-minute lease. Every run is visible in the admin under **Task records**, filterable by status, and failed runs can be retried from there.

To make sure work isn't queued twice (a double click, a retried request, two servers), use `ReindexProduct { id: 7 }.enqueue_unique("product-7")`. It returns `None` while a run with that key is waiting or running. The database's unique index decides: in the test, 10 racing enqueues queue exactly one run, and the key frees when the run ends.

Recurring work is Celery beat's job, which is a separate process: run two by accident and every job fires twice. Here it's one line, and no extra process:

```rust
App::new()
    .schedule("0 3 * * *", PurgeSessions)              // 03:00 UTC daily
    .schedule("*/15 * * * 1-5", SyncPrices { venue: "nse".into() })  // weekdays, every 15 min
```

Standard five-field cron in UTC (`*`, ranges, steps, lists, `@daily` and friends; Vixie day-of-month/day-of-week semantics), checked at startup. Every worker loop, in every process, offers each tick to the queue under a unique key, and the database accepts exactly one. The e2e test races 20 workers with separate memory on one tick on all three databases: one run is queued and it runs once. With the unique key removed, the same test queues it 20 times. A tick missed while every worker was down runs once on restart if it is less than ten minutes old, and only the latest such tick runs. Scheduled runs are ordinary task runs, so they get retries, backoff and the admin view.

## Webhooks

Add a webhook under **Webhooks** in the admin (a URL, optionally one table, which events, a secret) and every add, change or delete made through the admin, the API or an AI agent is POSTed to it as JSON: `{"event": "changed", "table": "blog_post", "id": 7, "object": "Hello", "message": "Changed via MCP.", "user": "alice", "at": "..."}`. That's Slack, Zapier, n8n or your own service without code. Deliveries are queued in the change's own transaction (a rolled-back change sends nothing), signed with `X-Rangoli-Signature: sha256=<HMAC of the body>`, and retried with backoff by the task queue. The row's data isn't in the payload, so a webhook can't leak private or encrypted fields. Tested with a local receiver: the right webhooks fire, signatures verify, and a failed delivery is retried. (HTTPS uses rustls with the standard root certificates. CI can't reach outside, so that path was checked by hand: a delivery to a public HTTPS echo service succeeded, and one to a site with a self-signed certificate was refused, `UnknownIssuer`, and left queued for retry.)

## Real-time: WebSockets and Server-Sent Events

Django needs Channels, a Redis channel layer and an ASGI server (daphne or uvicorn) for this, and moving a sync codebase onto them is a rewrite of its views. Rangoli is async throughout, so a socket is just another handler:

```rust
use rangoli::realtime;

async fn chat(ws: WebSocketUpgrade, headers: HeaderMap, Path(room): Path<String>) -> Response {
    // Each message a client sends goes to everyone in the room; return None to drop it.
    realtime::serve(ws, &headers, format!("chat:{room}"), |text| async move { Some(text) })
}

realtime::publish("chat:lobby", "deploy finished");      // from any view or background task
async fn feed() -> impl IntoResponse { realtime::events("dashboard") }  // the same groups as SSE
```

A WebSocket handshake is a `GET`, so ordinary CSRF checks don't cover it; `serve` refuses browsers whose `Origin` isn't your host (cross-site WebSocket hijacking). Slow clients skip ahead instead of stalling a room, idle groups are cleaned up, and connections are pinged every 30 seconds. On Postgres, `runserver` and `worker` join every instance through `LISTEN/NOTIFY`, so a publish from any web server or background worker reaches subscribers on all of them, with no Redis. Delivery across instances is at most once (like Channels' Redis layer), in publish order, with messages up to about 7.9 KB (bigger ones stay on the publishing instance). A live run with two servers and a worker on one database: a message posted to server B, and one published by a task in the worker, both reached an SSE client on server A. On MySQL and SQLite, groups live in one process.

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

For big tables, `?cursor=` (empty for the first page) switches to keyset pagination: each page links the next with `cursor=<last id>`, and there is no `COUNT(*)` and no `OFFSET`, so page 10,000 costs what page 1 does (Django's admin and DRF count every row, Trac [#8408](https://code.djangoproject.com/ticket/8408)). On a 1,000,000-row Postgres table, the page at row 990,000 took 1.2 ms by cursor against 120 ms with offset and count (best of 5). MCP's list tools take the same `cursor`.

Lists also answer the HTTP `QUERY` method ([new-features #185](https://github.com/django/new-features/issues/185)): the same options as a JSON body, `{"filters": {...}, "search": "...", "ordering": "-id", "limit": 20, "cursor": ""}`, for queries too long or too private to put in a URL. It returns exactly what the matching `GET` does.

Validation uses the same rules as the admin and answers like DRF: `400 {"title": ["This field may not be blank."]}`. Access levels are `Public`, `Authenticated`, `Staff` (the default) and `Nobody`.

Rows that belong to a user take one call instead of DRF's `IsOwner` permission class, a `get_queryset` override and a `perform_create` override:

```rust
App::new().api::<Note>(Api::new().read(Access::Authenticated).write(Access::Authenticated).owned_by(&Note::OWNER_ID))
```

Non-staff users then list, read, update and delete only their own notes. Someone else's is a 404, the same as a note that doesn't exist, so ids reveal nothing. Whatever owner a request body names is replaced by the requesting user, so nobody can create a row as someone else or hand one over. Staff see and reassign everything. `owned_by` on a column that isn't a foreign key to the user stops the app at startup. The e2e test has two users attempt every cross-user read and write on all three databases. With the owner filter removed from DELETE alone, the same test catches one user deleting the other's row.

Browser apps on another domain (a React or mobile web front end) need CORS, which Django leaves to the django-cors-headers package ([new-features #8](https://github.com/django/new-features/issues/8)). Here it's one line: `App::new().cors(["https://app.example.com"])`. Listed origins get preflight answers and may also make writes, which the cross-origin protection otherwise blocks. `"*"` lets any site read public data without cookies, but never write. `cors_credentials(true)` allows cookies and auth headers from listed origins.

Scripts, mobile apps and other services use API tokens (DRF's `authtoken`, without its plaintext storage):

```sh
cargo run -- createtoken alice ci-bot --days 90   # prints rgl_… once
curl -H "Authorization: Bearer rgl_…" -X POST localhost:8000/api/blog_post/ -d @post.json
```

or `rangoli::auth::create_token(&user, "ci-bot", Some(ttl))` in code. A token carries its user's rights and stops working when it expires, when its `ApiToken` row is deleted, or when the user is deactivated. Only a SHA-256 of the token is stored, so a database leak doesn't leak working tokens, and the `rgl_` prefix lets secret scanners recognise one pushed to a repository. A bad token is a `401 {"detail": "Invalid token."}` with `WWW-Authenticate: Bearer`. `last_used_at` is written at most once a minute per token, so checking a token is two reads and no write on hot paths (asserted with `count_queries`). Password fields are write-only, datetimes are ISO 8601, `auto_now` fields are filled in. Browser requests are covered by the same cross-origin protection as the admin.

### JWT sign-in

For mobile apps and single-page front ends, or when sign-in already lives with Clerk, Auth0 or Supabase Auth:

```rust
App::new().jwt(JwtAuth::hs256(secret).cookie("access_token"))      // tokens you issue: auth.issue(&user, ttl)
App::new().jwt(JwtAuth::jwks(&provider_jwks, "email"))              // a provider's RS256 tokens, matched by email
```

A valid token, in `Authorization: Bearer` or the cookie, signs the request in as its user, and every access rule and `owned_by` applies. A forged, expired or tampered token gets a 401. The checks are run in the order the attacks happen: `alg: none` is refused; a key only verifies its own algorithm, so an HS256 token "signed" with an RSA public key fails (the alg-confusion attack); `kid` picks the key; and `exp`/`nbf` allow 60 seconds of clock skew. Both RFC 7515 test vectors (HS256 and RS256) pass. JWTs can't open the admin, which still wants a password and 2FA.

## AI agents (MCP)

Every model you expose with `.api::<M>(...)` is also an MCP server at `POST /mcp` (Streamable HTTP, protocol 2025-06-18 back to 2024-11-05). Each model gets five tools, such as `blog_post_list`, `blog_post_get`, `blog_post_create`, `blog_post_update` and `blog_post_delete`, with input schemas generated from the model.

An agent is an ordinary API client. It signs in with an API token, from **Connect an AI agent** in the admin or `cargo run -- createtoken`, and each tool call runs through the JSON API as that token's user. Access levels, `owned_by`, validation and lockouts all apply unchanged, and an agent is only offered the tools its user can call. Every create, update and delete, through the API or MCP, writes a History entry ("Added via MCP.") in the same transaction as the change.

**Or just sign in.** claude.ai and Claude Desktop add a server as a custom connector by its address alone: paste `https://your-site/mcp`, and you're sent to a Rangoli page that asks what the agent may do (read only, ask before each change, or read and change). No token to copy. It's the MCP authorization flow: OAuth 2.1 metadata (RFC 9728 and 8414), dynamic client registration (RFC 7591), and only the authorization-code grant with PKCE S256 (RFC 7636's test vector is in the tests). The connection is an ordinary scoped API token that lasts 90 days and appears under **API tokens**, where deleting it disconnects the app. Codes live 60 seconds and are spent by a single `DELETE`, so a code redeemed twice, or tried with the wrong PKCE verifier, gets nothing. A sign-in can only return to an address the app registered (https, or http on this machine). A bad or expired token on `/mcp` answers 401 with the sign-in address, so a lapsed connection renews itself. Anonymous MCP still works where some data is public; where nothing is, it's a 401 that starts the sign-in.

Guardrails, all enforced by the server on every request, not just hidden from the tool list:

- **Read-only by default.** Agents connected from the admin can look things up but not change anything unless you tick "Allow changes". `createtoken --read-only` does the same from the command line.
- **Only the tables you pick.** A token limited to some tables sees the others as if they don't exist (404, and no tools for them).
- **Private fields.** `#[field(private)]` keeps a column (an API key, a tax id) out of every API and MCP response, filter, sort, search, write and schema. Staff still see it in the admin. The most-used Django MCP package has had open bugs where its field allowlists leak ([#66](https://github.com/gts360/django-mcp-server/issues/66), [#72](https://github.com/gts360/django-mcp-server/issues/72)); here the e2e test tries those probes, and with the check removed it catches the leak.
- **Tokens can't open the admin**, even a staff user's.
- **Ask me first.** An agent can be allowed to make changes only with approval. Its writes, over MCP or the API, are checked as usual and then wait under **Pending changes** in the admin; nothing is written until a person approves. An approved change runs as the user who asked, with permissions checked again, and History records "Added via MCP, approved by alice." The agent gets `{"pending": true}` straight away and an `approval_status` tool to see the outcome. (Replit's agent deleted a production database during a code freeze; a human in the loop is the fix the MCP spec recommends.)
- **Undo.** Every change or delete an agent (or any API client) makes stores the row as it was, and its History page offers **Undo** on the newest one: an add is removed, a change is put back, and a delete comes back with the same id. Undo is in History too. (Replit shipped one-click restore after its agent deleted a production database.)
- **Untrusted data is labelled.** Rows an agent reads come with a note that they are data typed by users, not instructions. That lowers the odds of a prompt injection working; it doesn't prevent one, which is why everything above is enforced by the server instead of left to the model.
- **Rate limits.** MCP allows 120 requests a minute per caller by default (the MCP spec says servers must rate limit tool calls), so a runaway agent loop gets 429s instead of hammering the database. `App::rate_limit(n)` limits the JSON API the same way; `mcp_rate_limit(None)` turns the MCP limit off. Counts are per process.

**The Supabase attack, replayed.** In 2025 a support ticket told an AI agent with full database access to copy integration tokens into the ticket thread, and it could ([write-up](https://generalanalysis.com/blog/supabase-mcp-blog)). The e2e test files the same kind of ticket and then plays a fully obedient support bot: it reads the ticket, then tries to read the secret, probe it through filters, open other tables, write the secret out, and reach the admin. Every step fails, because the bot's token is scoped to tickets and read-only and the secret is a private field. Prompt injection still fools models; on Rangoli it can't make one leak.

Checked by: an e2e test that speaks raw MCP on all three databases (tool lists per user, writes and History, `owned_by` holding for agents, validation errors returned to the agent), 3,000 rounds of random JSON-RPC and tool calls in the fuzzer, and a live run of the official MCP Python SDK (2.3.0) against the example blog.

## Databases

Postgres, MySQL and SQLite share one code path through sqlx's `Any` driver. Only the SQL spelling differs per dialect. CI runs the same end-to-end test on all three.

Field types: `i64`, `f64`, `bool`, `String` (`VARCHAR`, or `TEXT` with `#[field(text)]`), `DateTime`, `Date`, `Decimal<P>`, `Uuid`, `Json` and choice enums, each optionally wrapped in `Option` for a nullable column.

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

A stored value longer than the column's `max_length` is a compile error. `#[field(default = ...)]` takes a literal and is used for existing rows when `makemigrations` adds the column, as the initial value of new admin forms, and for fields an API client leaves out. `DateTime` is UTC, stored as Unix seconds so it behaves identically on every database, and serializes as ISO 8601. `Date` is a calendar date stored as days since 1970-01-01: `YYYY-MM-DD` in the API and OpenAPI (`format: date`), a native date picker and date-range filters in the admin, and `Date::today()` for existing rows when `makemigrations` adds a non-null one.

Money is `Decimal<P>`, with the number of places in the type: `price: Decimal<2>`. It is stored as an integer count of the smallest unit (cents), so ten `0.10`s sum to exactly `1.00` (they don't as floats; a unit test shows both), SQL equality and ordering are exact on every database, and mixing `Decimal<2>` with `Decimal<3>` doesn't compile. Input with more places than `P` is rejected rather than silently rounded, in the API (`"1.005"` → 400), the admin and forms. The API writes decimals as strings, like DRF, and reads strings or whole numbers. A fractional JSON number is refused, since it is already a float when parsed (`19.999999999999999` arrives as `20`), so clients send `"19.99"`. Range: ±92 quadrillion at 2 places.

`Uuid` is stored as lowercase text (`VARCHAR(36)`) on every database. `#[field(auto, unique)] public_id: Uuid` fills a time-ordered v7 on insert unless you set one, and shows read-only in the admin and the API (`format: uuid`). Adding a unique UUID column to a table that already has rows is one `makemigrations`: the migration adds it nullable, gives each existing row its own UUID, then makes it `NOT NULL UNIQUE`. Django needs [three hand-written migrations](https://docs.djangoproject.com/en/stable/howto/writing-migrations/#migrations-that-add-unique-fields) for that; the e2e test runs it on all three databases, and without that step Postgres refuses with `could not create unique index`. `#[field(auto_now_add)]` and `#[field(auto_now)]` fill `created_at`/`updated_at` style fields on save, and the admin keeps them read-only. Field attributes: `max_length`, `text`, `unique`, `index`, `choices`, `default = literal`, `password`, `fk = Model`, `auto` (UUIDs), `auto_now`, `auto_now_add`, and `cascade` (`ON DELETE CASCADE`; foreign keys protect referenced rows by default).

### Encrypted fields

`#[field(encrypted)] tax_id: String` keeps the value encrypted in the database (AES-256-GCM, a fresh nonce on every write), so a dump or a backup shows `enc:v1:...` instead of the data. Your code, the admin, the API and MCP see plain values; only the storage boundary changes. Set `RANGOLI_ENCRYPTION_KEY` to 64 hex characters (`openssl rand -hex 32`); an app with encrypted fields won't start without one. To rotate, move the old key to `RANGOLI_ENCRYPTION_OLD_KEYS` and set a new one: old values still read, and each row is re-encrypted on its next save. Rows written before a field was encrypted read as they are. Encrypted fields can't be searched or filtered, since the stored bytes change on every write; the API says so with a 400. Django needs a third-party package for this ([new-features #150](https://github.com/django/new-features/issues/150)).

## Migrations

```json
{
  "operations": [
    { "op": "add_column", "table": "blog_post", "column": { "name": "views", "type": "int" }, "default": 0 }
  ]
}
```

To deploy the binary alone, compile the migrations in: `App::new().embed_migrations(rangoli::migrations!())`. `migrate` then uses the embedded files plus any in `RANGOLI_MIGRATIONS` (a file on disk wins over an embedded one with the same name, so a fresh `makemigrations` works before the next build). The example blog's release binary, copied alone into an empty directory, migrates and serves; built without that line, the same test gets a 500 because its tables were never created. Cargo rebuilds when an embedded file changes but not when a new one appears, so `migrate` warns, like Django, when the models have changes that no migration in the binary or on disk covers.

Operations: `create_table`, `drop_table`, `add_column`, `drop_column`, `alter_column`, `add_index`, `drop_index`, and `sql` (the equivalent of Django's RunSQL). Each migration runs in its own transaction. MySQL commits DDL automatically, just as it does under Django. `makemigrations --check` and `migrate --check` exit non-zero, for CI. `makemigrations` refuses a type change that would silently reinterpret stored numbers, such as `Decimal<2>` to `Decimal<3>` (19.99 would read as 1.999), `i64` to `Decimal`, or `DateTime` to `Date`. It prints the `UPDATE` that converts the data, for a migration you write by hand.

## What's not done yet

Still missing, roughly in order:

1. **Translations for the admin's own pages** (apps translate theirs with `App::locales`), and routing to more than one primary database (a read replica is supported).
2. **WebAssembly,** explored later: the same validation rules running in the browser and on the server.

**Known limits:**

- Changing a foreign key on an existing column needs an `sql` operation on Postgres and MySQL.
- Vector search on MySQL and SQLite compares vectors in your app; it isn't indexed.
- The cache lives in each process's memory.

## Only what you use

Every part beyond the ORM, migrations, auth and the JSON API is a Cargo feature, all on by default:

| Feature | Adds |
|---|---|
| `admin` | the admin pages |
| `web` | templates, partials, static files |
| `realtime` | WebSockets and Server-Sent Events |
| `postgres`, `mysql`, `sqlite` | the database drivers |

```toml
rangoli = { version = "0.2", default-features = false, features = ["postgres"] }  # an API service
```

[`examples/api_only`](../examples/api_only/src/main.rs) built that way is a 7.1 MB binary from 196 crates; the full example blog is 13.1 MB from 249. Turning a feature off never changes your schema: the admin log and task tables are always part of it.

## Reliability

The framework must never be the reason a backend goes down. What that means in practice, and what checks it:

- **A panic in your handler costs one 500**, logged, and the server keeps serving (caught at the outermost layer).
- **Hostile input never produces a server error.** [`tests/robustness.rs`](../rangoli/tests/robustness.rs) is a deterministic fuzzer: SQL-injection strings, `%` and `_`, null bytes, `../`, `<script>`, emoji and right-to-left text, 10,000-character values, impossible dates, numbers too big for any column, and random JSON of every shape, thrown at the admin's lists, forms, bulk actions and paths, the JSON API, and login, plus 200 concurrent reads and writes. Any 5xx fails the build. It runs on SQLite, Postgres and MySQL on every push, and for 3,000 rounds (about 20,000 requests) in its own job.
- **Database outages are outages, not crashes.** Requests that need the database fail within `RANGOLI_DB_TIMEOUT` (5 s) with `503` and `Retry-After`; everything else keeps working; when the database comes back, so does the app, without a restart. [`scripts/outage_check.sh`](../scripts/outage_check.sh) proves it against real Postgres in CI: the database hangs (`docker pause`) and restarts under a running app.
- **`GET /-/health` for load balancers and Kubernetes.** It answers `200 ok` when the database responds, `200 warn` when due background jobs have waited over five minutes (workers down), and `503 down` when the database doesn't answer within 2 s. It reveals no versions, hosts or errors. The outage check above asserts all three states.
- **No request waits forever:** `RANGOLI_REQUEST_TIMEOUT` (30 s) answers with `503`. Bodies over 2 MB are refused with `413`.
- **No lock can poison itself:** the login lockout and the task registry recover from a panic elsewhere instead of failing every later request.

## Performance

`bench/` holds the same blog written in Django 6.1.2 (with DRF and gunicorn) and a script that loads both with identical data and measures them with [oha](https://github.com/hatoo/oha). On an Apple M1 Pro, SQLite, 64 concurrent connections, Django on 8 sync workers:

| | Rangoli | Django 6.1.2 |
|---|---|---|
| `/posts.json` (50 posts + author) | 14,119 req/s | 2,909 req/s |
| Homepage template (50 posts + tags) | 3,958 req/s | 1,384 req/s |
| JSON API list | 8,653 req/s | 1,119 req/s (DRF) |
| Admin changelist (logged in) | 2,757 req/s | 211 req/s |
| Startup to first response (median of 5) | 15 ms | 232 ms |
| Memory under load | 61 MB | 511 MB |

The first run had Rangoli *slower* than Django on three of four pages. Profiling showed every SQLite connection in the process waiting on SQLite's global statistics mutexes; Django never sees them because each gunicorn worker is its own process. Rangoli now switches memory statistics off at runtime, and SQLite apps should copy [`.cargo/config.toml`](../.cargo/config.toml), which also builds SQLite without its shared page cache. That took the homepage from 249 to about 3,900 req/s. Rerun with `python bench/run.py`; `python bench/experiments.py` reruns the behaviour comparisons (typos, branch migrations, N+1, security defaults, tasks).

## Development

```sh
cargo test                                                     # unit tests + end-to-end on SQLite
RANGOLI_TEST_DATABASE_URL=postgres://... cargo test --test e2e
RANGOLI_TEST_DATABASE_URL=mysql://...    cargo test --test e2e
```

## License

Dual-licensed under [MIT](../LICENSE-MIT) or [Apache-2.0](../LICENSE-APACHE), at your option.

# Roadmap: the gaps, the evidence, the proof

Rangoli is built by reading what Django developers keep asking for and shipping it, each item with a test that fails without it. This page is the working list. It was drawn up on 2026-10-14 from:

- **Django's open tickets** on Trac, ranked by people following each one (CC), and the [django/new-features](https://github.com/django/new-features/issues) board, ranked by reactions.
- **The open issues of the Django ecosystem**: DRF, django-ninja, Channels, Celery, django-rq, django-storages, django-filter, drf-spectacular, simple-history, Wagtail, django-health-check, import-export and allauth.
- **[django-bolt](https://github.com/dj-bolt/django-bolt)**, the Rust-powered API layer for Django. We read its issues and its published benchmarks.

## Decisions everything else follows

1. **Shared state lives in your database, not in a process.** Sessions, tasks, schedules, approvals, OAuth codes and webhooks are all stored there, and Postgres `LISTEN/NOTIFY` fans out realtime messages, so N processes behave like one with no Redis. django-bolt's MCP Tasks need `--processes 1` for the opposite reason (bolt#299).
2. **Fail closed, at startup.** A misspelled field doesn't compile. An impossible admin or API setting stops the app at startup. An unknown access rule can't exist, because access is a closed enum. django-bolt shipped custom guards that silently allowed everything, and a test client that skipped them (bolt#392).
3. **One policy, three doors.** The admin, the JSON API and MCP check the same access rules, `owned_by`, private fields and approvals. Any new permission feature plugs in there, never in one door only.
4. **Every claim has a test that fails when the code is removed.** Benchmarks state the machine, the command and the variance.
5. **No new dependency when the tree already has one.** For example, AES-GCM, HMAC and TLS all come from the `ring` and `rustls` already compiled in through sqlx.

## Done in this pass (with the issue each closes)

| Gap | Evidence | Proof in the repo |
|---|---|---|
| Sign in to MCP from claude.ai / Claude Desktop (OAuth 2.1 + PKCE) | MCP authorization spec; custom connectors need it | e2e: discovery, registration, consent, single-use codes, wrong-verifier rejection |
| Conditional fields in the admin | wagtail#2172 (29👍) | e2e on 3 databases + Playwright toggling in Chrome |
| `/-/health` that warns on job backlog | django-health-check #683, #730 | outage script: 200 → 503 during a DB hang → 200 |
| Typed errors and PATCH bodies in OpenAPI | drf-spectacular#101 (11👍), ninja#1583 | validated by openapi-spec-validator |
| Queue a task once; retry failed runs | celery#4400 (48👍), django-rq#465, #513 | 10 racing enqueues → 1 run |
| Faster than django-bolt on its own benchmark | bolt claims; bolt#283 | [bench/vs_bolt](../bench/vs_bolt/RESULTS.md): 1.3–3.1×, rerun weekly on GitHub runners |
| Trusted proxies, IPv6 /64 rate-limit buckets | bolt#301, #302 | unit tests: spoofed headers ignored, proxied clients split |
| Admin on 1M-row tables | Trac#8408, NF#31 | 93 ms → 7 ms per page; startup warning for unindexed ordering |
| Row-level permissions, one policy for admin, API and MCP | NF#12, NF#17 | e2e probes every door; mutation-checked |
| Tree queries (`WITH RECURSIVE`) | Trac#28919 | 10,000-node tree in one query on 3 databases |
| Full-text search | NF#107 | admin search on 1M rows: 250 ms → 7–22 ms |
| Zero-downtime indexes and unique changes | NF#124 | a write goes through during the build (asserted) |
| Foreign keys indexed, as Django does | (gap found while profiling) | author filter on 1M rows: 35 → 9 ms |
| JWT sign-in (HS256, RS256 + JWKS) | bolt#262, #264 | RFC 7515 vectors; alg-confusion refused |
| S3 uploads (presigned POST/GET) | storages#165, #1481, NF#89 | live against SeaweedFS in CI; size/type/key/expiry refused |
| Cache cleared on writes, across instances | NF#95 | 7 ms across instances via NOTIFY |
| CSV export and import in the admin | import-export #2097, #445 | round trip unchanged; formulas defused |
| Vector search, and semantic search for agents | NF#145 | recall@10 0.98 at 8 ms on pgvector; agents never see others' rows |
| `unique(a, b)` (unique_together) | Django staple | refused pair, accepted parts, on 3 databases |
| Embeddings kept current automatically | follows `App::embed` | text edits re-embed, other writes cost no call (counted) |
| Filter lookups in the API and MCP (`__gte`, `__in`, `__isnull`...) | django-filter#1134 (19👍) | e2e on 3 databases, every lookup typed by its field; private fields refused |
| Email and password reset | Django staple | Mailpit in CI; header injection refused |
| Admin permissions and groups, fieldsets, `list_editable`, Save as new, bulk "Edit selected" | Django admin parity | `tests/{admin_perms,list_editable,bulk_edit}.rs`: every row checked, all saved or none |
| Durable change hooks (`App::on_change`) | NF#62 signals that survive a crash | runs after commit, retried as a task |
| Realtime and cache across instances on MySQL and SQLite | Channels without Redis | `rangoli_event` table, no broker |
| Translations, read replicas, `?expand=`, row rules (`App::validate`) | Trac#4182 i18n habits, NF#56, DRF `depth`, `Model.clean()` | one test file each |
| Dry runs for every write (`?dry_run=1`, MCP `dry_run`) | agents need a "what would happen" | nothing saved, nothing queued (counted) |
| Agent guardrails: daily write budget, approval diff preview, "Pause all agents" | AI agents on production data | 12 racing writes → exactly 5 with a budget of 5; pause stops API and MCP writes, reads go on |
| MCP prompts, row resources and your own tools (`App::mcp_tool`) | MCP spec; custom actions beyond CRUD | custom tools hidden from read-only, approval and other-table tokens |
| Totals for agents and the API (`/aggregate`, `<table>_aggregate`) | "how many / how much" without paging | decimals exact; filters and row permissions apply |
| Ask your data in the admin (`App::llm`) | plain-English reporting | the model sees the schema, never rows; read-only, hidden fields refused |
| AI-filled fields (`App::ai_field`) and one-line providers (`rangoli::ai`) | classification, summaries | refreshed only when sources change; answers outside the choices never stored |
| Custom commands (`App::command`) | `manage.py` commands | `helpdesk close_stale` |
| Query inspector: N+1s for staff and their agents | silk#80, debug-toolbar#2048 | an N+1 route flagged, a select_related one not |
| Webhooks, 2FA, inlines, QUERY, encrypted fields, undo, cursor paging, upsert, aggregates, approvals, scoped tokens, CORS, rate limits | NF#8 (73), NF#185 (41), Trac#9025, NF#12, Trac#8408, Trac#28919-adjacent, NF#204 | see README, each with its numbers |

## Next, in order

| # | Build | Why |
|---|---|---|
| 1 | Rerun the Django and django-bolt benchmarks on a quiet machine, Django with the same `created_at` index | the last Django rerun ran under load 20–43 and was thrown away |
| 2 | Admin `date_hierarchy` | Django admin parity; the date filters cover most of it today |
| 3 | AI-filled fields: remember refused answers per row in the database, not per process | today a restart retries them once |

## Decided against, with reasons

- **Composite primary keys (Trac#373).** Every admin URL, API path and History entry is keyed by one id; changing that touches everything for a rare need. Most real cases want composite *uniqueness*, which is `#[model(unique(a, b))]`.
- **A Postgres schema per tenant (Trac#6148).** Each migration would run once per customer. One table with a `tenant_id`, row-level permissions and `unique(tenant_id, ...)` does the job; see [the guide](howto/multi-tenant.md).

Already covered, so not on this list:

- **Django items Rangoli already answers:** NF#6 (a standalone ORM, 128), NF#23 (types, 119), NF#98 (header-based CSRF: Rangoli checks `Sec-Fetch-Site`/`Origin`, no tokens), NF#61 (settings from the environment) and NF#114 (static serving).
- **Async (NF#7):** Rangoli is async from the socket to the driver, with no `sync_to_async` bridge. That bridge cost django-bolt 23-45% of async Postgres throughput (bolt#283).

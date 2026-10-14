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
| Webhooks, 2FA, inlines, QUERY, encrypted fields, undo, cursor paging, upsert, aggregates, approvals, scoped tokens, CORS, rate limits | NF#8 (73), NF#185 (41), Trac#9025, NF#12, Trac#8408, Trac#28919-adjacent, NF#204 | see README, each with its numbers |

## Next, in order

Each line has the demand signal, and how we'll know it works.

| # | Build | Why (demand) | Proof |
|---|---|---|---|
| 1 | **Benchmarks against django-bolt's own harness** | bolt claims 311k req/s for hello-world and 2,881 / 301 req/s for its Postgres `/db` and `/articles` endpoints | same tool (bombardier, C=100), endpoints, row counts, p99, and machine stated, in CI as a regression gate |
| 2 | **Trusted proxies and IPv6 /64 buckets for rate limits and lockouts** | bolt#301, #302 (spoofed `X-Forwarded-For`, every user in one bucket) | spoofed header ignored, a proxied client gets its own bucket, rotating within one /64 is still one bucket |
| 3 | **Keyset pagination in the admin** | NF#31 (21), Trac#8408 (open since 2008) | the changelist on 1M rows renders page N with no `COUNT(*)` scan, timed |
| 4 | **Row-level permissions, one policy for admin, API and MCP** | NF#12 (40), NF#17 (24), django-guardian | a property test shows no door returns a row outside the policy |
| 5 | **CTEs and recursive queries** | Trac#28919 (33 CC, open since 2017) | a 10k-node tree in one `WITH RECURSIVE` query; a wrong column doesn't compile |
| 6 | **Full-text search** (Postgres tsvector, SQLite FTS5, MySQL FULLTEXT) | NF#107 (18) | the same API on all three databases, with ranking order tested |
| 7 | **Zero-downtime migrations** (`CREATE INDEX CONCURRENTLY`, a linter for unsafe operations) | NF#124 (28) | add an index to a 1M-row table under load with 0 failed requests |
| 8 | **File uploads to S3-compatible storage** (presigned, size and type limits) | storages#165 (26👍), #1481, NF#89 | a large upload never passes through the app; an expired URL gives 403 |
| 9 | **JWT / JWKS sign-in** for apps using Clerk, Auth0 or Supabase Auth | bolt#262, #264 | real RS256 tokens verified; key rotation by `kid` |
| 10 | **Composite primary and foreign keys** | Trac#373 (41 CC; FKs still missing in Django: Trac#35956) | `(tenant_id, id)` keys through migrations, joins and admin URLs |
| 11 | **Caching invalidated across instances** | NF#95 (19) | a save on one instance clears another's cache within 100 ms |
| 12 | **Vector search** (pgvector) | NF#145 | an HNSW index by migration; recall@10 ≥ 0.95 against exact search |
| 13 | **Postgres schemas / multi-tenancy** | Trac#6148 (22 CC, open since 2007) | a cross-tenant read returns nothing through every door |

Already covered, so not on this list:

- **Django items Rangoli already answers:** NF#6 (a standalone ORM, 128), NF#23 (types, 119), NF#98 (header-based CSRF: Rangoli checks `Sec-Fetch-Site`/`Origin`, no tokens), NF#61 (settings from the environment) and NF#114 (static serving).
- **Async (NF#7):** Rangoli is async from the socket to the driver, with no `sync_to_async` bridge. That bridge cost django-bolt 23-45% of async Postgres throughput (bolt#283).

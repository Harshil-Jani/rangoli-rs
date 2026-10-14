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
| Webhooks, 2FA, inlines, QUERY, encrypted fields, undo, cursor paging, upsert, aggregates, approvals, scoped tokens, CORS, rate limits | NF#8 (73), NF#185 (41), Trac#9025, NF#12, Trac#8408, Trac#28919-adjacent, NF#204 | see README, each with its numbers |

## Next, in order

| # | Build | Why (demand) | Proof |
|---|---|---|---|
| 1 | **File uploads to S3-compatible storage** (presigned, size and type limits) | storages#165 (26👍), #1481, NF#89 | a large upload never passes through the app; an expired URL gives 403 |
| 2 | **Caching invalidated across instances** | NF#95 (19) | a save on one instance clears another's cache within 100 ms |
| 3 | **Composite primary and foreign keys** | Trac#373 (41 CC); FKs still missing in Django (Trac#35956) | `(tenant_id, id)` keys through migrations, joins and admin URLs |
| 4 | **Vector search** (pgvector) | NF#145 | an HNSW index by migration; recall@10 ≥ 0.95 against exact search |
| 5 | **Postgres schemas / multi-tenancy** | Trac#6148 (22 CC, open since 2007) | a cross-tenant read returns nothing through every door |

Already covered, so not on this list:

- **Django items Rangoli already answers:** NF#6 (a standalone ORM, 128), NF#23 (types, 119), NF#98 (header-based CSRF: Rangoli checks `Sec-Fetch-Site`/`Origin`, no tokens), NF#61 (settings from the environment) and NF#114 (static serving).
- **Async (NF#7):** Rangoli is async from the socket to the driver, with no `sync_to_async` bridge. That bridge cost django-bolt 23-45% of async Postgres throughput (bolt#283).

# Find out why a page is slow (or ask Claude to)

Run with `RANGOLI_DEBUG=1` (or add `App::inspect_queries()` on staging). Rangoli records each request's SQL with timings, keeping the last 200 requests in memory.

- **In the admin:** **Requests** in the top bar lists recent requests with their time, query count and time spent in SQL. A request that ran the same statement 5 or more times is marked **repeated**: one query per row in a loop, the N+1 problem. Fix it with `select_related` or `prefetch`.
- **For AI agents:** a staff agent gets an `inspect_requests` tool. Ask *"why is /api/orders slow?"* and it sees, for example, `SELECT ... FROM customer WHERE id = $1` run 50 times. Agents limited to some tables don't get the tool, because the log covers every table.

Statements are captured where Rangoli runs SQL, so the ORM, transactions, the admin, the API and MCP are all included. [`tests/inspect.rs`](../../rangoli/tests/inspect.rs) checks that an N+1 is flagged and a `select_related` version isn't.

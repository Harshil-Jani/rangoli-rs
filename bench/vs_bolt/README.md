# Rangoli vs django-bolt

[django-bolt](https://github.com/dj-bolt/django-bolt) runs Django behind a Rust HTTP server. Its authors publish a cross-framework benchmark, [python-api-frameworks-benchmark](https://github.com/FarhanAliRaza/python-api-frameworks-benchmark). It has seven endpoints over PostgreSQL:

- two JSON payloads;
- ten rows;
- a paginated article list with authors and tags;
- an article with its comments;
- the same reads behind JWT-cookie auth.

This directory is those seven endpoints written in Rangoli ([`src/main.rs`](src/main.rs)). We ran it against bolt itself, on the same machine and the same database, using the harness's own seed data and the load settings from its `bench.py`. Results are in [RESULTS.md](RESULTS.md).

**Same work on both sides:**

- Same paths and query parameters.
- Same JSON fields. Datetimes differ in precision only: Rangoli stores seconds, Django microseconds.
- Same rows in the same order. `compare.py` checks this before every run.
- Same query plan:
  - **Articles page:** a count, the page with each author joined in, then the page's tags.
  - **Article detail:** the article with its author, its tags and its comments.
  - **Auth endpoints:** the JWT is verified and the user row loaded, then the response is built.
- One process each, eight database connections each, access logs off.

## Reproduce

```sh
# 1. The harness, its database and bolt (needs uv and Docker)
git clone https://github.com/FarhanAliRaza/python-api-frameworks-benchmark pafb && cd pafb && uv sync
docker run -d --name pafb-pg -e POSTGRES_USER=benchmark -e POSTGRES_PASSWORD=benchmark -e POSTGRES_DB=benchmark -p 55433:5432 postgres:17-alpine
export POSTGRES_HOST=127.0.0.1 POSTGRES_PORT=55433
DJANGO_SETTINGS_MODULE=django_project.settings_migrate .venv/bin/python manage.py migrate
.venv/bin/python scripts/seed_db.py
DJANGO_SETTINGS_MODULE=django_project.settings_bolt DJANGO_BOLT_WORKERS=1 DJANGO_BOLT_EXECUTOR_THREADS=8 \
  DJANGO_BOLT_ORM_THREADS=8 .venv/bin/python manage.py runbolt --host 127.0.0.1 --port 9001 --processes 1 &

# 2. Rangoli on the same database, with the same rows (from this repo)
cargo build --release -p vs_bolt && cd bench/vs_bolt
export RANGOLI_DATABASE_URL=postgres://benchmark:benchmark@127.0.0.1:55433/benchmark
../../target/release/vs_bolt migrate
docker exec -i pafb-pg psql -U benchmark < load.sql
RANGOLI_PASSWORD=benchpass123 ../../target/release/vs_bolt createsuperuser benchuser
RANGOLI_BIND=127.0.0.1:9002 RANGOLI_WORKERS=0 RANGOLI_DB_POOL=8 ../../target/release/vs_bolt runserver &

# 3. Check both answer the same, then measure (needs bombardier: go install github.com/codesenberg/bombardier@latest)
python3 compare.py && python3 run.py
```

**Caveats:**

- **Absolute numbers depend on the machine.** Postgres in Docker on macOS adds a virtual-machine network hop to every query, for both servers.
- **Run-to-run noise is real.** Each endpoint is measured three times with the servers interleaved, and the median is reported. Gaps under 10% should be read as a tie.
- **The `/auth/me` response isn't stored in full.** Rangoli's built-in user has no first name, last name or email, so the bench fills in the same values bolt returns rather than adding a second query.

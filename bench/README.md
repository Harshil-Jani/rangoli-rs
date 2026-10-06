# Rangoli vs Django 6.1.2

The example blog (`examples/blog`) written again in Django 6.1.2 (`django_blog/`), loaded with the same data, measured the same way. Results feed the report site (`site/build.py`).

```sh
# from the repository root
cargo build --release -p blog --bins --examples

cd bench
uv venv .venv
uv pip install --python .venv/bin/python "django==6.1.2" djangorestframework gunicorn

# fresh databases with the same superuser (admin / bench-pass-123)
R=$PWD/results
(cd ../examples/blog && RANGOLI_DATABASE_URL="sqlite://$R/rangoli.sqlite3?mode=rwc" ../../target/release/blog migrate \
  && RANGOLI_DATABASE_URL="sqlite://$R/rangoli.sqlite3" RANGOLI_PASSWORD=bench-pass-123 ../../target/release/blog createsuperuser admin)
(cd django_blog && DJANGO_DB=$R/django.sqlite3 ../.venv/bin/python manage.py migrate \
  && DJANGO_DB=$R/django.sqlite3 DJANGO_SUPERUSER_PASSWORD=bench-pass-123 ../.venv/bin/python manage.py createsuperuser --noinput --username admin --email a@example.com)

python seed.py results/rangoli.sqlite3 results/django.sqlite3
python run.py           # -> results/bench.json
python experiments.py   # -> results/experiments.json
```

`run.py` warms each page up, then runs oha for 15 seconds at 64 connections. Django uses gunicorn with one sync worker per core and `DEBUG` off. Startup is the median of five launches; memory is the resident size of all server processes.

## The SQLite finding

The first run had Rangoli slower than Django on three of four pages, and slower as concurrency rose. `sample` showed connections waiting on SQLite's global mutexes:

- memory statistics (`SQLITE_DEFAULT_MEMSTATUS`) take a process-wide mutex on every allocation;
- `libsqlite3-sys` compiles with `SQLITE_ENABLE_MEMORY_MANAGEMENT`, which puts every connection behind one shared page cache;
- page-cache overflow statistics take another global mutex.

Django never sees these because gunicorn workers are separate processes. Rangoli switches memory statistics off at runtime, and `.cargo/config.toml` rebuilds SQLite without the shared page cache and the overflow statistics. Homepage throughput went from 249 to about 3,900 req/s.

`typo_rangoli/` is a deliberately broken program used by `experiments.py`: it must fail to compile.

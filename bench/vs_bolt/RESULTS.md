# Rangoli vs django-bolt, on django-bolt's own benchmark

Machine: Apple M1 Pro, 8 cores, Darwin. PostgreSQL 17 in Docker, seeded by the harness's own `seed_db.py`.
Load: bombardier -c 100 -d 10s, 500-request warmup, 3 rounds with the servers interleaved; median round shown.
Both servers: one process, 8 database connections, access logs off.

| Endpoint | django-bolt req/s | Rangoli req/s | Rangoli ÷ bolt | bolt p99 | Rangoli p99 | non-2xx (bolt / Rangoli) |
|---|---:|---:|---:|---:|---:|---:|
| `/json-1k` | 74,346 | 125,641 | 1.7× | 1.8 ms | 2.2 ms | 0 / 0 |
| `/json-10k` | 46,266 | 93,768 | 2.0× | 3.2 ms | 2.8 ms | 0 / 0 |
| `/db` | 3,162 | 5,467 | 1.7× | 62.6 ms | 35.9 ms | 0 / 0 |
| `/articles?page=1&page_size=20` | 383 | 1,193 | 3.1× | 340.3 ms | 158.4 ms | 0 / 0 |
| `/articles/1` | 688 | 1,474 | 2.1× | 239.3 ms | 144.9 ms | 0 / 0 |
| `/auth/me` | 4,611 | 5,782 | 1.3× | 48.7 ms | 29.5 ms | 0 / 0 |
| `/auth/articles?page=1&page_size=20` | 356 | 1,015 | 2.9× | 335.5 ms | 177.8 ms | 0 / 0 |

Memory after the run (RSS): django-bolt 129 MB, Rangoli 30 MB.

Run on 2026-10-14 with django-bolt 0.11.0 and Django 6.1 (the versions the harness's `uv.lock` pins) and Rangoli at this commit.

#!/usr/bin/env bash
# Build both apps' databases from scratch with identical data. Run from anywhere.
#   bench/setup.sh            (expects target/release/blog and bench/.venv)
set -euo pipefail
cd "$(dirname "$0")"
R="$PWD/results"
mkdir -p "$R"
rm -f "$R/rangoli.sqlite3" "$R/rangoli.sqlite3-wal" "$R/rangoli.sqlite3-shm" "$R/django.sqlite3"
BIN="$PWD/../target/release/blog"
(cd ../examples/blog \
  && RANGOLI_DATABASE_URL="sqlite://$R/rangoli.sqlite3?mode=rwc" "$BIN" migrate >/dev/null \
  && RANGOLI_DATABASE_URL="sqlite://$R/rangoli.sqlite3" RANGOLI_PASSWORD=bench-pass-123 "$BIN" createsuperuser admin >/dev/null)
(cd django_blog \
  && DJANGO_DB="$R/django.sqlite3" ../.venv/bin/python manage.py migrate -v0 \
  && DJANGO_DB="$R/django.sqlite3" DJANGO_SUPERUSER_PASSWORD=bench-pass-123 ../.venv/bin/python manage.py createsuperuser --noinput --username admin --email a@example.com >/dev/null)
python3 seed.py "$R/rangoli.sqlite3" "$R/django.sqlite3"

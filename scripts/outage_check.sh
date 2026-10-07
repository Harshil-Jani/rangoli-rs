#!/usr/bin/env bash
# The database disappears, hangs, and restarts under a running Rangoli app.
# The app must answer 503 quickly while the database is gone, keep serving pages
# that don't need it, and recover on its own when the database is back.
#
#   scripts/outage_check.sh        # needs docker and a built example: cargo build -p blog
set -euo pipefail
cd "$(dirname "$0")/../examples/blog"
BIN=../../target/debug/blog
PG=rangoli-outage-pg
URL=postgres://postgres:pw@127.0.0.1:55499/rangoli
export RANGOLI_DATABASE_URL=$URL RANGOLI_BIND=127.0.0.1:8099 RANGOLI_DB_TIMEOUT=2 RANGOLI_WORKERS=1

cleanup() { kill "${APP:-0}" 2>/dev/null || true; docker rm -f $PG >/dev/null 2>&1 || true; }
trap cleanup EXIT

code() { curl -s -o /dev/null -w '%{http_code}' --max-time "${2:-10}" "http://127.0.0.1:8099$1"; }
expect() { # expect <path> <status> <what>
  local got; got=$(code "$1")
  if [ "$got" != "$2" ]; then echo "FAIL: $3: $1 answered $got, expected $2"; exit 1; fi
  echo "ok:   $3 ($1 -> $got)"
}
wait_for() { for _ in $(seq 1 60); do [ "$(code "$1" 3)" = "$2" ] && return 0; sleep 1; done; echo "FAIL: $1 never answered $2"; exit 1; }

docker run -d --rm --name $PG -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=rangoli -p 55499:5432 postgres:17-alpine >/dev/null
for _ in $(seq 1 60); do docker exec $PG pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
sleep 2
$BIN migrate >/dev/null
$BIN runserver >/tmp/rangoli-outage.log 2>&1 & APP=$!
wait_for /api/blog_post/ 200
expect /api/blog_post/ 200 "database up"

echo "-- database hangs (docker pause)"
docker pause $PG >/dev/null
start=$(date +%s)
expect /api/blog_post/ 503 "hung database answers 503"
took=$(( $(date +%s) - start ))
[ "$took" -le 8 ] || { echo "FAIL: took ${took}s to give up"; exit 1; }
echo "ok:   gave up after ${took}s, not hanging"
expect /static/site.css 200 "pages without the database keep working"
kill -0 $APP || { echo "FAIL: the app died"; exit 1; }
docker unpause $PG >/dev/null
wait_for /api/blog_post/ 200
expect /api/blog_post/ 200 "recovered after the hang"

echo "-- database restarts (all connections dropped)"
docker restart $PG >/dev/null
for _ in $(seq 1 60); do docker exec $PG pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
wait_for /api/blog_post/ 200
expect /api/blog_post/ 200 "recovered after a restart"
expect "/admin/login" 200 "admin works after a restart"

kill -0 $APP || { echo "FAIL: the app died"; exit 1; }
echo "PASS: no crash, fast 503s during the outage, automatic recovery"

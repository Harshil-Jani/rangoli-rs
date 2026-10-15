#!/usr/bin/env bash
# Everything CI checks, locally: format, lints (including minimal feature sets), and the full
# test suite on SQLite plus any databases you point it at.
#
#   scripts/check.sh
#   PG=postgres://postgres:pw@127.0.0.1:5432/rangoli MY=mysql://root:pw@127.0.0.1:3306/rangoli scripts/check.sh
#
# ITERS sets the robustness fuzzer's rounds (default 300).
set -uo pipefail
cd "$(dirname "$0")/.."
fail() { echo "FAILED: $1"; exit 1; }
cargo fmt --check || fail "cargo fmt (run cargo fmt)"
cargo clippy -q --workspace --all-targets -- -D warnings || fail "clippy"
for f in postgres mysql sqlite sqlite,admin sqlite,web postgres,realtime; do
  cargo clippy -q -p rangoli --no-default-features --features "$f" --all-targets -- -D warnings || fail "clippy with only $f"
done
cargo test -q -p rangoli --no-default-features --features sqlite >/dev/null 2>&1 || fail "tests with only the sqlite feature"
for u in "" ${PG:-} ${MY:-}; do
  name=${u%%:*}; name=${name:-sqlite}
  out=$(RANGOLI_TEST_DATABASE_URL=$u cargo test -q --workspace 2>&1) || { echo "$out" | tail -40; fail "tests on $name"; }
  echo "$name: $(echo "$out" | grep -E '^test result' | awk '{p+=$4; f+=$6} END {print p" passed, "f" failed"}')"
  ROBUSTNESS_ITERS=${ITERS:-300} RANGOLI_TEST_DATABASE_URL=$u cargo test -q -p rangoli --test robustness >/dev/null 2>&1 || fail "robustness fuzz on $name"
done
echo "all checks passed"

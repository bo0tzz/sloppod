#!/usr/bin/env bash
# Runs a command against a throwaway Postgres, with DATABASE_URL pointing at it.
# The database tests (sqlx::test) create a fresh database per test inside it.
set -euo pipefail

dir=$(mktemp -d)
trap 'pg_ctl -D "$dir/data" -m immediate stop >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

initdb -D "$dir/data" -U postgres --auth=trust --no-sync >/dev/null
pg_ctl -D "$dir/data" -l "$dir/log" -w \
  -o "-k $dir -c listen_addresses='' -c fsync=off -c max_connections=200" start >/dev/null

export DATABASE_URL="postgres://postgres@localhost/postgres?host=$dir"
"$@"

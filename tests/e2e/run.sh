#!/usr/bin/env bash
# One-command stack check for a cloud/CI box with no Discord credentials: local Postgres, workspace
# tests against it, dashboard + mock Discord, headless-browser sweep of every page.
# Needs: postgresql-16, cargo, trunk (+ wasm32 target), node with playwright. Run from the repo root.
set -euo pipefail
cd "$(dirname "$0")/../.."
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
export DISCORD_API_BASE=http://127.0.0.1:9100 DASHBOARD_URL=http://127.0.0.1:8080
export DISCORD_CLIENT_ID=1 DISCORD_CLIENT_SECRET=x DISCORD_TOKEN=mock BIND=127.0.0.1:8080
export DASHBOARD_DIST=crates/dashboard/ui/dist

pg_lsclusters | grep -q online || { pg_ctlcluster 16 main start; sleep 3; }
su postgres -c "psql -qc \"alter user postgres password 'postgres'\""

cargo test --workspace
[ -f $DASHBOARD_DIST/index.html ] || (cd crates/dashboard/ui && trunk build --release)

python3 tests/e2e/mock_discord.py & mock=$!
cargo run -q --release -p tommybot-dashboard & dash=$!
trap 'kill $mock $dash 2>/dev/null' EXIT
for _ in $(seq 600); do curl -s $DASHBOARD_URL/api/me -o /dev/null -w "%{http_code}" | grep -q 401 && break; sleep 1; done

# The mock's owner is the guild owner, so the dashboard grants owner level without any seed data.
node tests/e2e/dashboard.js

#!/usr/bin/env bash
# Runs the real bot against the test server and fires the attack scenarios in live.py at it.
# Needs DISCORD_TOKEN and TEST_GUILD_ID (ATTACKER_TOKEN for nuke), postgresql-16, cargo.
# Usage: tests/e2e/live.sh [spam|nuke|all]. Run from anywhere; never prints the tokens.
set -euo pipefail
cd "$(dirname "$0")/../.."
export DATABASE_URL=${DATABASE_URL:-postgres://postgres:postgres@localhost:5432/postgres}

pg_lsclusters | grep -q online || { pg_ctlcluster 16 main start; sleep 3; }
su postgres -c "psql -qc \"alter user postgres password 'postgres'\""

cargo build -q --release -p tommybot
./target/release/tommybot > target/live-bot.log 2>&1 & bot=$!
trap 'kill $bot 2>/dev/null' EXIT
sleep 10  # gateway connect and guild cache
kill -0 $bot || { tail target/live-bot.log; exit 1; }
python3 tests/e2e/live.py "${@:-all}"

#!/usr/bin/env bash
# Runs the real bot against the test server and fires the attack scenarios in live.py at it.
# Needs DISCORD_TOKEN (a test bot, never the hosted bot: both would react; PITFALLS.md 8) and TEST_GUILD_ID (ATTACKER_TOKEN for nuke), postgresql-16, cargo.
# Usage: tests/e2e/live.sh [spam|lock|mention_lock|nuke|panic|verify_delete|dm_timing|purge|emojis|edit|restart|all]. Run from anywhere; never prints the tokens.
set -euo pipefail
cd "$(dirname "$0")/../.."
export DATABASE_URL=${DATABASE_URL:-postgres://postgres:postgres@localhost:5432/postgres}

pg_lsclusters | grep -q online || { pg_ctlcluster 16 main start; sleep 3; }
su postgres -c "psql -qc \"alter user postgres password 'postgres'\""

cargo build -q --release -p tommybot
# The restart scenario kills and starts the bot itself, so the pid lives in a file.
export BOT_PID_FILE=target/live-bot.pid
RUST_LOG=${RUST_LOG:-info} ./target/release/tommybot >> target/live-bot.log 2>&1 & echo $! > $BOT_PID_FILE
trap 'kill $(cat $BOT_PID_FILE) 2>/dev/null' EXIT
sleep 10  # gateway connect and guild cache
kill -0 $(cat $BOT_PID_FILE) || { tail target/live-bot.log; exit 1; }
python3 tests/e2e/live.py "${@:-all}"

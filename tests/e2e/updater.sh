#!/usr/bin/env bash
# Runs the real updater image against a throwaway compose project and a local registry: one good
# update, then one whose dashboard crashes and must be rolled back. Needs Docker. Run from the repo root.
set -euo pipefail
reg=localhost:5555
dir=$(mktemp -d)
cp crates/core/migrations/1700_update_runs.sql "$dir/"
docker build -q --target updater -t tommybot-updater:test . > /dev/null
docker run -d --rm --name updtest-registry -p 5555:5000 registry:2 > /dev/null
dc() { docker compose --project-directory "$dir" "$@"; }
trap 'dc down -v -t 1 > /dev/null 2>&1; docker rm -f updtest-registry > /dev/null; rm -rf "$dir"' EXIT

# push <service> <sha> <cmd>: publishes a tiny image standing in for a TommyBot build. It is tagged
# under 127.0.0.1 so the local build does not take the tag of the image compose runs, as CI never would.
push() {
  printf 'FROM busybox:1.36\nLABEL org.opencontainers.image.revision=%s\nCMD ["sh", "-c", "%s"]\n' "$2" "$3" \
    | docker build -q -t "127.0.0.1:5555/$1:latest" - > /dev/null
  docker push -q "127.0.0.1:5555/$1:latest" > /dev/null
}
serve='mkdir -p /w && echo ok > /w/index.html && exec httpd -f -p 8080 -h /w'
sha() { printf "$1%.0s" $(seq 40); }

cat > "$dir/docker-compose.yml" <<YML
name: updtest
services:
  postgres:
    image: postgres:16-alpine
    environment: { POSTGRES_PASSWORD: x }
    healthcheck: { test: ["CMD", "pg_isready", "-U", "postgres"], interval: 1s, retries: 30 }
  bot: { image: "$reg/bot:latest", restart: unless-stopped }
  dashboard: { image: "$reg/dashboard:latest", restart: unless-stopped }
  updater:
    image: tommybot-updater:test
    depends_on: { postgres: { condition: service_healthy } }
    environment: { DATABASE_URL: "postgres://postgres:x@postgres:5432/postgres" }
    volumes: ["/var/run/docker.sock:/var/run/docker.sock", ".:/project:ro"]
YML

sql() { dc exec -T postgres psql -U postgres -Atqc "$1"; }
label() { docker inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$(dc ps -q dashboard)"; }
# update: queues a run and prints "status to_sha steps" once the updater has finished it.
update() {
  sql "insert into update_runs (requested_by) values (1)"
  for _ in $(seq 120); do
    r=$(sql "select status, to_sha, cardinality(step_times) from update_runs order by id desc limit 1")
    case $r in queued* | running*) sleep 1 ;; *) echo "$r"; return ;; esac
  done
  echo timeout
}
fail() { echo "FAIL: $*"; dc logs updater | tail -20; exit 1; }

push bot "$(sha a)" 'exec sleep 3600'
push dashboard "$(sha a)" "$serve"
dc up -d --wait postgres bot dashboard > /dev/null 2>&1
dc exec -T postgres psql -U postgres -q < "$dir/1700_update_runs.sql"
dc up -d updater > /dev/null 2>&1
for _ in $(seq 30); do [ -n "$(sql 'select at from updater_seen')" ] && break; sleep 1; done
[ -n "$(sql 'select at from updater_seen')" ] || fail "no heartbeat"

push bot "$(sha b)" 'exec sleep 3600'
push dashboard "$(sha b)" "$serve"
r=$(update)
[ "$r" = "done|$(sha b)|5" ] || fail "good update: $r"
[ "$(label)" = "$(sha b)" ] || fail "dashboard not on the new image"
echo "ok: good update"

push dashboard "$(sha c)" 'exit 1'
r=$(update)
case $r in rolled_back*) ;; *) fail "bad update: $r" ;; esac
[ "$(label)" = "$(sha b)" ] || fail "dashboard not rolled back"
[ "$(docker inspect -f '{{.State.Running}}' "$(dc ps -q dashboard)")" = true ] || fail "dashboard down after rollback"
echo "ok: crashing update rolled back"

sql "insert into update_runs (requested_by) values (1), (1)" 2> /dev/null && fail "two runs queued at once"
echo PASS

#!/usr/bin/env bash
# Restores the newest dump into a throwaway postgres:16 and checks the key tables. Exit 1 on failure.
set -euo pipefail
dir=${BACKUP_DIR:-$HOME/tommybot-backups}
file=$(ls -1 "$dir"/*.sql.gz 2>/dev/null | sort | tail -n1 || true)
[ -n "$file" ] || { echo "no dump in $dir" >&2; exit 1; }
name=tommybot-restore-test
docker rm -f $name > /dev/null 2>&1 || true
trap 'docker rm -f $name > /dev/null 2>&1 || true' EXIT
docker run -d --name $name -e POSTGRES_USER=tommybot -e POSTGRES_PASSWORD=test -e POSTGRES_DB=tommybot postgres:16 > /dev/null
# The image restarts postgres once after init, so wait for two queries in a row to succeed.
ok=0
for _ in $(seq 60); do
  if docker exec $name psql -U tommybot -qc 'select 1' > /dev/null 2>&1; then ok=$((ok + 1)); else ok=0; fi
  [ $ok -ge 2 ] && break
  sleep 1
done
[ $ok -ge 2 ] || { echo "postgres did not start" >&2; exit 1; }
echo "restoring $file"
gunzip -c "$file" | docker exec -i $name psql -U tommybot -q -v ON_ERROR_STOP=1 tommybot > /dev/null
for t in guild_config cases; do
  n=$(docker exec $name psql -U tommybot -tA -v ON_ERROR_STOP=1 tommybot -c "select count(*) from $t") \
    || { echo "FAIL: table $t missing" >&2; exit 1; }
  echo "$t: $n rows"
done
echo OK

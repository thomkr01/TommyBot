#!/bin/sh
# Runs in the updater container. Waits for an update request from the dashboard (a queued row in
# update_runs), pulls the newest bot and dashboard images, recreates both, checks they stay up, and
# rolls back to the previous images if anything fails. Progress goes to the same row for the dashboard.
set -u
DC="docker compose --project-directory /project -f /project/docker-compose.yml"
OLD=/tmp/old-images

q() { psql "$DATABASE_URL" -Atq -v ON_ERROR_STOP=1 "$@"; }

# Starts step $1, recording how long the previous one took.
step() {
  q -v id="$RUN" -v n="$1" <<'SQL'
update update_runs set step = :n, step_started_at = now(),
  step_times = case when :n = 0 then '{}' else step_times || extract(epoch from now() - step_started_at)::int end
where id = :id;
SQL
}

# Ends the run with status $1 and message $2 (empty for none).
finish() {
  q -v id="$RUN" -v st="$1" -v err="$2" <<'SQL'
update update_runs set status = :'st', error = nullif(:'err', ''), finished_at = now(),
  step_times = case when :'st' = 'done' then step_times || extract(epoch from now() - step_started_at)::int else step_times end
where id = :id;
SQL
}

# Puts the images from before the update back under their tags and restarts on them. The old version
# is checked like the new one: it can crash too, for example on a migration it does not know.
rollback() {
  while read -r svc ref; do docker tag "${ref%:*}:rollback" "$ref"; done < "$OLD"
  if ! $DC up -d --no-deps --no-build bot dashboard > /dev/null 2>&1; then
    finish failed "$1 Rolling back failed too: run 'docker compose up -d' on the server."
  elif settled; then
    finish rolled_back "$1 The previous version is running again."
  else
    finish failed "$1 The previous version does not start either: check 'docker compose logs bot dashboard' on the server."
  fi
}

# Whether bot and dashboard are running, have not restarted, and the dashboard answers.
healthy() {
  for svc in bot dashboard; do
    c=$($DC ps -q "$svc")
    [ "$(docker inspect -f '{{.State.Running}} {{.RestartCount}}' "$c" 2> /dev/null)" = "true 0" ] || return 1
  done
  wget -q -T 5 -O /dev/null http://dashboard:8080/
}

# Up and answering within a minute, then still up 15 seconds later (catches crash loops).
settled() {
  i=0
  until healthy; do
    i=$((i + 1))
    [ "$i" -lt 30 ] || return 1
    sleep 2
  done
  sleep 15
  healthy
}

run() {
  step 0
  : > "$OLD"
  for svc in bot dashboard; do
    c=$($DC ps -aq "$svc")
    [ -n "$c" ] || { finish failed "The $svc container was not found. Start TommyBot with docker compose first."; return; }
    ref=$(docker inspect -f '{{.Config.Image}}' "$c")
    # A named copy of the running image, since the pull takes its tag away (and may delete it).
    docker tag "$(docker inspect -f '{{.Image}}' "$c")" "${ref%:*}:rollback" || { finish failed "Could not save the $svc image for a rollback."; return; }
    echo "$svc $ref" >> "$OLD"
  done

  step 1
  if ! out=$(timeout 900 $DC pull -q bot dashboard 2>&1); then
    finish failed "Download failed, nothing was changed: $(echo "$out" | tail -c 400)"
    return
  fi
  ref=$(awk '$1 == "dashboard" { print $2 }' "$OLD")
  # psql only fills :'vars' in stdin, never in -c.
  q -v id="$RUN" -v sha="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$ref" 2> /dev/null)" <<'SQL'
update update_runs set to_sha = nullif(:'sha', '') where id = :id;
SQL

  step 2
  out=$($DC up -d --no-deps --no-build bot 2>&1) || { rollback "Restarting the bot failed: $(echo "$out" | tail -c 300)"; return; }
  step 3
  out=$($DC up -d --no-deps --no-build dashboard 2>&1) || { rollback "Restarting the dashboard failed: $(echo "$out" | tail -c 300)"; return; }

  step 4
  settled || { rollback "The new version did not start properly."; return; }
  finish done ""
}

# A run cut off by a restart of this container cannot be resumed safely.
q -c "update update_runs set status = 'failed', error = 'The updater restarted during the update. Check the server and try again.', finished_at = now() where status = 'running'"
n=0
while :; do
  [ $((n % 10)) -eq 0 ] && q -c "insert into updater_seen values (true, now()) on conflict (id) do update set at = now()"
  n=$((n + 1))
  RUN=$(q -c "update update_runs set status = 'running' where id = (select min(id) from update_runs where status = 'queued') returning id" 2> /dev/null)
  [ -n "$RUN" ] && run
  sleep 3
done

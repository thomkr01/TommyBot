#!/usr/bin/env bash
# Daily dump: keep 7 local copies, upload to RCLONE_REMOTE, drop remote copies older than 30 days.
set -euo pipefail
cd "$(dirname "$0")/.."
set -a
[ -f .env ] && . ./.env
set +a
dir=${BACKUP_DIR:-$HOME/tommybot-backups}
mkdir -p "$dir"
file=$dir/$(date +%F).sql.gz
docker compose exec -T postgres pg_dump --clean --if-exists -U tommybot tommybot | gzip > "$file.tmp"
mv "$file.tmp" "$file"
find "$dir" -name '*.sql.gz' -mtime +7 -delete
# The remote is optional: without RCLONE_REMOTE only the local copies are kept.
[ -n "${RCLONE_REMOTE:-}" ] || exit 0
rclone() { docker run --rm -v "${RCLONE_CONFIG_DIR:-$HOME/.config/rclone}:/config/rclone" -v "$dir:/data:ro" rclone/rclone "$@"; }
rclone copy /data "$RCLONE_REMOTE" --include '*.sql.gz'
rclone delete "$RCLONE_REMOTE" --min-age 30d

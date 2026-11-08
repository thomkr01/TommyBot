# Running on a server

This runs TommyBot with Docker on a Linux server next to other services. The dashboard is reached through a Cloudflare Tunnel, so the server opens no ports and needs no reverse proxy or certificates. The examples use `tommybot.example.com`.

## 1. Cloudflare Tunnel

1. In the Cloudflare dashboard, go to **Zero Trust > Networks > Tunnels > Create a tunnel**, pick **Cloudflared**, and name it `tommybot`.
2. Copy the token from the install command (the long string after `--token`). Skip installing the connector; Docker runs it.
3. Add a **public hostname**: subdomain `tommybot`, your domain, service type `HTTP`, URL `dashboard:8080`. Delete any existing DNS record for that name first.

## 2. Discord Developer Portal

Under **OAuth2 > Redirects**, add `https://tommybot.example.com/auth/callback`.

## 3. Server

Needs Docker with the compose plugin (`curl -fsSL https://get.docker.com | sh` on a fresh server). Building takes a few GB of RAM.

```bash
git clone https://github.com/thomkr01/tommybot.git
cd tommybot
cp .env.example .env
nano .env
```

Fill in the Discord values and `POSTGRES_PASSWORD`, then set:

```bash
DASHBOARD_URL=https://tommybot.example.com
TUNNEL_TOKEN=<token from step 1>
COMPOSE_PROFILES=tunnel
```

`COMPOSE_PROFILES=tunnel` makes every `docker compose` command include the tunnel. Start it:

```bash
docker compose up -d --build
docker compose ps
```

The containers restart on their own after a crash or reboot, as long as Docker starts at boot (`sudo systemctl enable docker`).

## Updating

GitHub Actions builds the images on every push to `main` and publishes them to `ghcr.io/thomkr01/tommybot-bot` and `tommybot-dashboard` (arm64, tags `latest` and the short commit sha; only the newest 3 versions are kept). Pulling is much lighter than building on the server:

```bash
cd ~/tommybot && git pull && docker compose pull && docker compose up -d && docker image prune -f
```

New packages on GitHub start private. Make both packages public once (package page > **Package settings** > **Change visibility**), which keeps them free. If you keep them private, log in once with a classic personal access token that has only the `read:packages` scope:

```bash
echo <token> | docker login ghcr.io -u thomkr01 --password-stdin
```

Roll back by pinning an older sha tag (see the package versions on GitHub) in `.env`, then run `docker compose pull && docker compose up -d`. Remove the line to follow `latest` again:

```bash
TOMMYBOT_TAG=a1b2c3d
```

To build on the server instead, use `docker compose up -d --build`.

## Database backup

`scripts/backup.sh` dumps the database daily, keeps 7 copies in `~/tommybot-backups`, and (optionally) uploads them with rclone to a remote, where copies older than 30 days are deleted. It uses the official `rclone/rclone` image, so nothing is installed.

### Off-site remote (free)

Google Drive (15 GB free, no card) is the default. Backblaze B2 (10 GB free) works the same way; for Cloudflare R2 you need a payment card on file, so it is not used here. Create the remote once, interactively:

```bash
mkdir -p ~/.config/rclone
docker run -it --rm -v ~/.config/rclone:/config/rclone rclone/rclone config
```

Choose `n` (new remote), name it `gdrive`, storage `drive`, accept the defaults, and sign in. On a server with no browser answer `n` to "Use web browser to automatically authenticate" and run the printed `rclone authorize "drive"` command on your own computer (rclone is a single download from rclone.org), then paste the result back. For B2 pick `b2` and paste an application key.

Then set in `.env` (`gdrive:` is the remote name, the part after the colon is a folder that rclone creates):

```bash
RCLONE_REMOTE=gdrive:tommybot-backups
```

Leave `RCLONE_REMOTE` unset to keep local copies only.

### Schedule

Create the folder once with `mkdir -p ~/tommybot-backups`, test with `~/tommybot/scripts/backup.sh`, then add to `crontab -e`:

```bash
0 4 * * * ~/tommybot/scripts/backup.sh
```

### Restore test

`scripts/restore-test.sh` restores the newest local dump into a throwaway `postgres:16` container, checks that `guild_config` and `cases` exist, prints their row counts, and exits non-zero on failure. Run it monthly (cron mails or logs the failure; here it appends to a log):

```bash
0 5 1 * * ~/tommybot/scripts/restore-test.sh >> ~/tommybot-backups/restore-test.log 2>&1
```

### Restoring for real

```bash
gunzip -c ~/tommybot-backups/2026-11-01.sql.gz | docker compose exec -T postgres psql -U tommybot tommybot
```

To fetch a dump from the remote first: `docker run --rm -v ~/.config/rclone:/config/rclone -v ~/tommybot-backups:/data rclone/rclone copy gdrive:tommybot-backups /data`.

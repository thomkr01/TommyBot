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

```bash
cd ~/tommybot && git pull && docker compose up -d --build && docker image prune -f
```

## Database backup

A daily dump at 04:00, keeping 7 days. Add it with `crontab -e`:

```bash
0 4 * * * cd ~/tommybot && docker compose exec -T postgres pg_dump --clean --if-exists -U tommybot tommybot | gzip > ~/tommybot-backups/$(date +\%F).sql.gz && find ~/tommybot-backups -name '*.sql.gz' -mtime +7 -delete
```

Create the folder once with `mkdir -p ~/tommybot-backups`. To restore a dump:

```bash
gunzip -c ~/tommybot-backups/2026-11-01.sql.gz | docker compose exec -T postgres psql -U tommybot tommybot
```

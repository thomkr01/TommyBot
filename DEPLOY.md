# Running on a server

A step-by-step guide from an empty server to a running TommyBot with a dashboard at `https://tommybot.example.com` and, optionally, an MCP server for Claude at `https://mcp.example.com`. Replace `example.com` with your domain everywhere.

The dashboard and the MCP server are reached through a Cloudflare Tunnel, so the server opens no ports and needs no reverse proxy or certificates.

**You need:**

- A Linux server (VPS) you can reach with SSH. The published images are **arm64** (for example Oracle Cloud's free Ampere servers). On an amd64 server, build the images yourself (`docker compose up -d --build`, see [Updating](#5-updating)).
- A domain whose DNS is on Cloudflare (the free plan is enough).
- A Discord account.

Commands marked **(server)** run in an SSH session on the server, **(computer)** on your own computer. Values in `<angle brackets>` are placeholders: replace the whole thing, brackets included.

## 1. Discord application

In the [Discord Developer Portal](https://discord.com/developers/applications):

1. **New Application**, name it (for example `TommyBot`).
2. **OAuth2**: copy the **Client ID** and **Client Secret** (reset it if it is hidden). Under **Redirects**, add `https://tommybot.example.com/auth/callback` (the dashboard login). If you will use Claude through claude.ai (step 6), also add `https://mcp.example.com/oauth/callback`. Save.
3. **Bot**: **Reset Token** and copy it. This is `DISCORD_TOKEN`; anyone with it controls the bot, so never paste it anywhere but `.env`. Enable **Server Members Intent** and **Message Content Intent**. Save.
4. Invite the bot to your server with this URL (replace `CLIENT_ID`), then in Discord drag the bot's role to the top of the role list:

   ```text
   https://discord.com/oauth2/authorize?client_id=CLIENT_ID&scope=bot+applications.commands&permissions=8
   ```

## 2. Cloudflare Tunnel

In the [Cloudflare dashboard](https://dash.cloudflare.com):

1. **Networking > Tunnels > Create a tunnel**, pick **Cloudflared**, and name it `tommybot`.
2. Copy the token from the install command: the long string after `--token`, not the tunnel ID. Skip installing the connector; Docker runs it. This is `TUNNEL_TOKEN`.
3. In the tunnel, **Routes > Add route > Published application**: subdomain `tommybot`, your domain, service URL `http://dashboard:8080`, path empty. If a DNS record with that name already exists, delete it first (Cloudflare creates the record itself).
4. Optional, for the MCP server (step 6): add a second route the same way, subdomain `mcp`, service URL `http://mcp:8090`. Until the `mcp` container runs, that address answers with a 502 error page; that is harmless.

The service URLs are container names, so they only work from inside the Docker network, which is where the tunnel runs.

## 3. Server

**(server)** Install Docker with the compose plugin and make it start at boot:

```bash
curl -fsSL https://get.docker.com | sh
sudo systemctl enable docker
sudo usermod -aG docker $USER   # then log out and in again, so docker works without sudo
```

The `docker` group is root-equivalent; only add your own account.

**(server)** Get the code and create `.env`:

```bash
git clone https://github.com/thomkr01/tommybot.git
cd tommybot
cp .env.example .env
nano .env
```

Fill in (nano: arrow keys to move, Ctrl+O and Enter to save, Ctrl+X to close):

```bash
DISCORD_TOKEN=<bot token from step 1.3>
DISCORD_CLIENT_ID=<client id from step 1.2>
DISCORD_CLIENT_SECRET=<client secret from step 1.2>
DASHBOARD_URL=https://tommybot.example.com
POSTGRES_PASSWORD=<letters and digits only, it goes into a URL>
TUNNEL_TOKEN=<token from step 2.2>
COMPOSE_PROFILES=tunnel
```

`COMPOSE_PROFILES=tunnel` makes every `docker compose` command include the tunnel container. A random password: `openssl rand -hex 24`.

**(server)** Start:

```bash
docker compose pull
docker compose up -d
docker compose ps
```

All services should show `running` (postgres `healthy`). Open `https://tommybot.example.com` and log in with Discord. The containers restart on their own after a crash or a reboot.

## 4. GitHub packages (images)

GitHub Actions builds the images on every push to `main` and publishes four packages to GitHub's container registry (GHCR): `tommybot-bot`, `tommybot-dashboard`, `tommybot-updater` and `tommybot-mcp` (arm64, tags `latest` and the short commit sha; only the newest 3 versions are kept). `docker compose pull` downloads them.

- **Using these images as they are:** nothing to do; they are public.
- **Running your own fork:** your Actions publish to `ghcr.io/<your-github-name>/tommybot-*`. Change the four `image:` lines in `docker-compose.yml` to your name. New packages start **private**, and the server then gets `denied` when it pulls. Make each package public once: GitHub > your profile > **Packages** > the package > **Package settings** (bottom right) > **Danger Zone** > **Change visibility** > **Public**, and type the name to confirm. A package only appears after its first build. This makes the image public, not your code or `.env`. To keep them private instead, log in on the server once with a classic personal access token that has only `read:packages`: `echo <token> | docker login ghcr.io -u <your-github-name> --password-stdin`.

## 5. Updating

**(server)**

```bash
cd ~/tommybot && git pull && docker compose pull && docker compose up -d && docker image prune -f
```

Wait for the **Images** workflow on GitHub (Actions tab) to finish after a merge first, or you pull the previous version.

To build on the server instead of pulling (needed on amd64): `docker compose up -d --build`. Building takes a few GB of RAM and up to 20 minutes on a small arm64 server.

### From the dashboard

The bot owner (the owner of the Discord application) sees **Update** in the dashboard header. It shows the running and the newest published version and updates with one click, with live progress and a time estimate based on earlier updates. It needs the `updater` service, which starts with the others; on a server set up before it existed, run `git pull && docker compose up -d` once.

The updater is the only container with the Docker socket. It has no open port and takes no input except an "update now" row in the database, so the dashboard (which faces the internet) never controls Docker. It pulls the bot and dashboard images, recreates both, and checks that they stay up. If anything fails it puts the previous images back and restarts on them (they stay on disk under the `rollback` tag). It does not update itself, the `mcp` container or `docker-compose.yml`: run the command above for those.

### Rolling back

Pin an older sha tag (see the package versions on GitHub) in `.env`, then `docker compose pull && docker compose up -d`. Remove the line to follow `latest` again:

```bash
TOMMYBOT_TAG=a1b2c3d
```

## 6. MCP server for Claude (optional)

The `mcp` service lets Claude read TommyBot: running version, log lines from the last 24 hours (secrets masked), guild config and its history, cases, and the raid and join gate logs. **It cannot change anything.** It holds no bot token and no Docker socket, logs in to Postgres as `tommybot_mcp` (which sees only the `mcp_*` views, read-only), and mounts the log files read-only. Two ways in:

- **Claude Code** (the `claude` command in a terminal) uses a fixed token, `MCP_TOKEN`.
- **claude.ai** (web and apps, including Projects) logs in through Discord; only the user in `MCP_OWNER_ID` gets in.

### 6.1 Turn it on

You need the `mcp` route from step 2.4.

**(server)** Add the secrets. Each `echo` line adds one line to `.env` and prints nothing; that is normal:

```bash
cd ~/tommybot
echo "MCP_TOKEN=$(openssl rand -hex 32)" >> .env
echo "MCP_DB_PASSWORD=$(openssl rand -hex 24)" >> .env
nano .env
```

In nano, change `COMPOSE_PROFILES=tunnel` to `COMPOSE_PROFILES=tunnel,mcp`, save and close. Then:

```bash
docker compose pull && docker compose up -d
docker compose logs mcp | tail -3
```

The log shows `mcp listening on 0.0.0.0:8090`. The bot restarts too, because it sets the `tommybot_mcp` password at start.

**(computer)** Test it. Get the token on the server with `grep MCP_TOKEN .env` (copy what follows `=`), then:

```bash
curl -s https://mcp.example.com/mcp -H "Authorization: Bearer <token>" -H "content-type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | head -c 300
```

The answer starts with `{"id":1,"jsonrpc":"2.0","result":{"tools":[`.

### 6.2 Claude Code

**(computer)** Install Claude Code if `claude` is not found (`curl -fsSL https://claude.ai/install.sh | bash`, then open a new terminal, run `claude` once and log in). Then:

```bash
claude mcp add --scope user --transport http tommybot https://mcp.example.com/mcp --header "Authorization: Bearer <token>"
claude mcp list
```

`tommybot` shows as connected. `--scope user` makes it available in every folder. Start `claude` and ask, for example, "use tommybot and show the last 20 lines of the bot log"; allow the tool the first time.

### 6.3 claude.ai

claude.ai cannot send a fixed token; it uses OAuth, and the MCP server sends you through your Discord application to log in.

1. Discord Developer Portal > your app > **OAuth2 > Redirects**: `https://mcp.example.com/oauth/callback` must be listed (step 1.2).
2. Find your Discord user id: Discord > **User Settings > Advanced > Developer Mode** on, then right-click your name > **Copy User ID**. It is a long number.
3. **(server)** Replace `<your id>` with that number (digits only, no brackets):

   ```bash
   cd ~/tommybot
   echo "MCP_OAUTH_SECRET=$(openssl rand -hex 32)" >> .env
   echo "MCP_PUBLIC_URL=https://mcp.example.com" >> .env
   echo "MCP_OWNER_ID=<your id>" >> .env
   grep MCP_ .env
   docker compose pull mcp && docker compose up -d mcp
   docker compose logs mcp | tail -3
   ```

   The log no longer says `oauth off`.
4. claude.ai > **Settings > Connectors > Add custom connector**: name `TommyBot`, URL `https://mcp.example.com/mcp`. **Add**, then **Connect**, and log in with Discord. Anyone else who tries is refused.
5. Turn the connector on where you want it: in a chat with the tools button, or in a Project's settings.

Access tokens last an hour and claude.ai renews them for 30 days. Changing `MCP_OAUTH_SECRET` (then `docker compose up -d mcp`) logs every claude.ai client out at once.

### 6.4 Keeping it safe

Every tool call is logged with its arguments (`docker compose logs mcp`). Anyone with `MCP_TOKEN` can read everything the tools show, so keep it out of chats, screenshots and repositories. To revoke it, put a new value in `.env` and run `docker compose up -d mcp`, then run `claude mcp add` again on your computer. Text from Discord (reasons, names, messages) reaches Claude through these tools; the server is read-only so such text cannot make Claude change anything in TommyBot.

## 7. Troubleshooting

| What you see | Why | Fix |
|---|---|---|
| `Error response from daemon: ... denied` on `docker compose pull` | The package is private, or not built yet | Wait for the **Images** workflow to finish, then make the package public (step 4) |
| `WARNING: Some service image(s) must be built from source` | Same: an image could not be pulled | As above, then pull again |
| Dashboard or `mcp.` address shows a Cloudflare 502 page | The container is not running, or the route's service URL is wrong | `docker compose ps`; check the route is `http://dashboard:8080` or `http://mcp:8090` |
| `mcp` keeps restarting, log says `MCP_OWNER_ID must be a Discord user id` | The placeholder was pasted literally | `sed -i 's/^MCP_OWNER_ID=.*/MCP_OWNER_ID=<your id>/' .env`, then `docker compose up -d mcp` |
| `mcp` log says `MCP_TOKEN must be at least 32 characters` or `not set` | Missing or short token | Redo the `echo "MCP_TOKEN=..."` line, check with `grep MCP_ .env` |
| `mcp` log says `oauth off` | One of the three OAuth values is missing | Step 6.3.3; `grep MCP_ .env` shows which |
| claude.ai login ends on Discord with "Invalid OAuth2 redirect_uri" | The redirect is not in the Discord app | Step 1.2 |
| claude.ai login says only the bot owner has access | You logged in with another Discord account, or `MCP_OWNER_ID` is wrong | Check the id (step 6.3.2) |
| curl answers `{"error":"unauthorized"}` | Wrong or incomplete token | One space between `Bearer` and the token, no `<>` |
| `logs_tail` says no logs | The `logs` volume is missing | `git pull && docker compose up -d` once |
| `command not found: claude` | Claude Code is not installed | Step 6.2 |
| `UserPromptSubmit hook error ... node: command not found` in Claude Code | A hook in your own Claude Code setup needs Node.js; not TommyBot | Harmless; install Node (`brew install node` on a Mac) to silence it |
| No log output at all | `RUST_LOG` too low | Compose sets `RUST_LOG=info`; keep it |

## 8. Database backup

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

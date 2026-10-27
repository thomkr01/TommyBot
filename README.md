# TommyBot

A Discord moderation bot written in Rust, modelled on Wick and Sapphire. It protects a server from raids, nukes and spam, and comes with a web dashboard so every setting can be changed without commands. All features are free.

## Features

- **Anti-nuke**: limits on mass bans, kicks and channel or role deletion, with automatic revert and panic mode
- **Automod**: spam and link filters with strikes and punishments
- **Join gate and lockdown**: screen new members, lock channels during a raid
- **Verification**: captcha and web-based member verification
- **Logging**: message, member, role, channel and moderation logs
- **Appeals and cases**: ban appeals, notes, reports and a full case history
- **Permits**: staff get no Discord permissions; the bot does the moderation, so a compromised staff account is limited and revertible
- **Web dashboard**: configure everything in the browser after logging in with Discord

## Installation

You need Docker with the Compose plugin and a Discord application.

**1. Create the Discord app.** In the [Developer Portal](https://discord.com/developers/applications):

- Create a **New Application** and copy the Application ID and Client Secret.
- Under **OAuth2 > Redirects**, add `<DASHBOARD_URL>/auth/callback`, for example `http://localhost:8080/auth/callback`.
- Under **Bot**, reset and copy the token, and enable **Server Members Intent** and **Message Content Intent**.
- Invite the bot with this URL, replacing `CLIENT_ID`, then move the bot's role to the top of the role list:

```text
https://discord.com/oauth2/authorize?client_id=CLIENT_ID&scope=bot+applications.commands&permissions=8
```

**2. Get the code and configure it.** Windows users run `git` in MSYS2 UCRT64.

```sh
git clone https://github.com/thomkr01/tommybot.git
cd tommybot
cp .env.example .env
```

Fill in `.env` (see [Configuration](#configuration)).

**3. Start everything.** Windows users run `docker` in PowerShell, from the repo folder.

```sh
docker compose up -d --build
```

Open `http://localhost:8080` and log in with Discord.

## Usage

Run these in PowerShell on Windows, in a normal shell elsewhere:

```sh
docker compose logs -f bot     # follow bot output
docker compose ps              # check the services are up
docker compose down            # stop (add -v to also delete the database)
```

Day-to-day setup happens in the dashboard. The bot also answers slash commands in Discord; type `/` to see them.

To run in production, use a Linux host and put a reverse proxy such as Caddy or nginx in front of the dashboard. Set `DASHBOARD_URL` to the https address and add the matching redirect in the Developer Portal.

### Without Docker

Needs a recent stable Rust toolchain and Postgres 16.

```sh
set -a; . ./.env; set +a       # the binaries read the environment, not .env
cargo run --release -p tommybot

# in a second terminal: build the dashboard UI once, then run it
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
(cd crates/dashboard/ui && trunk build --release)
cargo run --release -p tommybot-dashboard
```

Run the tests with `cargo test --workspace`. Set `DATABASE_URL` to include the database tests.

## Configuration

Settings are environment variables in `.env`; `.env.example` lists all of them with comments. The ones you must set:

| Variable | Purpose |
|---|---|
| `DISCORD_TOKEN` | Bot token |
| `DISCORD_CLIENT_ID`, `DISCORD_CLIENT_SECRET` | Dashboard login |
| `DASHBOARD_URL` | Public address of the dashboard |
| `POSTGRES_PASSWORD` | Database password (Docker) |
| `DATABASE_URL` | Database connection (without Docker) |

Everything else is optional. Per-server settings live in the dashboard. Dashboard access rules are in `crates/dashboard/README.md`.

## Testing without Discord credentials

`tests/e2e/run.sh` runs the workspace tests against a local Postgres, starts the dashboard against `tests/e2e/mock_discord.py` (a fake Discord REST API) and lets headless Chromium log in and open every dashboard page, failing on console errors, HTTP 5xx or blank pages. Screenshots land in `/tmp/e2e`. Needs postgresql-16, trunk, node with playwright (`NODE_PATH` set to the global modules). The bot itself needs a real token, so gateway behaviour is not covered.

## Legal

See [PRIVACY.md](PRIVACY.md) and [TERMS.md](TERMS.md).

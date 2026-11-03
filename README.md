# TommyBot

TommyBot is a fast, all-in-one Discord security and moderation bot written in Rust. It stops raids, nukes and spam within seconds, puts back what an attacker broke, and keeps a full record of everything that happens on your server. Every setting lives in a clean web dashboard, so you never have to memorise a command. Every feature is free: no premium tier and no paywall.

## Why TommyBot

- **Built in Rust**: one small process, low memory use and no garbage-collector pauses, so it reacts while an attack is still happening.
- **Undo for everything**: optional backups every 3 hours, automatic revert of deleted channels and roles, and a log entry for every action it takes.
- **Staff without dangerous rights**: moderators work through permits instead of Discord permissions, so a hacked staff account can do little and everything it does can be reverted.
- **Survives restarts**: timed locks, temp bans and half-finished reverts pick up where they stopped.
- **Everything free**: every feature, for every server.

## Features

- **Anti-nuke and panic mode**: limits on mass bans, kicks, channel, role and webhook changes, with quarantine, automatic revert and a panic lockdown.
- **Automod**: a heat system that scores every message for spam, mentions, links, invites, caps, emojis and more, catches coordinated spam across accounts, reads QR codes in images to stop account-stealing login codes, and punishes with escalating strikes.
- **Join gate and join raid**: screen new accounts and bots, and lock the server when a raid starts.
- **Lockdown**: lock one channel, a category or the whole server, on a timer if you like, and unlock exactly what was locked.
- **Verification**: captcha in Discord or on the web.
- **Logging**: message, member, role, channel, voice and moderation logs in tidy embeds, sent only to private log channels.
- **Appeals, reports and cases**: ban and timeout appeals, a report button for members, notes and a searchable case history.
- **Tickets, embeds and Discord AutoMod**: members DM TommyBot to open a ticket with any server they share with it, staff answer in a private channel (anonymous replies, snippets, greeting and closing messages, blacklist, full transcripts viewable on the dashboard); plus an embed builder and control of Discord's own AutoMod rules.
- **Web dashboard**: log in with Discord, run the setup wizard and change every setting in the browser, protected by two-factor login.

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

To run on a server, see [DEPLOY.md](DEPLOY.md).

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

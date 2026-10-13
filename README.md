# TommyBot

Rust Discord moderation bot modelled on Wick and Sapphire. Two binaries:

- `tommybot` (`crates/bot`): the gateway bot.
- `tommybot-dashboard` (`crates/dashboard`): web dashboard, axum API plus a Leptos UI.

Both need the same Postgres. Migrations run on startup.

## 1. Create the Discord app

1. Open the [Discord Developer Portal](https://discord.com/developers/applications) and click **New Application**.
2. **General Information**: copy the **Application ID**. This is `DISCORD_CLIENT_ID`.
3. **OAuth2**: copy the **Client Secret**. This is `DISCORD_CLIENT_SECRET`. Under **Redirects**, add `<DASHBOARD_URL>/auth/callback`, for example `http://localhost:8080/auth/callback`.
4. **Bot**: click **Reset Token** and copy it. This is `DISCORD_TOKEN`. Under **Privileged Gateway Intents**, turn on **Server Members Intent** and **Message Content Intent**. The bot will not connect without them. Discord requires approval for these once the bot is in more than 100 servers.
5. Invite the bot to your server with this URL, replacing `CLIENT_ID`:

   `https://discord.com/oauth2/authorize?client_id=CLIENT_ID&scope=bot+applications.commands&permissions=8`

   Permission `8` is Administrator. That matches the design: staff get no Discord permissions, and the bot does the moderation. After the invite, drag the bot's role to the top of the role list. Discord rejects actions on roles above the bot.

## 2. Configure

```sh
cp .env.example .env
```

Fill in the values. `.env` is ignored by git.

## 3. Run with Docker (local or server)

You need Docker Engine with the Compose plugin. Nothing else is installed on the host.

```sh
docker compose up -d --build
docker compose logs -f bot        # bot output
docker compose ps                 # all three services should be up
```

Open `DASHBOARD_URL` (default `http://localhost:8080`) and log in with Discord.

Compose starts Postgres, the bot and the dashboard. Postgres is not exposed to the host. Its data lives in the `pgdata` volume. Stop with `docker compose down`. Add `-v` only if you want to delete the data.

**On a server:** run the same commands on a Linux host. The bot makes outbound connections only. For HTTPS, put a reverse proxy in front of the dashboard, such as Caddy or nginx, that forwards to `127.0.0.1:8080`. Set `DASHBOARD_URL` to the https address, and add the matching `/auth/callback` redirect in the Discord portal. Then restart with `docker compose up -d`.

## 4. Run without Docker

You need a recent stable Rust toolchain, Postgres 16, and `psql` for creating the database.

```sh
createdb tommybot                 # or create a role and database with psql
cp .env.example .env              # set DATABASE_URL, for example postgres://user:pass@localhost:5432/tommybot
set -a; . ./.env; set +a          # the binaries read the environment, not .env

# Bot
cargo run --release -p tommybot

# Dashboard: build the UI once, then run the API
rustup target add wasm32-unknown-unknown
cargo install trunk --version 0.21.14 --locked
(cd crates/dashboard/ui && trunk build --release)
cargo run --release -p tommybot-dashboard
```

The dashboard serves the UI from `crates/dashboard/ui/dist` by default. Override with `DASHBOARD_DIST`.

Tests run against a real database when `DATABASE_URL` is set, and skip otherwise:

```sh
DATABASE_URL=postgres://... cargo test --workspace
```

## Environment reference

| Variable | Used by | Required | Purpose |
|---|---|---|---|
| `DISCORD_TOKEN` | bot, dashboard | yes | Bot token |
| `DATABASE_URL` | bot, dashboard | yes | Postgres connection string |
| `DISCORD_CLIENT_ID` | dashboard | yes | OAuth2 login |
| `DISCORD_CLIENT_SECRET` | dashboard | yes | OAuth2 login |
| `DASHBOARD_URL` | dashboard, bot | yes | Public base URL, used for the OAuth redirect |
| `SUPPORT_URL` | bot | no | Link behind the Support button in the welcome message |
| `BIND` | dashboard | no | Listen address, default `0.0.0.0:8080` |
| `DASHBOARD_DIST` | dashboard | no | Built UI folder |
| `RUST_LOG` | both | no | Log level, default `info` in Docker |

Dashboard access rules are in `crates/dashboard/README.md`.

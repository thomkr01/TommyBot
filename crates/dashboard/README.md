# tommybot-dashboard

axum API (`src/`) plus a Leptos CSR UI (`ui/`, built with `trunk build --release`, served from `DASHBOARD_DIST`, default `crates/dashboard/ui/dist`).

Env: `DATABASE_URL`, `DASHBOARD_URL` (public base URL), `DISCORD_CLIENT_ID`, `DISCORD_CLIENT_SECRET`, `DISCORD_TOKEN` (bot), optional `BIND`.
Add `<DASHBOARD_URL>/auth/callback` as an OAuth2 redirect in the Discord developer portal.

Saves append a `guild_config` version and `NOTIFY config_changed, '<guild_id>'`; the bot should `LISTEN` and reload that guild.
Access comes from `extra_owners`, `trusted_admins` and permits holding `dashboard_view` / `dashboard_edit` (Discord Manage Server is ignored).

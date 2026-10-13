# TommyBot

Rust Discord moderation bot. Workspace: `crates/core` (config, db, models, migrations), `crates/bot` (gateway binary). Feature crates live in `crates/<name>` and join `members` automatically via the `crates/*` glob.

Run: set `DISCORD_TOKEN` and `DATABASE_URL`, then `cargo run -p tommybot`.

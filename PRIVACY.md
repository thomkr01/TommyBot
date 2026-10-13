# TommyBot Privacy Policy

Last updated: 2026-10-13

This policy explains what TommyBot ("the bot") stores, why, and how long it is kept. The bot is free and open source. Its code is at [github.com/thomkr01/TommyBot](https://github.com/thomkr01/TommyBot).

## What the bot receives from Discord

When the bot is in your server, Discord sends it the events it needs to do its job: member joins and leaves, role and channel changes, audit log entries, and messages in channels it can read. The bot reads message content so that automod and logging can work.

## What the bot stores

The bot keeps data in a PostgreSQL database. It stores:

| Data | Why | Kept for |
|---|---|---|
| **Server config** (automod settings, log channels, permit roles, limits). Every change is saved as a new version. | To run the bot's features and to show config history on the dashboard. | As long as the server uses the bot, and until deletion is requested. |
| **Moderation cases** (action type, target and moderator user IDs, reason, proof link you provide, points, expiry). | Warnings, bans, and history for `/case` and warn thresholds. | Until deletion is requested. Not removed automatically. |
| **Action log** (each mutating action with the data needed to undo it). | `/revert` and audit history. | Until deletion is requested. Not removed automatically. |
| **Automod strike state** (user IDs, strike counts and timestamps). | Keeps repeat-offender penalties working across restarts. | Until deletion is requested. |
| **Quarantine and lockdown records** (saved role IDs, lock snapshots). | To restore roles and channel permissions when a quarantine or lock ends. | Removed when the quarantine or lock is released. |
| **Scheduled jobs** (for example, a timed ban or timeout). | To run them after a restart. | Removed when the job runs. |
| **Server backups** (roles, channels, permission overwrites, as JSON). | `/backup` and nuke recovery. | A new backup is taken every 3 hours. Only the 10 most recent per server are kept. |
| **Nuke incident records** (who was quarantined and what was reverted). | Recovery history after an attack. | Until deletion is requested. |
| **Dashboard sessions** (your Discord user ID and username, your Discord OAuth access token, a CSRF token, and a step-up timestamp). | To keep you signed in to the dashboard. | 7 days from sign-in, then expired. |
| **Dashboard two-factor setup** (your user ID and TOTP secret). | Optional second factor for dashboard changes. | Until you remove it or deletion is requested. |
| **Rescue keys** (a hash of the key, not the key itself). | Lets a server owner reclaim bot ownership if they lose their Discord account. | Until the key is used or deletion is requested. |

Message content is not written to the database. The bot keeps a short-term in-memory cache of recent messages (up to 10,000 messages, for 30 minutes, then discarded) so it can log edits and deletions. This cache is lost when the bot restarts.

The bot does not sell data, show ads, or send data to third parties beyond Discord and the hosting provider that runs the bot.

## Why the bot stores it

Only to run the features you turn on: moderation, logging, automod, anti-nuke, backups, and the dashboard. The bot does not use this data for profiling or advertising.

## Who can see the data

- Server staff see moderation data for their server through Discord and, if they sign in, the dashboard. Staff only see what their Discord permissions and the bot's permit rules allow.
- The bot's operator can access the database to run and maintain the bot.

## Deleting data

- **Remove the bot from your server.** This stops new data collection. It does not by itself delete stored data.
- **Request deletion.** Open an issue at [github.com/thomkr01/TommyBot/issues](https://github.com/thomkr01/TommyBot/issues) and say which server ID, or which user ID, you want removed. Server owners can request deletion of a whole server's data. Users can request deletion of their own user ID from stored cases, strike state, and dashboard records. Please do not post private personal data in a public issue; say you want to send it privately and a maintainer will reply.
- We aim to complete deletion requests within 30 days.
- Backups for a server are deleted with that server's data.

## Children

Discord requires users to be at least 13 (or the minimum age in your country). The bot does not knowingly collect data from anyone below that age beyond what Discord sends for server activity.

## Security

Data is stored in a private database. Dashboard sessions expire after 7 days. Dashboard changes are protected by CSRF checks and optional TOTP step-up. No system is perfectly secure. If you find a security problem, report it privately through the issues page by asking for a private channel.

## Changes to this policy

We may update this policy. The date at the top shows the latest version.

## Contact

[GitHub issues](https://github.com/thomkr01/TommyBot/issues)

_This is a template policy written for a small open-source project. It is not legal advice. Have a lawyer review it before relying on it for anything commercial, and check it against the data you actually store, which changes as the bot changes._

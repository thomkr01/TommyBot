# TommyBot Privacy Policy

Last updated: 2026-11-11

This policy explains what TommyBot ("the bot") stores, why, and how long it is kept. The bot is free and open source. Its code is at [github.com/thomkr01/TommyBot](https://github.com/thomkr01/TommyBot).

## What the bot receives from Discord

When the bot is in your server, Discord sends it the events it needs to do its job: member joins and leaves, role and channel changes, audit log entries, and messages in channels it can read. The bot reads message content so that automod and logging can work.

If you send the bot a direct message, it is treated as a ticket: the bot forwards it (with its files) to a staff-only ticket channel in the server you pick, and forwards staff replies back to you.

## What the bot stores

The bot keeps data in a PostgreSQL database. It stores:

| Data | Why | Kept for |
|---|---|---|
| **Server config** (automod settings, log channels, permit roles, limits). Every change is saved as a new version. | To run the bot's features and to show config history on the dashboard. | As long as the server uses the bot: deleted 30 days after the bot is removed, or when the owner deletes it. |
| **Moderation cases** (action type, target and moderator user IDs, reason, proof link you provide, points, expiry). | Warnings, bans, and history for `/case` and warn thresholds. | Until deletion is requested. Not removed automatically. |
| **Staff notes** (member and moderator user IDs, the note text, when it was written) and **note edit history** (old and new text, who edited it and when). | Private notes staff keep about members; the history shows every earlier version on the dashboard. | Until staff remove the note (its history goes with it) or deletion is requested. |
| **Action log** (each mutating action with the data needed to undo it). | `/revert` and audit history. | Until deletion is requested. Not removed automatically. |
| **Moderator actions** (the staff member's and target's user IDs, the action type, where it was done: TommyBot, the Discord app, an appeal or the dashboard, the case number, and when). Includes bans, kicks and timeouts staff do in the Discord app, read from the server's audit log. | Moderator stats on the dashboard, so owners can see how active each staff member is. | 1 year, then removed automatically. |
| **Automod strike state** (user IDs, strike counts and timestamps). | Keeps repeat-offender penalties working across restarts. | Until deletion is requested. |
| **Quarantine and lockdown records** (saved role IDs, the channel overwrites a quarantine replaced, lock snapshots). | To restore roles and channel permissions when a quarantine or lock ends. | Removed when the quarantine or lock is released. |
| **Command rate-limit uses** (`command_uses`: the staff member's user ID, the command and the time) and **heat panic** (`heat_panic`: the server ID, the user IDs heat panic acts on, and when it ends). | So a restart does not reset the command limits and an active heat panic. | 1 hour for command uses; heat panic until it ends. Removed automatically. |
| **Scheduled jobs** (for example, a timed ban or timeout). | To run them after a restart. | Removed when the job runs. |
| **Server backups** (roles, channels, permission overwrites, as JSON). | `/backup` and nuke recovery. | A new backup is taken every 3 hours. Only the 10 most recent per server are kept. |
| **Server owners** (the server ID and the user ID of its Discord owner). | To find the other servers with the same owner for Ban Sync. The owner is checked with Discord again before every copy. | Updated when the owner changes, deleted when the bot leaves the server. |
| **Owner changes** (`owner_handovers`: the server ID, the previous and the new owner's user IDs, the user IDs of the Extra Owners, Trusted Admins and quarantine whitelist users the bot removed, the role IDs and names of the permits whose roles it paused, the names of the permits that stayed on, whether Ban Sync was on, and when). | So the new owner can keep the previous owner's staff, one by one, after the bot removed them at the change, and so the dashboard can refuse the previous owner. | Replaced at the next owner change; removed with the server's data: 30 days after the bot is removed, or when the owner deletes it. |
| **Ban Sync copies** (the receiving and sending server IDs, the banned user's ID, the reason (at most 400 characters), what made the ban, and, for a copy laid over a local temp ban, that ban's reason and end, the result and the case number). | So a ban is copied only once, an unban lifts only the copies Ban Sync made, and staff see every copy in the Log tab. | Removed with the receiving server's data: 30 days after the bot is removed, or when the owner deletes it. |
| **Raider List entries** (`raiders`: the user's ID, the status, the category, whether the bot owner verified it, a public note of at most 200 characters, who reviewed it and when, and when a scam listing ends or the next appeal opens). | So servers that use the Raider List can act on confirmed raider accounts when they join. | Confirmed entries stay until the bot owner removes them; scam entries end 14 days after their last evidence. Rejected entries are deleted after 30 days, and pending entries nobody reviewed after 90 days. |
| **Raider List evidence** (`raider_evidence`: the user's ID, the server and its owner's ID, what caught them, the case or incident number, counts and timestamps, and for a report the reporter's ID). Never message text. | So the bot owner can check an entry, and so one owner with many servers counts once. Only the bot owner sees it. | Deleted with its entry (pending 90 days, rejected 30 days, scam 14 days after the last evidence); kept while an entry is confirmed. |
| **Raider List reviews** (`raider_reviews`: who decided, the user's ID, the old and new status, the note and the time). | A record of every review decision. | Kept for good. |
| **Raider List appeals** (`raider_appeals`: the user's ID, their appeal text of at most 1,000 characters, the result, who decided and when). | So a listed user can ask to be taken off the list. | Kept with the entry's review history. |
| **Raider List actions** (`raider_actions`: the server ID, the user's ID, what the list did there and the case number). | So staff see the list's matches in their server, and a removal lifts only what the list did. | Removed with the server's data: 30 days after the bot is removed, or when the owner deletes it. |
| **Raider List exceptions** (`raider_allow`: the server ID, the user's ID, the staff member who let them in and when). | So the list never acts again on someone staff allowed in that server. | Removed with the server's data: 30 days after the bot is removed, or when the owner deletes it. |
| **Join Gate records** (the punished user's ID, whether it is a bot and who added it, the filters that matched, the action, and Discord's error if it failed). | To show staff every Join Gate action on the dashboard. | 30 days, and at most the 1,000 newest per server. |
| **Nuke incident records** (who was quarantined and what was reverted). | Recovery history after an attack. | Until deletion is requested. |
| **Dashboard sessions** (your Discord user ID and username, your Discord OAuth access token, a CSRF token, and a step-up timestamp). | To keep you signed in to the dashboard. | 7 days from sign-in, then expired. |
| **Dashboard two-factor setup** (your user ID and TOTP secret). | Optional second factor for dashboard changes. | Until you remove it or deletion is requested. |
| **Tickets** (server, ticket number, channel, your user ID, open and close times, who closed it) and your ticket preferences (whether to confirm before sending, the server your last ticket message went to). | To route ticket messages between you and the server's staff. | Until deletion is requested. |
| **NSFW image scans** (`nsfw_scans`: the server, channel, message and user IDs, a SHA-256 hash of the image, never the image or its link, the model's scores, whether it was a hit, and staff feedback). | So the scan acts once per image, staff can mark a wrong hit, and the threshold can be tuned. Images are only held in memory while they are scored. | 30 days. |
| **Web verification address hashes** (`verify_ips`: the server ID, a salted hash of the IP address, never the address itself, the user's ID and the time). | To warn staff when several accounts verify from the same address. The VPN block compares the address with downloaded lists in memory and stores nothing. | 30 days. |
| **Rescue keys** (a hash of the key, not the key itself). | Lets a server owner reclaim bot ownership if they lose their Discord account. | Until the key is used or deletion is requested. |

Message content is not written to the database. When staff close a ticket, the conversation is posted as a text file in the server's private ticket log channel on Discord, where it stays until staff delete it. The bot keeps a short-term in-memory cache of recent messages (up to 50,000 messages per server, for 24 hours, then discarded) so it can log edits and deletions. This cache is lost when the bot restarts.

The bot does not sell data, show ads, or send data to third parties beyond Discord and the hosting provider that runs the bot.

## Why the bot stores it

Only to run the features you turn on: moderation, logging, automod, anti-nuke, backups, and the dashboard. The bot does not use this data for profiling or advertising.

## Who can see the data

- Server staff see moderation data for their server through Discord and, if they sign in, the dashboard. Staff only see what their Discord permissions and the bot's permit rules allow.
- The bot's operator can access the database to run and maintain the bot.

## Deleting data

- **Delete it from the dashboard.** A server owner can delete all of the server's stored data under Miscellaneous > Delete server data. This needs the authenticator code and the server's name typed in, and it cannot be undone. The bot stays in the server and goes back to its default settings.
- **Remove the bot from your server.** This stops new data collection. All of the server's stored data is deleted automatically 30 days after the bot is removed, unless the bot is added back before then.
- **Request deletion of your user ID.** Open an issue at [github.com/thomkr01/TommyBot/issues](https://github.com/thomkr01/TommyBot/issues) and say which user ID you want removed. Users can request deletion of their own user ID from stored cases, moderator actions, strike state, and dashboard records. Please do not post private personal data in a public issue; say you want to send it privately and a maintainer will reply.
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

# Changelog

All notable changes to TommyBot are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). TommyBot has no version numbers yet, so changes are grouped by pull request under Unreleased. Changes before #134 are in the merged pull requests on GitHub.

## [Unreleased]

### Fixed
- MCP writes: the owner's code DM is never cut. A change too long for it is refused, `embed_post` shows the embed it posts, a rollback shows live Extra Owner and Trusted Admin changes, and a change whose preview changed after the DM is refused. MCP login only returns to claude.ai's own callback page. Migration 3100. (#168)
- The backup channel list follows Discord's order. It sorted channels by category id instead of position, and matched channels to a category by name. (#164)
- DEPLOY.md step 6.4 names the right link: **MCP tokens** in the account menu. (#165)
- A dashboard icon used more than once no longer goes blank when its first copy is hidden. (#163)
- A bot that deletes the verification channel no longer keeps Administrator through its own bot role: that role is emptied and the bot quarantined, with a kick only as the fallback. The owner DM says when the bot was kicked. (#142)
- A punishment DM that gives up after 3 seconds is logged as a warning. (#142)
- The dashboard DM info box says that DMs for unban, untimeout and unquarantine come after the action. (#142)
- Dashboard deep test fixes: Cancel resets dropdowns, roles no longer show as "@@everyone", the Changes bar no longer hides right after a page loads, Join Raid can be turned on before its first save, an emptied number field saves, decimal fields show 100 instead of 100.0, two Overview switches in a row no longer conflict, Copy no longer says "Copied!" when the browser refused, the phone header fits the screen, and the theme choice is kept. (#158, #160)
- Auto Setup makes a saved role or channel again when it was deleted in Discord, and its steps stay green or red with the result left on the page. (#147, #151)
- The toast is one piece with only the bottom strip. (#148)
- Dashboard number fields show a plain box with - and + again instead of code text, and the - button works. (#143)
- The dashboard no longer crashes ("reactive value that has already been disposed") when you leave a page while it is still loading or saving, for example during an Update. (#143)

### Added
- View in Backups opens a page for that backup: the channels as a tree in Discord order (click a channel to see its permissions, Compact hides the icons), the server name, verification level and explicit media filter, the webhooks, and the roles in their colours (click one to see its permissions). Load and Delete are on the same page. (#164)
- The dashboard shows only what your permits allow: pages without access are hidden, read-only pages are greyed out with a note, and the Commands page greys out commands you cannot use. `COMMAND -h` no longer explains commands you cannot use. (#146)
- An info icon on 21 hard settings opens a plain-language explanation on hover, focus or tap. (#160)
- An optional MCP server so Claude can read the bot's data and logs, for the bot owner only: Claude Code logs in with a fixed token, claude.ai through Discord (redirects only to claude.ai and localhost, a login lasts at most 30 days). Bot and dashboard logs can also be written to hourly files in `LOG_DIR`. Setup is in DEPLOY.md, which is now a step-by-step guide from an empty server. (#149, #150, #155, #156, #161)
- The MCP server can make changes for Claude with a token made on the dashboard's new MCP tokens page (bot owner, 2FA to make or revoke, stored only as a hash, one per client): config sections, permits, config rollback, lockdown, notes, cases and embeds, plus the security score. Each change shows a diff and runs only after the owner types Claude the code the bot sent by DM; sections that need 2FA on the dashboard also need an authenticator code. Update, deleting server data, backups and anti-nuke revert stay out of reach. MCP logs now name the tool only, not its arguments. (#159)
- Icons for every page and group in the dashboard sidebar and on the Filters cards. (#163)
- On pages without a server (the server list, Commands, Update) the left menu lists every server you can manage. Click one to open it. The server you opened last is highlighted.
- A Yes/No question ("Do you want to proceed?", 10 seconds) before ban, kick, timeout, warn, quarantine, unquarantine and unban when the target is typed as a raw id or a name. A mention or a slash command pick never asks. Only the moderator who ran the command can answer. The Confirm switch turns it off. (#136)
- Members get a DM before a warn, timeout, quarantine, kick, ban, softban or tempban (not a hackban), and after an unquarantine, untimeout or unban worked. TommyBot waits at most 3 seconds for the DM and punishes whatever happens to it. (#136)
- Result messages list the reason, the moderator, the details of the action (duration, soft ban, hack ban, no avatar, voice, DM sent), who was punished and who was not, with the reason. (#136)
- `kick ?na` kicks only members without an avatar, and `kick ?vc` only members in a voice channel. (#136)
- A switch, on by default, that writes "Did not verify within the time limit" to the audit log when verification kicks, bans or quarantines a member. (#135)
- If the verification channel is deleted, TommyBot makes a new one with the same name, category, position and permissions and posts the panel again, at most 3 times in 10 minutes. The owner gets a DM saying who deleted it. That person is quarantined; the owner, Extra Owners and Trusted Admins get one warning first, and the Discord server owner only a DM. (#135)
- Every join gate punishment is logged in the Join Gate log with the filters that matched, the bot and who added it, and the error if Discord refused. The dashboard lists the last 30 days under Join Gate › Join Gate Log. (#135)
- An optional switch, off by default, lets bots added by the owner or an Extra Owner skip the join gate account filters. (#135)
- The security score flags a log channel anyone can see, with a Make private fix that takes a backup first. There is no fix while @everyone or a main role has Administrator; that has to be removed first. (#135)
- The security score warns when a role sits between the bot and the quarantine role, with a Move role fix that takes a backup first and only moves the quarantine role. (#133)
- A Panic Mode Report after every panic: the case, members quarantined and not, the actions taken and the backup outcome ("Backup System is disabled!" when backups or restore after a nuke are off). It is sent even when locking or pinging fails. (#133)
- The Revert Center can restore the permissions of a nuking bot's role. (#133)
- Staff notes keep their full edit history: the dashboard Notes page shows when a note was created and by whom, and a History list with the time, the editor and the old and new text of every edit. Discord still shows the note with its original time. (#134)
- Adding, editing, removing and sweeping notes is logged to the private mod log; an edit shows what changed. (#134)
- Dashboard Notes page has a Sweep server button behind a confirm dialog. (#134)

### Changed
- Channel Setup is rebuilt around the server's channel tree and replaces role profiles. Make, rename, move and delete categories and channels, and limit who can see one to some roles plus staff (the Admins and Moderators permit roles, which owners can leave out). Channels follow their category; a channel with its own settings is marked and keeps them when the category changes. Every change is a draft until Push, which checks what the draft changes with the security score's channel rules, refuses an unsafe change (with a Fix), and takes a backup. Problems elsewhere on the server stay on the Overview, which now also flags channels the quarantine role can see (with a Fix) and an unpushed Channel Setup draft that is not safe. Only the owner and extra owners can change channels and Push (with 2FA); trusted admins can view the page. Old role profiles are ignored, and old settings exports still import. (#166)
- `/revert` and the Revert Center leave out what an Owner, Extra Owner or Trusted Admin did; only an Owner or Extra Owner can include it. Saving the whitelist panel and adding Trusted Admins in Discord need a fresh dashboard 2FA step-up (account menu › Confirm 2FA). (#146)
- Permission checks are tighter: slowmode and lock refuse other servers' channels, unlocking needs the permit for each lock it lifts, releasing a quarantine checks the roles it gives back, a member cannot verify out of a non-join quarantine, verification never gives a permit role or a role with dangerous permissions, nobody can unwarn, close or edit their own case or an immune member's, and only owners can change anti-nuke whitelist flags or see the rescue key. (#146)
- Dependencies: twilight 0.18, constant_time_eq 0.6, Postgres 18 and newer GitHub Actions. (#152, #153, #157)
- The "always DM punished members" switch is on by default, also for existing servers, and one switch covers every punishment DM. `?dm` still forces a DM when it is off. (#136)
- Automatic punishments from warn thresholds DM the member first too. (#136)
- A target like `name#0` finds the same member as the name or the id. (#136)
- The verification Help message names the roles you get after passing, before the time limit line. (#135)
- The verification time limit stands still while a member is timed out, also when they join while timed out. If TommyBot missed the timeout update, the member gets the full time limit again after the timeout. (#135)
- Log setup in the dashboard makes a reused public log category private, and the owner gets one DM a day per channel when TommyBot holds back a log because the channel is public. (#135)
- A bot that nukes is no longer kicked: its own role loses all permissions and the bot is quarantined, so it stays in the server for review. A bot without its own role is quarantined too. A kick is only the fallback when that fails. (#133)
- Panic mode locks the server for 60 seconds by default, for anti-nuke panic and heat panic. Servers that still had the old 10-minute default move to 60 seconds; other saved values stay. (#133)
- Anti-nuke always reverts and logs first (and contains the nuker when auto quarantine is on), then starts panic mode. A failed log no longer stops panic mode. (#133)
- Sanitizing a channel an attacker made now gives an audit log reason. (#133)
- Number settings share one set of limits in the prefix panels, the dashboard and on save and import: anti-nuke Minute Cap 2–50, Hour Cap 2–500, Heat 1–100, and minimum account age 5 seconds to 100 days. The dashboard +/- buttons stop at the bounds, and a value outside a range is refused with a message instead of being changed silently. (#134)
- Saved values from before the limits keep working: a saved cap or minimum account age outside its range is read as the nearest limit, a saved heat of 0 means the card's default, and an unchanged old value never blocks a save or import. (#134)
- `jg 3c` accepts a bare number as days (`3` = 3 days) and still honours units like `12h`. (#134)
- A server-wide note sweep always asks for confirmation, even when confirmations are turned off. (#134)
- `ancaps` and the other settings panels answer an unknown card with the real range, for example "Options available are [1 to 7]", and name the letters a card has, instead of only saying the panel has no such setting. (#134)

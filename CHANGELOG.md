# Changelog

All notable changes to TommyBot are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). TommyBot has no version numbers yet, so changes are grouped by pull request under Unreleased. Changes before #134 are in the merged pull requests on GitHub.

## [Unreleased]

### Added
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

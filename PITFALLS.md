# Pitfalls

Technical mistakes that already happened in TommyBot and were fixed. Each entry says what went wrong, why, and how to avoid it next time. Read the section for the area you change before you start. Add a new entry when you fix a bug whose cause could happen again.

**When you fix a bug, search the whole codebase for the same mistake and for similar ones, and fix every place, not only the one that was reported.** Name in the entry where else it was found. (Thomas, 2026-11-10)

Dashboard UI (Leptos) details are in [crates/dashboard/ui/PITFALLS.md](crates/dashboard/ui/PITFALLS.md).

## 1. Discord API and permissions

**A bot's managed role cannot be removed.** (#58, #133, #142)
Quarantining a nuking bot failed, because Discord does not let anyone take a bot's own managed role away, and that role kept Administrator. #142 found the same hole again in `quarantine_actor` (verification channel delete, command rate limit), which skipped the shared path.
*Avoid:* every place that contains an actor goes through `antinuke::contain()`. It sets the managed role's permissions to 0, then quarantines, and kicks only as a fallback. Never write a second quarantine path.

**Role order decides everything.** (#57, #133)
The first live nuke test got 403 because TommyBot's role was tied with the attacker's. A role between the bot and the quarantine role also breaks quarantine.
*Avoid:* the bot role must sit above every role it acts on, and the quarantine role right below the bot. The security score checks the second one (Move role fix). When a live test gets 403, check the role order first.

**Tied role positions were compared as equal.**
On a server whose roles were never reordered, every role sits at position 1 (Skytest: both bots, Quarantine, Member, Admins, Moderators). Discord breaks the tie by id, with the older role ranking higher, but TommyBot compared only the numbers. So `/kick` and `/ban` refused any target with a role, anti-nuke would not strip Admins from a nuker, lockdown and `tshoot lockdown` skipped every role, the security score read them all as level with the bot, and Auto Setup never moved the quarantine role. `security.rs` already ranked ties by id, but other code called it with raw positions.
*Avoid:* rank roles once where they are read from Discord: `hierarchy::ranked()` in the bot, `ops::roles_of()` and `recovery::live_image()` in the dashboard, all built on `tommybot_core::roles::ranks()`. Every role list that is compared or written back goes through one of them. Never compare `Role::position` straight from Discord.

**Leaving and rejoining ended a quarantine.** (sticky roles plan, 11-11)
The `quarantines` row survived a leave, but no join code read it, so a quarantined member who left and came back was a normal member again. Only verification with "quarantine on join" put the role back, and only because it quarantines every unverified joiner. Checked for the same gap in other state: timeouts survive a rejoin in Discord itself, strikes and warn points are stored per user, and join lock and join raid act on every join.
*Avoid:* state that holds a member back must be re-applied on `MemberAdd`, not only while they stay. `antinuke::requarantine_rejoin` runs first in `joingate::on_join` and goes through `contain()`, so a re-added bot loses its managed role's permissions too.

**The post-join username filter punished immune staff.** (Join Gate check, 11-11)
Join Gate's username filter checks names again after join (the "POST join" switch) and called `punish` without a level check, so an Extra Owner or Trusted Admin who picked a nickname on the word list could be kicked or banned. At join time nobody is staff yet, so the gap only showed after join. The other automatic paths on existing members (automod, Discord AutoMod hits, reactions, threads) already go through `whitelisted()`.
*Avoid:* every automatic action against a member who is already in the server checks `is_immune` or `whitelisted` first. The one deliberate exception is auto-dehoist (and Join Gate's Rename through `dehoist::rename`): Thomas's D6 skips only the owner and Extra Owners, so Trusted Admins and mods are renamed too.

**Member edits share one rate-limit bucket.** (auto-dehoist, 11-11)
Nick changes, role adds, timeouts and kicks of members all use Discord's member bucket, so a burst of automatic renames or role restores slows the punishments of a raid.
*Avoid:* bulk automatic member edits are capped per guild (`dehoist::rename`, 10 a minute through `RateLimiter`) and auto-dehoist is off during a join raid or lockdown.

**Not every member lookup error means "not a member".** (#97)
All `guild_member` errors (5xx, timeouts) were read as "left the server". Staff then lost their permit level for that event, so automod or anti-nuke could act on them.
*Avoid:* use `http_err::member()`. Only a 404 means not a member. Other errors propagate.

**Swallowed errors hide missing permissions.** (#97)
`.await.ok()` dropped real failures, so a punishment Discord refused left no trace.
*Avoid:* use `http_err::best_effort()`. It stays quiet only for 404 and closed DMs (50007) and logs everything else. Failed automod actions go to the log with `action_failed()`.

**Bulk delete takes at most 100 messages.** (#97)
More than 100 gave a 400, nothing was deleted, and the error was swallowed.
*Avoid:* split with `reply::batches()` (100 per call, 14-day cutoff, plain delete for one message).

**Write state after the Discord call, or roll it back.** (#97)
The quarantine row was written before the role change. When the change failed, the bot believed a member was quarantined and refused a correct save later.
*Avoid:* write the row after the call succeeds, or delete the row you created when the call fails.

**Outgoing HTTP needs a timeout.** (#97)
The raw reqwest client had none, so one hung call could hold a lockdown or rule sync forever.
The dashboard's and the MCP server's Discord clients had none either, found 11-11.
*Avoid:* every client gets a timeout (15 s in `automod/src/discord_api.rs`, the dashboard's and the MCP's Discord client). Twilight already handles 429.

**A REST call before an interaction's answer can miss the 3 seconds.** (spammer-flag review, 11-11)
The verify Start button asked Discord for the spammer flag on every click before it answered, through `Api::send`, which waits out a 429 up to 30 s and retries; the 15 s client timeout was the only limit. Discord drops an interaction answer after 3 s, so a slow or rate-limited lookup left the member with "This interaction failed", and every click cost another request on one bucket. `verify::guild_name` (before the "verified" answer) had no limit either.
*Avoid:* do the work that needs no REST first and call only when the answer changes the result (`flagged_step`), cache the answer (`App::spam_flags`, 10 minutes, bounded), and bound the call: one try with no 429 wait (`Api::send_tries(.., 1)`) inside `tokio::time::timeout(1.5 s)`. Decide what a timeout means (here: a captcha, fail closed). Anything slower than that defers the interaction first.

**A DM can be slow or refused.** (#136, #142)
A DM before a punishment must not hold the punishment back, and a DM to a bot is refused.
*Avoid:* send DMs through the shared helper with the `DM_WAIT` limit (3 s). The punishment runs anyway. A DM that times out is logged.

**Discord's own rendering quirks.** (5ec560a, #92)
A mention of the @everyone role renders as `@@everyone`, so write plain `@everyone`. Discord lists each mentioned user once per message, so mention spam counts one ping per user per message. Discord merges audit log entries, so the bot's deletion of someone else's message can show no Responsible.

**A saved id can point at something deleted in Discord.** (#147)
Auto Setup answered "internal error" because the saved Quarantine role had been deleted in Discord: the PATCH got 404 and the whole setup stopped. Config keeps ids after the role or channel is gone (deleted by hand, or a backup restore).
*Avoid:* treat a 404 on a saved id as "missing" and make it again (`sections::create` checks an existing id first). Never let a 404 on reuse fail a whole setup.

**For bans the bot makes on staff's behalf, the audit actor is the bot, so checks must use the case moderator.** (Ban Sync review, 11-11)
Ban Sync held a ban for 60 s and dropped it when anti-nuke contained the audit actor. A `t!ban` or a dashboard ban is made with the bot's token, so the actor was always the bot, never the hacked moderator, and the check could never fire for staff commands. Root cause: the audit entry names who called Discord, not who asked the bot to.
*Avoid:* when the actor is the bot, take the moderator from the matching case (`cases.mod_id`, written by `apply_dm` within seconds) and check that person; with no case (an automatic ban), stop only for a nuke case or the quarantine of someone whose permit can ban, kick or quarantine since the ban. Any quarantine at all is too wide: Join Gate and verification quarantine raiders on join, so raiders could switch sharing off by getting themselves quarantined. Store the moderator with anything that acts later (`ban_sync_links.mod_id`), so the later step checks again. Searched for the same mistake: `antinuke::on_audit` skips the bot's own entries on purpose (staff commands are limited by the permit rate limit in `commands::gated` instead), and `verify::deleted::on_audit` treats the bot as the actor only for its own deletes; neither reads a staff member from a bot entry.

**Never mark a row done before the Discord call.** (Ban Sync review, 11-11)
The first Ban Sync copy set its row to `banned` before the ban, so a job that crashed or failed in between left a "banned" row with no ban, and the retry saw `banned` and stopped. Root cause: the claim that makes a step run once was used as the record that it happened.
*Avoid:* keep the row in its working state (`pending`) while the call runs, so a failure leaves it for the retry; on the retry, recognise your own earlier result in Discord (a ban whose reason starts with `Ban Sync from` counts as ours) and only then move the row on with `UPDATE … WHERE state = 'pending' RETURNING`; write the case only if the row has none yet. Mark a row skipped only for a refusal, never while the action exists. The Ban here button does the same and gives the row back to `alerted` when the ban does not happen. Searched for the same mistake: `reports::report` deletes its row when the log message fails, `joingate::punish` records after the action, lockdown uses `locking` → `locked` (section 2), and the quarantine row is written after the role change (above).

**A Discord outage is not a refusal.** (Ban Sync review, 11-11)
`commands::apply_dm` turned every failed Discord call into a refusal reply, a 502 too. A job that bans through it (a Ban Sync copy) then marked the row skipped and never retried, although the same request would have worked a minute later. Root cause: the error arm did not tell "Discord said no" (4xx) from "Discord did not answer" (5xx, timeout), and the doc comment ("Err only for infrastructure failures") already promised the second as `Err`.
*Avoid:* only a 4xx is a refusal (`http_err::refused`); anything else goes back as `Err`, so jobs retry and commands show "Something went wrong". Searched the other places that turn an error into reply text with `http_err::why`: `confirm`, `reports`, `appeals`, `slash` and `tickets::dm` answer a person, who can press again; `verify::deleted` sends the failure to the owner as its alert; and `joingate::punish` returns the error after its log, so the job retries.

**The dashboard acts as the bot.** (Raider List review, 11-11)
An unban from the dashboard (an accepted appeal, a revert) is made with the bot's token, so the audit entry names the bot. The Raider List read a bot-made unban as one of its own lifts and did not record that staff let the user back in, so the next removal or join acted on them again. Root cause: the same as "the audit actor is the bot" above, on the unban side.
*Avoid:* when the dashboard lifts something the bot may have done, record the staff choice in the same transaction (`tommybot_core::raiders::allow` in `appeals::accept_appeal` and `ops::revert`), and on the bot side take a bot-made unban's moderator from the newest unban case or action-log row by someone other than the bot in the last 5 minutes (`raiders::on_unban`). Searched for the same mistake: Ban Sync's `on_unban` takes any actor on purpose (a lifted copy is lifted whoever did it); anti-nuke skips the bot's own entries (above); MCP has no unban tool.

**Raw requests need a real User-Agent.** (live tests, 11-09)
Without `User-Agent: DiscordBot (...)` Cloudflare answers 403 / error 1010. Until 11-11 none of the bot's, dashboard's or MCP's raw Discord clients set one.
*Avoid:* every reqwest client that talks to Discord sets `DiscordBot (<repo url>, <version>)`.

**Twilight drops permission and user flag bits it does not know.** (#133)
Saving a bot role's old permissions loses bits newer than twilight. This is a known limit (marked `shortcut:` in the code). Upgrade twilight before relying on new bits.
User flags too: `UserFlags` is read with `from_bits_truncate`, so `public_flags` never holds Discord's undocumented spammer bit (`1 << 20`).
*Avoid:* read such a bit from the raw gateway JSON before twilight parses it, like `spam_flag` in `crates/bot/src/main.rs` (the loop reads raw messages instead of `next_event`).

## 2. Concurrency and restarts

**Unlock deleted its snapshot first.** (#97, #111)
Unlock deleted the snapshot, then aborted on the first error (a deleted channel or role). The server stayed half locked with nothing to retry from. A restart in the middle had the same effect.
*Avoid:* lock state is a row with `locking` → `locked` → `unlocking`, deleted only when the permissions are back. `lockdown::resume` finishes cut-off work at startup. A failed step is counted and the rest continue.

**Queue the undo before the action.** (#111)
A restart between a temp ban and its unban job made the ban permanent. Same for removing main roles on timeout and for timed locks.
*Avoid:* queue the undo job first, then act. A failed action leaves an undo that finds nothing and is dropped. Nuke reverts get a guard job that reruns after a crash.

**Events run concurrently.** (#100)
`main.rs` spawns a task per event, so messages already in flight after a trigger each added a strike. One burst could reach the strike cap and send several DMs, cases and logs.
*Avoid:* claim the punishment (`HeatState::claim`, one per 10 s). Anything that must happen once needs an atomic claim: `DELETE … RETURNING`, `WHERE status='open'`, `SKIP LOCKED`.

**Anti-nuke tripped once per event, not once per nuke.** (race review, 11-11)
Past the limit every audit entry returned `Quarantine` again, and each one is its own task, so a 20-channel nuke started about 16 quarantine-and-revert runs at once. They read the same unreverted rows, so the backup recreated the same deleted channel several times (the copy has a new id, so the next run still saw the original as missing), and opened as many cases and owner alerts.
*Avoid:* one run per (guild, actor) (`App::nuke_runs`). A later event only logs its row and asks the running run to revert once more; the run frees its claim in the same step that finds nothing new, so no action falls between two runs. The `nuke_revert` guard job is due after 120 s, and a run under rate limits can take longer: the scheduler skips a guard whose run is still alive, and the job path takes the same claim. An actor an earlier run already quarantined only gets the revert, with no second case or alert. `panic::start` undoes its claim and its category exemptions when its `panic_end` job cannot be saved; otherwise mod commands stay locked with nothing to end them. The same applies to panic mode: `panic::start` checks and inserts `panic_runs` in one step, or a second start saves the already exempted overwrites as the old ones.

**A run marked rows it never read.** (race review, 11-11)
`revert_actor` read the actor's rows, worked for seconds, then marked every row since `since` as reverted. What the nuker did meanwhile was marked too and never undone, not even by `/revert`.
*Avoid:* mark only the ids you read (`WHERE id = ANY($1)`). Never close work with a range condition when rows can arrive while it runs.

**Two locks at once saved a locked server as the snapshot.** (race review, 11-11)
`lock_for` checked for an active lock, then planned with several HTTP calls, then saved with `ON CONFLICT DO UPDATE`. Mention-spam lock calls it for every message past the limit, so a second call could plan after the first began applying, save locked permissions and overwrite the first snapshot. Unlock then restored the locked state.
*Avoid:* lock and unlock of a guild run one at a time (`lockdown::exclusive`). An in-process lock is enough here only because the dashboard queues jobs and never writes `lock_snapshots`; a second writer process needs an advisory lock (see the Channel Setup entry below).

**A gateway event and its audit entry run in their own tasks.** (race review, 11-11)
The ticket-category exemption read the category that the `ChannelCreate` task stores. When the audit task ran first, the lookup missed and a ticket bot's channels counted towards its limit.
*Avoid:* do not rely on the order of two events. `note_parent` stores every channel (also "no category"), so a miss means "not seen yet", and `parent_of` waits up to 0.5 s for it.

**Join-time steps race each other.** (sticky roles plan, 11-11)
A `MemberUpdate` (from verification, another bot or the bot's own role adds) can run before or alongside the `MemberAdd` task, and a late update can come from the membership before. Saving roles "per user id" would let such an update overwrite the saved roles before the rejoin reads them.
*Avoid:* key per-member state by membership: `member_roles` rows carry `joined_at`, an update writes only when its `joined_at` matches the row, and the rejoin claims the older row in one statement (`sticky::claim`), so a replayed `MemberAdd` gets nothing.

**Membership was checked when roles were claimed, not when they were added.** (sticky roles review, 11-11)
Sticky roles checked the member once, when the rejoin claimed the saved list, and then added roles later: from a job a minute on, after verification, or when a timeout ended. By then the member could have left and rejoined, been banned, quarantined or sent back to verification, and still got the old roles. Errors after the claim (a failed read, a refused add) dropped the claimed list, and a join where any step acted (even a Log hit) froze the row, so the next rejoin got roles from two memberships ago. Found the same gap in `timeout_roles::restore`, which gave main roles back to a quarantined member.
*Avoid:* every delayed add carries the membership it was claimed for (`joined_at`) and checks again right before the add: same `joined_at`, setting on, no quarantine row, not pending (`sticky::due`). Do fallible reads before a claim, and put whatever a claim took and could not finish into a job. A ban leaves a tombstone row instead of deleting, so a late event of the old membership cannot write it back.

**Member role adds share Discord's rate-limit bucket with punishments.** (sticky roles, 11-11)
A mass rejoin (after a raid, or a re-invite) giving every member's roles back would fill the per-guild member bucket that timeouts, kicks and bans need right then.
*Avoid:* bulk role or nickname work on members gets a per-guild cap (`sticky::take`, 20 adds a minute) and queues the rest as a job instead of dropping it.

**A switch checked in only one of two call sites.** (race review item 5, 11-12)
`on_audit` calls the quarantine hold from two places. Kicks, bans and timeouts checked `quarantine_hold`, role changes did not, so staff were still quarantined with the switch off.
*Avoid:* when a feature has a switch, grep every caller of its entry point and check the switch in each, or check it once inside the function. `hold()` now checks it inside, after the join-quarantine branch: an unverified joiner's role change is always undone (that punishes nobody), and only the punishment follows the switch.

**A join quarantine counted as a nuke target.** (race review follow-up, 11-11)
Verification's "quarantine on join" writes a quarantine row with no saved roles. The quarantine hold treated it like an anti-nuke quarantine, so an autorole bot giving a joiner its role was quarantined and lost its role permissions on every join.
*Avoid:* a row with empty `saved_roles` is a join quarantine. The hold undoes the role change but punishes nobody. Code that reads `quarantines` must tell the two kinds apart.

**Long work under a global lock.** (#100)
Similarity compared whole 4000-character messages (quadratic) while holding the global heat lock.
*Avoid:* bound the input (first 500 characters) and keep work under shared locks short.

**Listeners must survive a reconnect.** (#100)
The config listener died after one DB hiccup, so dashboard changes never applied until a bot restart.
*Avoid:* long-lived listeners restart in a loop and reload their caches after reconnecting.

**A failed log must not stop protection.** (#100, #133)
The timeout DM went out before the timeout worked, and a failed anti-nuke report stopped panic mode.
*Avoid:* fixed order Contain → Revert → Report → Panic. Logs and DMs are side effects; their failure never skips the next protective step.
The race review (11-11) found the same in Join Raid (a failed raid log or report job skipped the lockdown and every punishment, and one raider who had already left stopped the punish loop), in automod (the mention-lock log and lock came before the strike, the timeout log before the strike-cap lock) and in `warn_actions`. A loop that punishes several members logs a failure and goes on with the next.

The sticky roles review (11-11) found it in Join Gate too: a refused gate punishment (403 on kick) ended `on_join`, so the join raid check never ran for that joiner. Gate punishments now log and go on.

**A gateway event's value can be older than the bot's own write.** (auto slowmode review, 11-11)
Events run in parallel tasks and Discord sends a channel update for every edit (a rename too), so a ChannelUpdate handled after the bot's own PATCH can still carry the old slowmode. Auto slowmode read that value as "staff changed it" and put it back, which undid its own slowmode.
*Avoid:* don't decide from a field of the event when the bot itself writes that field. Read the live value (`GET channel`) and compare it with both the old and the bot's value (`slowmode::staff_value`), or skip the event when a later compare-and-set already handles it (the off job reverts only its own value). Also checked: `enforce_quarantine` and `catch::keep_top` act on event values too, but their writes are idempotent, so a stale event only repeats them.

**A claim row without its job blocked the channel for good.** (auto slowmode review, 11-11)
An `auto_slowmode` row is claimed with `ON CONFLICT DO NOTHING`. When its `slowmode_off` job gave up after its retries (Discord down, a missing permission retried until the last attempt), the row stayed and every later start found "already owned". Also: a failed start deleted row and job even when the PATCH might have landed, the not-calm re-queue made a job for a row staff had already removed, and a 403 was retried until the job gave up.
*Avoid:* a claim takes over a row with no pending job (`DELETE … WHERE NOT EXISTS (job)` in the claim's transaction, so a parallel claim conflicts instead of doubling it). Treat 50001/50013 as final (`http_err::forbidden_any`): end the claim and log it. Re-queue only while the row exists, under `FOR UPDATE`. Scope a job's rows to its own guild (`AND guild_id = $2`). A start that stops early because the channel already shows the auto value also gives a stuck row a new off job (`requeue_stuck`), since the takeover in the claim never runs then.

**Heat lives in memory.** (#111, accepted)
A restart resets running heat. Strikes and timeouts survive. This was left as is on purpose.

**A resumable run read its intent from the state it was changing.** (Channel Setup review, 11-11)
Push decided "this channel follows its category" from live Discord (synced with the category). Once the category's overwrites were pushed and a child's step failed, the child was no longer synced in live, read as an exception on the next push, and stayed public.
*Avoid:* a run that is resumed by recomputing a diff stores every decision derived from the old state before its first step (Push writes `reset` into the draft for each follower), and stops at the first failed step instead of running the rest (deletes included) against a half-changed server.

**A per-process flag does not stop a second writer.** (Channel Setup review, 11-11)
Push guarded itself with a static `HashSet` of guilds, so a second dashboard process could push at the same time, and a draft PUT or DELETE during a push was overwritten by the push's write-back. A stale PUT after a push brought back `new-*` keys that were already created, and the next push created them twice.
*Avoid:* hold a Postgres advisory lock for the whole run (`pg_try_advisory_xact_lock` in a transaction the run owns, so it is freed when the task ends or the process dies) and refuse other writers with 409 while it is held. Keys a run has used up are recorded (`next`) and refused when they come back. Run the steps in `tokio::spawn` and await the handle, so a dropped request does not stop a run half way; log what will happen before the first step and the result after, and record a failed save instead of returning with `?`.

**A dropped sqlx transaction kept the push lock after the answer.** (Channel Setup lock, 11-11)
`lock()` took `pg_try_advisory_xact_lock` in a sqlx transaction, and every handler let it drop on return. sqlx only queues the ROLLBACK on drop; it runs when the pooled connection is returned in a spawned task, so the lock outlived the response. The next PUT or Push that came in first got "a push is running" (about 1 in 60 runs of `draft_is_saved_discarded_and_pushed_only_when_safe`).
*Avoid:* a transaction that holds an advisory lock which others try-lock ends with an awaited `rollback()` or `commit()` on every path before the answer, early returns included (`release()` in `dashboard/src/channels.rs`; the whole push runs in one task that releases at the end). Drop is only the fallback for a cancelled request or a dead process. Checked the other `pg_advisory_xact_lock` users (`bot/settings`, `bot/tickets`, `dashboard/store.rs`, `core/cases.rs`): they wait for the lock instead of trying it, so a late ROLLBACK only delays the next writer by a moment and nothing is refused.

**No row lock across Discord calls.** (Raider List review, 11-11)
The Raider List lift held `SELECT … FOR UPDATE` on its action row while it unbanned, so a slow Discord held the lock for the length of the HTTP timeout, and a second run waited instead of finding the work done. Root cause: the lock was used to make the case and the notice happen once, which a claim does without waiting.
*Avoid:* read without a lock, make the Discord call (it must be safe to repeat: the unban checks the ban is still the list's), then claim with `DELETE … ` and act on `rows_affected() == 1`; only the claimer writes the case and the log. A lift that gives up after its last try posts "Raider List removal failed" to the mod log (`raiders::lift_gave_up`). Searched for the same pattern: `appeals` decide locks one appeal row across its unban (a person's click, left as is), and `backup::track_created` locks the newest backup across one Discord GET for its read-modify-write (left as is: it needs the lock and the GET is short).

**Ban Sync's hourly cap was counted outside the row's transaction.** (Ban Sync, count-then-insert sweep 11-11)
`share_job` and `import_job` each counted the users shared in the last hour, then wrote rows. A share and an owner-started import running at once both saw room and went past the 30 per hour cap.
*Avoid:* `offer` counts and writes in one transaction under `pg_advisory_xact_lock(hashtextextended('ban_sync_cap:' || source, 0))`, and says whether it hit the cap. Send the rest stays outside the cap on purpose (the owner's choice). The general rule is "A count checked a cap, then a second statement inserted" (#183).

**A button read its state, acted, then marked it done.** (Raider List, full-codebase check 11-11)
The Raider List alert's Ban/Kick/Quarantine read `action = 'alert'`, ran the punishment and only then set the row. Two staff pressing at once both passed the read: two cases, two DMs, two mod logs, or a ban and a kick.
*Avoid:* claim first with `UPDATE ... WHERE action = 'alert'` and act only on `rows_affected() == 1`; put the row back when nothing was done. Searched: Ban Sync's Ban here already claims first; appeals decide claims after the unban, but an unban is safe to repeat and the case and log come after the claim, so it was left as is.

**Advisory locks on raw ids share one key space.** (Raider List review, 11-11)
`report` locked the reporting owner's user id and then `add` locked the reported user's id, both as plain `pg_advisory_xact_lock(id)`. Owner A reporting B while owner B reports A took the same two keys in opposite order and Postgres killed one as a deadlock.
*Avoid:* give each kind of lock its own key, `pg_advisory_xact_lock(hashtextextended('<kind>:' || id, 0))` (as `raider_user:`, `raider_owner:`, `channel_drafts:` do). Searched: the other raw locks (`cases::create`, ticket numbers, config saves) all take one guild id per transaction, so they cannot cross; left as is.

**"It is gone, so an earlier try of mine removed it" needs proof.** (Raider List review, 11-11)
The lift took a missing ban to mean an earlier try had unbanned, and wrote an unban case in the bot's name even when staff had unbanned by a path the list never saw.
*Avoid:* record the intent (`raider_actions.unbanning = true`) just before the Discord call, and only a retry that finds that mark writes the missed case.

**A count checked a cap, then a second statement inserted.** (Jev full check, 11-11)
`token_create` counted live MCP tokens and inserted in a separate statement, so parallel requests at 9 live tokens all saw room (the test made up to 20 against a cap of 10). `propose` (open MCP changes, each one a DM to the owner) and message reports (10 per reporter an hour; each interaction is its own task) had the same gap. A single `INSERT … SELECT … WHERE (SELECT count(*)) < n` does not help under READ COMMITTED: parallel statements do not see each other's uncommitted rows.
*Avoid:* count and insert in one transaction under `pg_advisory_xact_lock(hashtextextended('<kind>:' || key, 0))` (`mcp_tokens:` and `mcp_writes:` are global, `reports:` is per guild and reporter), taken right before the insert and never held across a Discord call. A unique index is enough when it already lets only one win (`appeals_one_open`). Searched every `count(*)`, `exists(` and `.len() >=` that gates an insert: the appeals daily limit is covered by its unique open index; the Raider List report cap, ticket numbers and config saves were already locked; the Ban Sync hourly cap is counted inside sequential jobs (an owner's import can overlap a share job by a few, left as is); the in-memory caps (rescue claims, verification remakes, 2FA fails) check and push under one Mutex.

## 3. Automod

**Three throwaway servers are three owners.** (Raider List review, 11-11)
Strong evidence from 3 different server owners listed a user by itself. Anyone can make 3 Discord accounts, give each a new server and invite TommyBot, then get any user listed and banned in every server that enforces the list. Root cause: "different owners" was taken as "independent witnesses" without asking what an owner costs.
*Avoid:* evidence keeps its source server's member count and the time the bot joined it (`Detail.members`, `Detail.bot_joined`), and only servers with `RAIDER_MIN_MEMBERS` members that had the bot for `RAIDER_MIN_DAYS` count; a bot account never lists by itself; and an automatic listing only alerts until the bot owner verifies it (`UNVERIFIED_ALERT_ONLY`). One owner's raid or spam detection makes no entry at all until a second owner flagged the user. Searched for other counts of sources: Ban Sync counts the users a source shares (a rate limit, not a vote), and the dashboard's owner counts only sort the review queue.

**Filters only saw plain text.** (#101)
Links in `[x](https://..)`, `||spoilers||` or `**bold**`, lookalike letters, zero-width characters, spaced letters (`f u c k`), forwards, polls, user-installed app replies, display names and voice-channel prefixes all got past the filters.
*Avoid:* filter on `filters::urls` (finds links anywhere) and `filters::word_forms` (cured text). When Discord adds a new place for user text, run it through the instant filters too.

**decancer breaks `String += &String`.** (#101)
Its `AddAssign` impl makes `s += &other` ambiguous once decancer is in scope, and the code stops compiling.
*Avoid:* use `push_str`.

**Exemptions that were too wide or too narrow.** (#100)
Whitelisted channels and roles still got base message heat, other bots' slash-command replies counted as webhook spam, and saying "gg" twice in a quiet channel gave a timeout.
*Avoid:* every new heat source checks the whitelist, and quiet-channel heat counts once per user. Add a test with both spammers and normal chatters in one channel.

**A bot rename reads as a new name.** (auto-dehoist, 11-11)
The post-join username filter dedupes on username + global name + nick. Its Rename action changes the nick, so the echo update looked like a new bad name and the bot would rename on every update; a dehoist nick on a member the filter already punished would punish them again.
*Avoid:* for Rename the dedupe key leaves out the nick, and a clean nick counts as done (`joingate::post_join_key`). Auto-dehoist skips a name the filter matched, and its output always starts with a letter or digit, so dehoisting twice changes nothing.

**The verify button judged "suspicious" on other names than Join Gate.** (spammer-flag PR, 11-11)
Join Gate checks the cured username and display name; the verification Start button checked only the raw username. A member caught by the username filter at join (lookalike letters or the display name) was then let through without a captcha in "suspicious only" mode.
*Avoid:* build the name for `is_suspicious` with `joingate::names`. Discord's spammer flag has the same gap: only the join frame carries it, so the Start button asks Discord (`Api::public_flags`) when the flag would turn a Grant into a challenge, and a failed lookup means a captcha.

## 4. Parsing and limits

**Byte slicing panics on non-ASCII.** (e7edf56)
Duration parsing sliced off the last character by byte and panicked on input like `5å`. Converting large values to milliseconds overflowed.
*Avoid:* split on `char` boundaries (`chars()`, `char_indices()`), use checked or saturating maths. Never `&s[..n]` on user input.

**Limits drifted between places.** (#134)
The prefix panels, the dashboard and the save path each had their own bounds, and an out-of-range value was changed silently.
*Avoid:* all bounds live in `core::limits` and are used everywhere. A value out of range is refused with a message.

**Heat max and decay had no range outside the dashboard.** (auto slowmode review, 11-11)
Only the dashboard's `heat_ok` checked them, and it let decay 0 through. The `heat` prefix panel took any number (`4 1000` saved a decay of 10, so `(1 - 10)^t` made every heat NaN; `3 0` made every message trip), and the bot used the values as read. Decay 0 meant heat never cooled, so an auto slowmode channel was never calm. Panic mode's `heat_decay` had no range either.
*Avoid:* `HEAT_MAX` and `HEAT_DECAY` live in `automod::heat` and go through `core::limits::float_for_path` (dashboard, import and MCP saves, the prefix panel's `Kind::Num` rows), and the bot clamps when it reads (`HeatCfg::clamped` in `AutomodCfg`'s deserializer, the panic decay in `add_heat`). A number the bot divides by, raises to a power or waits on gets a range in `limits` and a clamp on read.

**A URL checked by prefix lets look-alike hosts through.** (MCP security review, 2026-11-10)
The MCP OAuth registration took any `https://` redirect and checked localhost with `starts_with("http://localhost")`, which `http://localhost.evil.com` also passes. A stranger could register their own site, send the owner a login link and get the code, and with it a token for everything the MCP reads.
*Avoid:* parse the URL (`reqwest::Url`) and compare scheme and host exactly against an allow list. OAuth redirects go only to known clients (claude.ai) or the owner's own computer.

**A refresh token that issues a fresh one with a full lifetime never expires.** (MCP security review, 2026-11-10)
Each MCP refresh gave a new 30-day refresh token, so a stolen one could be renewed forever and only rotating `MCP_OAUTH_SECRET` stopped it.
*Avoid:* carry the login time (`auth`) through every refresh and cap the new token at login time plus the limit.

## 5. Database and migrations

**Two PRs used the same migration number.** (#64/#65)
Git merged them without a conflict, then sqlx failed every test with "duplicate key _sqlx_migrations_pkey".
*Avoid:* parallel work gets its own number range (see the handover notes). Before merging several PRs, `ls crates/core/migrations` and look for duplicate prefixes.

**Old migrations are frozen.** (#112)
sqlx stores a checksum, so editing an applied migration breaks every existing database.
*Avoid:* never edit an old migration, not even a comment. Write a new one.

**A default change must not overwrite user choices.** (#133, migration 2200)
*Avoid:* a migration that changes a default only moves rows that still hold the old default (600 → 60) and leaves other values alone. Test it from an empty DB and from the old schema with data.

**A new migration file was not picked up.** (MCP views, migration 2800)
`sqlx::migrate!` embeds the migrations at compile time, and adding a file does not make cargo rebuild `tommybot-core`. Tests then failed with "relation mcp_... does not exist".
*Avoid:* after adding a migration, `touch crates/core/src/db.rs` before testing. CI builds clean and is not affected. The same happened after `git merge` brought in a changed `RaidCfg` (MCP PR 4): two schema tests failed against the old build until `touch crates/automod/src/join.rs`. If a test fails right after a merge on code you did not touch, touch the changed crate's file and run it again before you debug.

**A table stored personal data that PRIVACY.md did not list.** (VPN block, 11-12)
`verify_ips` (salted IP hashes from web verification, kept 30 days) had no row in PRIVACY.md.
*Avoid:* a migration that stores anything about a user adds its row to PRIVACY.md (what, why, how long) in the same PR.

**Parallel worktrees shared one target and one DB.** (11-01)
Cargo mixed crates from different worktrees, and one branch's migration broke the others' tests. Four targets (about 7 GB each) filled the disk.
*Avoid:* one `CARGO_TARGET_DIR` and one database per worktree (`su postgres -c "createdb X"`; `createdb -h localhost` hangs on a password prompt). Delete finished targets.

**Saving a note without a change was stored as an edit.** (11-11)
`notes::edit` always wrote a `note_edits` row and both callers logged a Modify, so pressing Save twice gave a "x → x" history line and a log.
*Avoid:* an edit that changes nothing writes nothing (`old.text <> $3` in the SQL) and the callers skip the log when `old == text`.

## 6. Dashboard

See [crates/dashboard/ui/PITFALLS.md](crates/dashboard/ui/PITFALLS.md) for the two worst ones (#143): a `>` in a `view!` attribute ends the tag, and reading a signal after its page is gone crashes the whole dashboard. Also:

- **List closures that call `.get()`** rebuild every child on each change and kill CSS transitions (#99). Read per element with a `Memo` and `class:`.
- **The UI crate is not in the cargo workspace.** `cargo clippy` at the root does not check it. Run `cargo clippy --target wasm32-unknown-unknown -- -D warnings` in `crates/dashboard/ui`.
- **The debug wasm is about 100 MB** and makes the page close silently in Playwright. Use `trunk build --release` for screenshots and the e2e sweep.
- **A refused save vanished with the toast** (8d72d50). An error the user must fix stays up until it is fixed or clicked away.
- **Auto Setup's steps read their state from the toast text** (#151). The toast closes itself after 3 s, so "Running" and "Done" vanished and the steps never stayed green. *Avoid:* keep a run's state in its own signal and show the result in the page; the toast only announces it.
- **Check access before you look at the body.** `put_section` and `import` validated the input before the level check, so someone without access learned the shape of a section from the error. Call `require` first, then validate.
- **A section struct without `#[serde(default)]` breaks the first save.** `RaidCfg` had required fields, so turning Join Raid on for a server that never saved it was refused ("missing field `suspicious_only`"). A config struct with a `Default` impl gets `#[serde(default)]` on the struct.
- **Pages everyone may open still need greyed write buttons.** Lockdown was `Full` for viewers, and History's Roll back was enabled for everyone; the server refused, but the buttons looked usable The same held for Overview's Fix buttons and system switches.
- **A number field without a schema default must be optional in the config struct.** The form saves an emptied field as its default, or `null` without one. Join Raid's `id_margin_ms` is a plain `i64`, so emptying it made every save fail. `schema::tests::number_fields_without_default_accept_null` checks every field, and `one_field_alone_deserializes` checks every section loads with one field set. Stepping an optional field back to 0 empties it: Join Raid's ID flag at 0 would flag every joiner in a raid. Give a page whose main action is a write `need(level)` in `access::page`, or disable the button with `Access::rank()`.
- **A user choice kept only in the DOM is lost on reload.** The light/dark toggle only set `data-theme`, so a reload went back to dark. A choice the user makes in the UI is saved (here in `localStorage`, read again in `main()`).
- **Every bug from the deep test of 2026-11-10** is listed with its cause in `tests/e2e/dashboard-deep-test-2026-11-10.md`; the UI ones (hover contrast, double page load, copy button, null number fields, `.row label` on the Import button) are in the UI file, sections 3 to 7.
- **`tests/e2e/deep.js`** checks every write route for seven access levels, every page as every level, the owner's number fields, toggles, save and cancel, the server switcher, 2FA, Auto Setup, toasts, theme, mobile and hover contrast.
- **The backup view sorted channels by category id, not position.** `(parent or own id, kind, position)` put categories (and loose channels) in snowflake order, so the tree did not match Discord, and matching children by category name broke when two categories share a name. *Avoid:* sort by `(voice, position, id)` and group children by `parent_id` (sent as a string: snowflakes lose precision as JS numbers).
- **A dropdown option's `selected=` stops working once the user has picked** (#158). The attribute only sets the default; after a pick the browser keeps the user's choice. Cancel reset the setting and the preview, but the dropdown still showed the old pick. *Avoid:* use `prop:selected=` (the live property) on every `<option>`, and `prop:value`/`prop:checked` on inputs. Fixed in all 16 options in `form.rs`, `embeds.rs` and `lockdown.rs`; no plain `selected=`/`checked=`/`value=` bindings are left. The e2e sweep picks a channel on Logging, presses Cancel and checks the dropdown; it waits for the page to load first, because a pick made before the config arrives is overwritten by it.
- **Role names from Discord already start with `@` for @everyone** (#158). The dashboard added its own `@` and showed `@@everyone`. *Avoid:* trim a leading `@` before adding one. Fixed in `form::items` (every role list and picker) and the `md.rs` role mentions; `recovery.rs` and the wizard's ticket pings already did. The bot itself never writes a role name after `@`.
- **The toast looked like two pieces.** A coloured `border-left` on a rounded box curves at the corners, and the absolutely placed progress strip starts inside that border, so the two met as a hook. *Avoid:* one colour mark per box. Put strips and edges inside an `overflow:hidden` box with `left:0; right:0` and no border on that side. Also `cargo fmt` in `crates/dashboard/ui` reformats unrelated files; revert them before committing.

- **Editing or deleting a case left no trace.** (Mod stats, 11-11) Anyone with Manage Cases could rewrite or close a case and a trusted admin could delete one, and nothing recorded who did it, so a staff member could hide their own history. `case_delete` also answered OK for a case that did not exist. *Avoid:* a change that can erase moderation history writes an append-only row (`cases::record`, kind `case_edit`/`case_delete`) in the same transaction, and a delete of nothing answers 404. Searched for other erasing routes: note edits already keep history (`note_edits`), report and appeal status changes keep `handled_by`/`decided_by`; the Revert Center and purge are owner/admin actions already logged.
- **An Admin-only check let every trusted admin through a permit-gated page.** (Mod stats, 11-11) `api::staff` and `access::page`'s `has()` treat Admin as holding every permit, which is wrong for a permit meant to be owner-granted (View Mod Stats). *Avoid:* such a permit gets its own check (`access::mod_stats`) placed before `has()`, `page_access` fetches permit perms for trusted admins too, and `keep_exemptions` stops non-owners from changing who holds it on save, rollback and import.
- **A missing `enabled` key was read as on.** (UI review, 11-11) Overview's switches guessed their own defaults and showed Anti Nuke and Join Raid on for a server that never saved them, while the bot, the security score and the pages read them as off. The settings pages only ringed cards red when `enabled` was literally `false`, so a system that was never switched on showed no "Not in use" ring. *Avoid:* read a missing switch as the bot does (the schema default, off for every system); never keep a second copy of a default in the UI.
- **The 2FA check ran before the DM code.** (MCP writes, PR 4, caught in review) `confirm` checked the TOTP code first, so anyone holding an MCP token could send wrong TOTP codes and lock the owner out of dashboard 2FA for 10 minutes. *Avoid:* when a request carries several secrets, check the one the caller must already hold (the DM code) first, and touch shared lockout counters only after it passes.
- **An action ran something other than what was reviewed.** (Channel Setup review, 11-11) Push ran the draft stored at that moment, which another editor (or a late save) could have changed after the preview. *Avoid:* every reply carries a version of what it shows (`updated_at`), the action sends it back, and a mismatch is refused with 409.
- **An approval showed less than it approved.** (Security review, 11-11) The MCP code DM cut the change at 1500 characters, so the end of a long change was approved unseen. The same class: `embed_post`'s DM showed only `{repost}` and not the embed it posts, a rollback's preview diffed the permits copy in the last config version instead of the live tables (a rescue-claimed Extra Owner was removed without showing), and the embed could be edited between the DM and the code. A `target` on an action without `{t}` in its path was never checked but went into the DM's inline code, so backticks and a fake block could reshape it (found in an edge-case check of #169). *Avoid:* an approval shows all of what runs or refuses (propose says "split it"); drop input an action does not use before it reaches the approval; preview what the action really reads (the stored embed, live permits); store a hash of what was shown and refuse at confirm (`stale`) when a fresh preview differs (migration 3100 `preview_hash`).
- **A check before a slow call did not hold for the write after it.** (Security review, 11-11, after #180) 2FA enrollment checked "not confirmed", sent the DM, then upserted the secret with no condition; a confirm during the DM had its secret replaced while staying confirmed, so a stolen session could end up holding the owner's 2FA. Confirm had the mirror race: it marked confirmed whatever secret was stored by then. *Avoid:* put the precondition in the write itself (`do update ... where not confirmed`, `update ... where secret = <the one checked>`) and treat 0 rows as refused. Searched the other upserts: the rescue-key regenerate replaces on purpose, the rest guard nothing.
- **A host allow list let any page on that host catch an OAuth code.** (Security review, 11-11) MCP OAuth took any path on claude.ai/claude.com, and a client id signed under that rule stays valid for 10 years. *Avoid:* compare host and path exactly (`REDIRECTS`), no query or port, and check the redirect again at `/authorize`, not only at registration.
- **An empty list switched an authorization rule off.** (Channel Setup review, 11-11) "Only owners may hide a channel from staff" compared against the staff roles; a server without roles on the Admins and Moderators permits had none, so nothing ever counted as hidden and anyone could do it. *Avoid:* decide what an empty list means for the rule (here: every newly limited channel hides it from staff) and say in the refusal how to fix it.
- **A filter meant for display also weakened the check.** (Channel Setup review, 11-11) Member overwrites were left out of the issue list so they would not make rows pulse, which also let a dangerous member overwrite through Push. *Avoid:* filter for display where it is shown; the list that decides keeps every row.
- **A picker's filter is not validation.** (Channel Setup review, 11-11) The UI left the quarantine and main roles out of the role picker, but the server took any role id, and the UI's own list still offered roles the server would refuse. *Avoid:* the server validates with the same rule (`Ctx::pickable`) and sends the allowed ids, and the UI offers only those.
- **The same rule written twice drifts.** (Raider List review, 11-11) The dashboard's Report button had its own copy of the bot's report checks and missed some: permit holders, a quarantined moderator's saved roles, a server that opted out, and putting a rejected entry back to pending. *Avoid:* a rule both processes enforce lives once in `tommybot_core` (`raiders::report`, `raiders::not_collectable`, `raiders::allow`); each side only looks up what only it can reach (Discord) and passes it in. Searched: the dashboard's staff and permit checks already call `tommybot_core::permits`; the appeal rules live only in the dashboard.
- **A body extractor answers before the handler's own check.** (MCP writes, PR 4) The "not through the tunnel" check sat inside the handler, after `Json(..)`, so a request without a JSON body got 415 instead of 404 and the route showed it exists. *Avoid:* a check that must refuse before anything else is a `route_layer` middleware, not the first line of the handler.

- **Writing into Discord JSON by position panicked after the decision was saved.** (Jev full check, 11-11) Appeals `close_log` added the Decision field with `embeds[0]["fields"].as_array_mut()`. Writing through `v[0]` panics when `v` is not an array or is empty, and writing through `v["k"]` panics when `v` is a string, number, bool or array. A log message with no embed panicked after `decide` had committed, so the member never got the verdict DM. *Avoid:* to change JSON from outside (Discord replies, stored documents, user input), use `get_mut(i)` / `as_object_mut()` / `entry(..)` and do nothing on a wrong shape; reading with `v["x"]` never panics. Side work like `close_log` must not be able to stop what comes after it. Searched every write by index or key in `crates/*/src` and the UI: config writes are safe because `put_section` and import only store objects that parse into their typed sections. The UI embed editor's `set_at` also panicked on a stored `"image": "url"` (the server check only reads `/image/url`) and now replaces a non-object parent with `{}`. The Decision field itself could pass Discord's 1024 limit ("Denied and blacklisted by <@id>" plus a 1000-character reply), which made Discord refuse the edit and left the buttons on the log; both the dashboard and the bot's button path now cut it to 1024 and skip a 26th field.

## 7. Build and deploy

**Trunk's prebuilt wasm-bindgen needs a newer glibc.** (#123)
On the arm64 VPS `trunk build` failed with `GLIBC_2.38 not found`.
*Avoid:* the Dockerfile builds `wasm-bindgen-cli` itself at the version the UI lockfile resolves to. Don't hardcode the version: the UI lockfile is gitignored.

**containerd deletes an untagged image.** (#128)
Rollback by image id failed because the old image was already gone.
*Avoid:* the updater keeps the previous version under a `:rollback` tag.

**The updater does not update itself.** (#128)
Changes to `docker-compose.yml` or the updater need `git pull && docker compose up -d` on the VPS once.

**No logs without `RUST_LOG=info`.** (#92)
The bot prints nothing useful at the default level. `live.sh` and compose set it.

**A new RustSec advisory turns the `audit` job red.** (#115)
Fix with `cargo update -p <crate>`. Ignore an advisory in `.cargo/audit.toml` only with a reason, as for RUSTSEC-2023-0071 (rsa only via the unused sqlx-mysql).

**DEPLOY.md named a menu that does not exist.** (DEPLOY 6.4) It said **MCP** in the top bar; the link is **MCP tokens** in the account menu, so the owner could not find it. *Avoid:* when a guide names a button or menu, check its label and place in `crates/dashboard/ui/src/main.rs` (header) or the page itself. The rename missed "new MCP page" in CHANGELOG.md (found in the docs round after #166): grep every .md, CHANGELOG and README included, for the old label.

## 8. Tests

- **Defaults are off on new servers.** Heat filters and anti-nuke start off, so a live scenario must turn on what it tests (#92, #93).
- **Bots are exempt from a lot.** Webhooks owned by TommyBot or by a verified bot are exempt, bot-authored commands are ignored and bots cannot press buttons. Coordinated spam, confirm buttons, appeals, reports, Join Gate and DM tickets need a second human account (#93, #142).
- **The live test must not use the hosted bot's token or server.** (11-11) The cloud env had the VPS bot's `DISCORD_TOKEN` and its only guild as `TEST_GUILD_ID`, so `live.sh` and the VPS bot would both react to every attack and the result says nothing. *Avoid:* a separate test bot (own Discord application) on a separate test server, or stop the VPS bot (`docker compose stop bot`) for the run.
- **The live script shares the bot's token** and its rate limits. Cache channel and role lookups and wait longer instead of polling hard (#142).
- **Make scenarios clean up after themselves** so they can run again. The old nuke scenario kicked the attacker bot every run (#142).
- **Cloud env vars are read at session start.** A variable added during a session needs a new session.
- **Two DB tests used the same guild id.** (MCP writes, PR 4; again in the race review, where `6_100_000_000` was already used twice in the bot crate) Both took `9_300_000_000 + pid`, ran at the same time in one process and saw each other's config versions, so `history_keeps_newest_200_versions` failed now and then. *Avoid:* give each DB test its own base (`grep -rn '_000_000_000 +' crates/` before picking one) and delete its rows at the end.
- **An e2e step waited for any row, not the one it made.** (11-11) The notes check waited for "a note is listed", which old notes from an earlier run already met, then the reload that put the new note on top swapped the card under the fill, so Save sent the old text. *Avoid:* wait for the exact value the step created.
- **An e2e step waited for a dropdown, not its options.** (11-11, PR #178 CI) The Logging Cancel check waited for the channel select, which shows with only "none" until the channel list loads, so it found no other option and crashed. *Avoid:* wait for the option the step will pick (`option:nth-child(2)`). Searched: the only other `selectOption` (log setup, by index) waits for its option already.
- **`tests/e2e/run.sh` needs root Postgres, trunk and Playwright**; the gateway is not covered by the Discord mock.
- **A global cleanup in one test deleted another test's rows halfway through its setup.** (PR #183 CI, 11-11) `raiders::expire` deletes every old rejected or pending entry in the database. Test 55 created its old entries first and their evidence after, so test 54's `expire()` running in parallel could delete an entry before its evidence existed; that evidence was then orphaned and the count read 2 instead of 1. *Avoid:* in a test that calls a job working on the whole table, create the rows such a parallel run could act on last, so every state it can see is already complete. Searched: `expire` is the only global cleanup called from the raider tests (54 and 55).

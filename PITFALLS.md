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
*Avoid:* every client gets a timeout (15 s in `automod/src/discord_api.rs`). Twilight already handles 429.

**A DM can be slow or refused.** (#136, #142)
A DM before a punishment must not hold the punishment back, and a DM to a bot is refused.
*Avoid:* send DMs through the shared helper with the `DM_WAIT` limit (3 s). The punishment runs anyway. A DM that times out is logged.

**Discord's own rendering quirks.** (5ec560a, #92)
A mention of the @everyone role renders as `@@everyone`, so write plain `@everyone`. Discord lists each mentioned user once per message, so mention spam counts one ping per user per message. Discord merges audit log entries, so the bot's deletion of someone else's message can show no Responsible.

**A saved id can point at something deleted in Discord.** (#147)
Auto Setup answered "internal error" because the saved Quarantine role had been deleted in Discord: the PATCH got 404 and the whole setup stopped. Config keeps ids after the role or channel is gone (deleted by hand, or a backup restore).
*Avoid:* treat a 404 on a saved id as "missing" and make it again (`sections::create` checks an existing id first). Never let a 404 on reuse fail a whole setup.

**Raw requests need a real User-Agent.** (live tests, 11-09)
Without `User-Agent: DiscordBot (...)` Cloudflare answers 403 / error 1010.

**Twilight drops permission bits it does not know.** (#133)
Saving a bot role's old permissions loses bits newer than twilight. This is a known limit (marked `shortcut:` in the code). Upgrade twilight before relying on new bits.

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

**Heat lives in memory.** (#111, accepted)
A restart resets running heat. Strikes and timeouts survive. This was left as is on purpose.

**A resumable run read its intent from the state it was changing.** (Channel Setup review, 11-11)
Push decided "this channel follows its category" from live Discord (synced with the category). Once the category's overwrites were pushed and a child's step failed, the child was no longer synced in live, read as an exception on the next push, and stayed public.
*Avoid:* a run that is resumed by recomputing a diff stores every decision derived from the old state before its first step (Push writes `reset` into the draft for each follower), and stops at the first failed step instead of running the rest (deletes included) against a half-changed server.

**A per-process flag does not stop a second writer.** (Channel Setup review, 11-11)
Push guarded itself with a static `HashSet` of guilds, so a second dashboard process could push at the same time, and a draft PUT or DELETE during a push was overwritten by the push's write-back. A stale PUT after a push brought back `new-*` keys that were already created, and the next push created them twice.
*Avoid:* hold a Postgres advisory lock for the whole run (`pg_try_advisory_xact_lock` in a transaction the run owns, so it is freed when the task ends or the process dies) and refuse other writers with 409 while it is held. Keys a run has used up are recorded (`next`) and refused when they come back. Run the steps in `tokio::spawn` and await the handle, so a dropped request does not stop a run half way; log what will happen before the first step and the result after, and record a failed save instead of returning with `?`.

## 3. Automod

**Filters only saw plain text.** (#101)
Links in `[x](https://..)`, `||spoilers||` or `**bold**`, lookalike letters, zero-width characters, spaced letters (`f u c k`), forwards, polls, user-installed app replies, display names and voice-channel prefixes all got past the filters.
*Avoid:* filter on `filters::urls` (finds links anywhere) and `filters::word_forms` (cured text). When Discord adds a new place for user text, run it through the instant filters too.

**decancer breaks `String += &String`.** (#101)
Its `AddAssign` impl makes `s += &other` ambiguous once decancer is in scope, and the code stops compiling.
*Avoid:* use `push_str`.

**Exemptions that were too wide or too narrow.** (#100)
Whitelisted channels and roles still got base message heat, other bots' slash-command replies counted as webhook spam, and saying "gg" twice in a quiet channel gave a timeout.
*Avoid:* every new heat source checks the whitelist, and quiet-channel heat counts once per user. Add a test with both spammers and normal chatters in one channel.

## 4. Parsing and limits

**Byte slicing panics on non-ASCII.** (e7edf56)
Duration parsing sliced off the last character by byte and panicked on input like `5å`. Converting large values to milliseconds overflowed.
*Avoid:* split on `char` boundaries (`chars()`, `char_indices()`), use checked or saturating maths. Never `&s[..n]` on user input.

**Limits drifted between places.** (#134)
The prefix panels, the dashboard and the save path each had their own bounds, and an out-of-range value was changed silently.
*Avoid:* all bounds live in `core::limits` and are used everywhere. A value out of range is refused with a message.

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

- **A missing `enabled` key was read as on.** (UI review, 11-11) Overview's switches guessed their own defaults and showed Anti Nuke and Join Raid on for a server that never saved them, while the bot, the security score and the pages read them as off. The settings pages only ringed cards red when `enabled` was literally `false`, so a system that was never switched on showed no "Not in use" ring. *Avoid:* read a missing switch as the bot does (the schema default, off for every system); never keep a second copy of a default in the UI.
- **The 2FA check ran before the DM code.** (MCP writes, PR 4, caught in review) `confirm` checked the TOTP code first, so anyone holding an MCP token could send wrong TOTP codes and lock the owner out of dashboard 2FA for 10 minutes. *Avoid:* when a request carries several secrets, check the one the caller must already hold (the DM code) first, and touch shared lockout counters only after it passes.
- **An action ran something other than what was reviewed.** (Channel Setup review, 11-11) Push ran the draft stored at that moment, which another editor (or a late save) could have changed after the preview. *Avoid:* every reply carries a version of what it shows (`updated_at`), the action sends it back, and a mismatch is refused with 409.
- **An approval showed less than it approved.** (Security review, 11-11) The MCP code DM cut the change at 1500 characters, so the end of a long change was approved unseen. The same class: `embed_post`'s DM showed only `{repost}` and not the embed it posts, a rollback's preview diffed the permits copy in the last config version instead of the live tables (a rescue-claimed Extra Owner was removed without showing), and the embed could be edited between the DM and the code. A `target` on an action without `{t}` in its path was never checked but went into the DM's inline code, so backticks and a fake block could reshape it (found in an edge-case check of #169). *Avoid:* an approval shows all of what runs or refuses (propose says "split it"); drop input an action does not use before it reaches the approval; preview what the action really reads (the stored embed, live permits); store a hash of what was shown and refuse at confirm (`stale`) when a fresh preview differs (migration 3100 `preview_hash`).
- **A host allow list let any page on that host catch an OAuth code.** (Security review, 11-11) MCP OAuth took any path on claude.ai/claude.com, and a client id signed under that rule stays valid for 10 years. *Avoid:* compare host and path exactly (`REDIRECTS`), no query or port, and check the redirect again at `/authorize`, not only at registration.
- **An empty list switched an authorization rule off.** (Channel Setup review, 11-11) "Only owners may hide a channel from staff" compared against the staff roles; a server without roles on the Admins and Moderators permits had none, so nothing ever counted as hidden and anyone could do it. *Avoid:* decide what an empty list means for the rule (here: every newly limited channel hides it from staff) and say in the refusal how to fix it.
- **A filter meant for display also weakened the check.** (Channel Setup review, 11-11) Member overwrites were left out of the issue list so they would not make rows pulse, which also let a dangerous member overwrite through Push. *Avoid:* filter for display where it is shown; the list that decides keeps every row.
- **A picker's filter is not validation.** (Channel Setup review, 11-11) The UI left the quarantine and main roles out of the role picker, but the server took any role id, and the UI's own list still offered roles the server would refuse. *Avoid:* the server validates with the same rule (`Ctx::pickable`) and sends the allowed ids, and the UI offers only those.
- **A body extractor answers before the handler's own check.** (MCP writes, PR 4) The "not through the tunnel" check sat inside the handler, after `Json(..)`, so a request without a JSON body got 415 instead of 404 and the route showed it exists. *Avoid:* a check that must refuse before anything else is a `route_layer` middleware, not the first line of the handler.

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
- **`tests/e2e/run.sh` needs root Postgres, trunk and Playwright**; the gateway is not covered by the Discord mock.

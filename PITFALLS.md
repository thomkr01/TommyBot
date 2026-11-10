# Pitfalls

Technical mistakes that already happened in TommyBot and were fixed. Each entry says what went wrong, why, and how to avoid it next time. Read the section for the area you change before you start. Add a new entry when you fix a bug whose cause could happen again.

Dashboard UI (Leptos) details are in [crates/dashboard/ui/PITFALLS.md](crates/dashboard/ui/PITFALLS.md).

## 1. Discord API and permissions

**A bot's managed role cannot be removed.** (#58, #133, #142)
Quarantining a nuking bot failed, because Discord does not let anyone take a bot's own managed role away, and that role kept Administrator. #142 found the same hole again in `quarantine_actor` (verification channel delete, command rate limit), which skipped the shared path.
*Avoid:* every place that contains an actor goes through `antinuke::contain()`. It sets the managed role's permissions to 0, then quarantines, and kicks only as a fallback. Never write a second quarantine path.

**Role order decides everything.** (#57, #133)
The first live nuke test got 403 because TommyBot's role was tied with the attacker's. A role between the bot and the quarantine role also breaks quarantine.
*Avoid:* the bot role must sit above every role it acts on, and the quarantine role right below the bot. The security score checks the second one (Move role fix). When a live test gets 403, check the role order first.

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

**Long work under a global lock.** (#100)
Similarity compared whole 4000-character messages (quadratic) while holding the global heat lock.
*Avoid:* bound the input (first 500 characters) and keep work under shared locks short.

**Listeners must survive a reconnect.** (#100)
The config listener died after one DB hiccup, so dashboard changes never applied until a bot restart.
*Avoid:* long-lived listeners restart in a loop and reload their caches after reconnecting.

**A failed log must not stop protection.** (#100, #133)
The timeout DM went out before the timeout worked, and a failed anti-nuke report stopped panic mode.
*Avoid:* fixed order Contain → Revert → Report → Panic. Logs and DMs are side effects; their failure never skips the next protective step.

**Heat lives in memory.** (#111, accepted)
A restart resets running heat. Strikes and timeouts survive. This was left as is on purpose.

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
*Avoid:* after adding a migration, `touch crates/core/src/db.rs` before testing. CI builds clean and is not affected.

**Parallel worktrees shared one target and one DB.** (11-01)
Cargo mixed crates from different worktrees, and one branch's migration broke the others' tests. Four targets (about 7 GB each) filled the disk.
*Avoid:* one `CARGO_TARGET_DIR` and one database per worktree (`su postgres -c "createdb X"`; `createdb -h localhost` hangs on a password prompt). Delete finished targets.

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
- **The toast looked like two pieces.** A coloured `border-left` on a rounded box curves at the corners, and the absolutely placed progress strip starts inside that border, so the two met as a hook. *Avoid:* one colour mark per box. Put strips and edges inside an `overflow:hidden` box with `left:0; right:0` and no border on that side. Also `cargo fmt` in `crates/dashboard/ui` reformats unrelated files; revert them before committing.

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

## 8. Tests

- **Defaults are off on new servers.** Heat filters and anti-nuke start off, so a live scenario must turn on what it tests (#92, #93).
- **Bots are exempt from a lot.** Webhooks owned by TommyBot or by a verified bot are exempt, bot-authored commands are ignored and bots cannot press buttons. Coordinated spam, confirm buttons, appeals, reports, Join Gate and DM tickets need a second human account (#93, #142).
- **The live script shares the bot's token** and its rate limits. Cache channel and role lookups and wait longer instead of polling hard (#142).
- **Make scenarios clean up after themselves** so they can run again. The old nuke scenario kicked the attacker bot every run (#142).
- **Cloud env vars are read at session start.** A variable added during a session needs a new session.
- **`tests/e2e/run.sh` needs root Postgres, trunk and Playwright**; the gateway is not covered by the Discord mock.

# Dashboard UI pitfalls

Pitfalls for the rest of the bot are in [PITFALLS.md](../../../PITFALLS.md) at the repo root.

These are Leptos mistakes that broke the dashboard in November 2026. They compile without a warning, so read this before you change the UI. The e2e sweep (`tests/e2e/dashboard.js`) checks for each of them.

## 1. A `>` in an attribute ends the tag

The `view!` macro reads an attribute value without braces only up to the next `>`. So this:

```rust
<button disabled=move || lo >= num() on:click=move |_| step(-1.0)>"-"</button>
```

becomes `<button disabled=move || lo>` plus the text `= num() on:click=move |_| step(-1.0)>-`. It still compiles. The page then shows that code as text, and the button has no click handler. Every number field with a minimum had this bug: the `-` button showed code and did nothing.

A `<` does no harm, because the macro does not treat it as the end of the tag. That is why the `+` button with `hi <= num()` worked.

**Rule:** if an attribute value holds `>` or `>=`, wrap it in braces: `disabled=move || { lo >= num() }`. You can also flip the comparison: `num() <= lo`. Expressions inside `(...)` are safe, for example `.is_some_and(|a| a.len() >= 25)`.

**Check:** the sweep fails if a page shows `move |` or `on:click=` as text. CSS can show the text in capitals, so the check ignores case.

## 2. Reading a signal after its page is gone

A page's signals and `StoredValue`s are disposed when you leave the page. Most reads of a disposed value panic:

```
panicked at reactive_graph-0.2.16/src/traits.rs:361:29:
Tried to access a reactive value that has already been disposed.
```

Rust in wasm cannot recover from a panic, so the whole dashboard stops ("Dashboard failed to start: RuntimeError: unreachable executed"). Reload the page to get it back.

This happens when code runs **after the page is gone**:

- the rest of a `spawn_local` task after an `.await`, while the request was still running when you changed page;
- a `set_timeout`, `request_animation_frame` or `later(...)` callback;
- a `load()` closure called again after an await, for example `Ok(_) => load()`, because `load` reads `g.get_value()` or `page.get_untracked()`.

A slow or failing server makes this much more likely. That is the normal case while the dashboard restarts during an Update, and Update is where the crash first showed.

What panics and what does not:

| Call on a disposed value | Result |
|---|---|
| `.get()`, `.get_untracked()`, `.with(..)`, `.with_untracked(..)`, `.read()` | **panics** |
| `StoredValue::get_value()`, `with_value(..)` | **panics** |
| `NodeRef::get_untracked()` | **panics** |
| `.set(..)`, `.update(..)`, `update_value(..)` | does nothing, safe |
| `.try_get_untracked()`, `.try_with(..)`, `.is_disposed()` | returns `None` / `true`, safe |

**Rules:**

1. After every `.await` in a `spawn_local`, check that the page still exists before you read anything:
   ```rust
   let r = api("GET", &path, None).await;
   if data.is_disposed() {
       return;
   }
   ```
2. A closure that can run again after an await (`load`, `load_mine`, ...) starts with `if g.is_disposed() { return; }`.
3. In timers and frame callbacks, read with `try_` (`msg.stay.try_get_untracked() == Some(false)`, `pv.try_get_untracked().flatten()`).
4. `run_act`, `run_change` and `run_save` (in `common.rs`) skip their callback if the page that started them is gone. Use them for actions instead of writing your own `spawn_local`.
5. Setting a value is fine without a check. Only reads panic.

We did not switch every task to `spawn_local_scoped_with_cancellation`. It also cancels a task when only a smaller part of the page is removed, for example a dialog that closes itself. The save's follow-up step would then never run.

**Check:** the sweep switches through every page while each API response is held back 800 ms, then again while every request fails. Any panic fails the test.

## 3. A hover rule beats the "selected" rule

`nav a:not(.srv):hover` has a higher specificity than `nav a.on`, because `:not(.srv)` counts as a class. Under the mouse the selected link (the server on the server list, the open page in the menu) got the dark hover background while its text stayed dark: 1.1:1 contrast, almost invisible.

**Rule:** a rule for a selected or active state lists its `:hover` variant with the same selectors as the hover rule: `nav a.on, nav a.on:not(.srv):hover { ... }`.

**Check:** the `contrast` phase of `tests/e2e/deep.js` hovers every link and button on every page in light and dark mode and fails under 3:1.

## 4. A page that reads a context signal is rebuilt when the signal is set again

The page closure in `App` read `access.of(p)`. The access is fetched again on every page change, and setting a `RwSignal` notifies even when the value is the same, so every page was built twice and loaded its settings, schema and Discord data twice. The page was also built before the access answer came, so hidden pages started their requests anyway.

**Rule:** read a value that is fetched again through a `Memo` (`page_access`), which only notifies when the value changes. Decide what to show before you build the page's components.

## 5. A dropped browser promise hides a refusal

`clipboard().write_text()` returns a promise. `let _ =` dropped it, the button said "Copied!" when the browser refused, and the refusal surfaced as an uncaught page error. That included the Rescue Key, which is shown once.

**Rule:** await browser promises with `JsFuture` and show the result. Use `copy_text` and `copy_label` in `common.rs` for copy buttons.

## 6. An emptied number field saved `null`

Clearing a number field stored `null`, and almost every number setting is a plain `i64`/`u32`/`f64` in Rust, so the save was refused with "invalid type: null, expected i64". A float step also kept the float error (`0.01 + 1 - 1` gave `0.010000000000000009`).

**Rule:** `Field` sets the field's default when a number input is emptied, and float steps are rounded like percent steps. A new number field gets a default (`dv`) unless `null` means something.

## 7. A descendant selector styles more than the field title

`.row label { flex:0 0 100%; font-weight:700 }` is meant for a field's title, but it also hit the Import button of every list field (a `<label>` around the file input). Import grew to the full row, turned bold, and the help text was drawn on top of it (Join Gate's Strict and Wildcard Words).

The first fix (`.io label`) did nothing: it had the same specificity as `.row label`, which comes later in the file, so `.row label` still won. The same rule also made every checkbox in role and channel lists bold and capitalized.

**Rule:** a label that wraps a control (`.row label:has(> input)`) resets what `.row label` sets. When a rule must beat another one, give it a higher specificity, not just an earlier or later spot in the file, and check the result in the browser (`DEEP_ONLY=overflow` measures it). A list field's box takes the full row so its help text goes below it.

## 8. Layout breaks that only show at some widths or with long text

Help text next to a switch was squeezed into two thin columns (Join Gate "Exclude bots added by the owner", Miscellaneous, Auto Mod, Verification), and at 390 px the header was wider than the screen, so the avatar was cut off and the search box covered the logo.

**Rule:** a switch's help text sits on its own line under the label (`.tog > .muted`). On a phone, header links that do not fit move into the account menu (`.m-only`). `DEEP_ONLY=overflow node tests/e2e/deep.js` checks every page at 1400, 1024 and 390 px for text outside its box, cut off, on top of other text, or squeezed.

## 9. One version per widget on a page that saves several sections

Each Overview switch loaded the config and kept its own version, so after one switch saved, the next one sent an old version and was refused with "config changed, reload". Five switches also meant five identical config requests.

**Rule:** widgets on one page that save into the same config share one loaded config and one version signal, and update it after each save.

## 10. Many copies of one SVG icon share its ids

Every icon in `assets/icons` has fixed ids (`info-grad`, `info-cut`). With the info icon shown many times on a page, every copy paints with the first copy's gradient and mask, and draws nothing once that first copy is hidden (Chrome does not render a gradient from a `display:none` SVG).

**Rule:** `icons::icon` numbers the ids of every copy, so every icon is safe to repeat. Never inline an `assets/icons` SVG another way (for example `include_str!` into `inner_html`) without the same numbering.

The first fix added a separate `icon_unique` for the info icon only, while the arrows in sortable lists, the check rows on Overview and the sidebar icons (General, Filters and Panic Mode appear under both Auto Mod and Anti Nuke) still shared ids. Root cause: uniqueness was opt-in. It is now the only way `icon` works.

## 11. A transform on a parent moves its popup too

The info icon grew on hover with `transform:scale` on `.tip`, which also scaled the card inside it, so the card was wider than measured and crossed the screen edge on a phone.

**Rule:** animate the icon (`.tip .i`), not the element that holds the popup. The info card is placed when it opens (`Tip` in `common.rs`): shifted to stay inside `main` and the screen, and moved below the icon near the header. `DEEP_ONLY=overflow` opens every info card and checks it.

## 12. `AnimatedShow` can hide itself while `when` is true

Leptos's `AnimatedShow` starts a hide timer each time it reruns with `when` false, but only remembers the last timer. A form that loads sets `val` and `orig` one after the other, so `when` (`val != orig`) reruns false twice. The first timer is lost and fires later. If the user changes something within 200 ms, the "Changes detected!" bar shows and is hidden again at once. Nothing can be saved or cancelled until the next change. The Cancel e2e check hit this on a fresh server (#158).

**Rule:** use `common::Fade` instead of `AnimatedShow`. It puts `when` behind a `Memo`, so it reruns only when the value really changes. All 8 uses were switched.

Same class of bug, also in #158: the page wrapper read the access level straight from the access signal, so every access answer (one per page change) rebuilt the open page and could throw away edits made before it arrived. It now reads a `Memo` of the level.

**Rule:** when a reactive closure builds a whole page or form, read derived values through a `Memo`, not a signal that is set again with the same content.

## 13. A hidden popup still makes the page wider

The info card (`.tip .bubble`) is only `opacity:0` until hovered, so it still takes layout room. An info icon near the right edge on a phone (MCP tokens page, 390 px) put the hidden card past the screen, and the whole page scrolled sideways.

**Rule:** `main` has `overflow-x:clip` (clip, not hidden, so vertical overflow and sticky parts keep working). A new popup that is hidden with opacity or a transform must also stay inside `main`, as `Tip`'s `place()` does when it is shown.

**Check:** on a 390 px viewport `document.documentElement.scrollWidth` equals the viewport width on every page.

## 14. An effect on a new input runs before its `prop:value` is set

Channel Setup's rename box focused and selected its text from an `Effect` on the input's `NodeRef`. The effect ran before `prop:value` was applied, so `select()` selected an empty box, the name was filled in afterwards, and typing added to the old name instead of replacing it. A `select()` in `on:focus` had the same problem.

**Rule:** an effect that needs a new input's value sets it first (`i.set_value(..)`, then `focus()` and `select()`). No other page selected text on open.

## 15. Overlapping saves stored an older draft

Channel Setup sent a PUT for every edit. Two in flight could reach the server in either order, so the older draft could be the one stored, and the refused-save path set the page from a follow-up GET without checking that it was still the newest request. An edit made before the first GET answered built on an empty draft and replaced the stored one.

**Rule:** a page that saves on every edit keeps one request in flight: later edits change only the local copy and mark it dirty, and the reply sends the newest copy if it differs. Every reply that sets page data after an await checks its sequence number with `try_get_value()` (rule 2). Actions that need the saved state (Push, Discard) are disabled while a request is pending, and editing waits until the first load is in.

## 16. A panel rebuilt from every reply loses the focus

Channel Setup's right panel is rebuilt from the server's reply after each change, so the button, switch or picker that was just used was replaced and keyboard focus fell back to the page.

**Rule:** a reactive closure that rebuilds controls notes the focused control's `aria-label` before it builds and focuses the new control with that label in a `request_animation_frame` (`panel_focus`/`refocus` in `channels.rs`). Every control in such a panel needs a unique `aria-label`.

## Where it was fixed (#143)

- `form.rs`: the `-` button on number fields (`>=` in the attribute); the settings card's config/schema load; the preview flash frame callback; list import.
- `update.rs`: the update status request.
- `pages.rs`: the setup score request.
- `common.rs`: the toast timer, and the shared `run` behind `run_act`/`run_change`/`run_save`.
- `appeals.rs`, `embeds.rs`, `lockdown.rs`, `records.rs`: the `load` closures that run again after an action.
- `main.rs`: config import after the file is read.

# Dashboard UI pitfalls

Pitfalls for the rest of the bot are in [PITFALLS.md](../../../PITFALLS.md) at the repo root.

Two Leptos mistakes broke the dashboard in November 2026. Both compile without a warning, so read this before you change the UI. The e2e sweep (`tests/e2e/dashboard.js`) now checks for both.

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

## Where it was fixed (#143)

- `form.rs`: the `-` button on number fields (`>=` in the attribute); the settings card's config/schema load; the preview flash frame callback; list import.
- `update.rs`: the update status request.
- `pages.rs`: the setup score request.
- `common.rs`: the toast timer, and the shared `run` behind `run_act`/`run_change`/`run_save`.
- `appeals.rs`, `embeds.rs`, `lockdown.rs`, `records.rs`: the `load` closures that run again after an action.
- `main.rs`: config import after the file is read.

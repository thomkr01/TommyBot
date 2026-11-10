//! Bot-owner page that updates a self-hosted TommyBot to the newest published version, with live progress.
use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::Value;
use std::time::Duration;

const STEPS: [&str; 5] = [
    "Checking the running version",
    "Downloading the new version",
    "Restarting the bot",
    "Restarting the dashboard",
    "Checking that everything runs",
];

fn short(v: &Value) -> String {
    v.as_str()
        .map(|s| s.chars().take(7).collect())
        .unwrap_or_else(|| "unknown".into())
}

fn dur(s: i64) -> String {
    if s >= 60 {
        format!("{} min {} s", s / 60, s % 60)
    } else {
        format!("{s} s")
    }
}

/// Share done (0 to 1) and seconds left, from the per-step estimate and the time spent in the current step.
fn progress(est: &[i64], step: usize, elapsed: i64) -> (f64, i64) {
    let step = step.min(est.len() - 1);
    let total: i64 = est.iter().sum::<i64>().max(1);
    let done = est[..step].iter().sum::<i64>() + elapsed.clamp(0, est[step]);
    let left = est[step + 1..].iter().sum::<i64>() + (est[step] - elapsed).max(1);
    (done as f64 / total as f64, left)
}

#[component]
pub fn Update(enrolled: ReadSignal<bool>) -> impl IntoView {
    let data = RwSignal::new(Value::Null);
    // Set while the dashboard cannot be reached, which is expected while it restarts itself.
    let away = RwSignal::new(false);
    let loaded_sha = StoredValue::new(None::<Value>);
    let msg = Msg::default();
    let need = RwSignal::new(false);
    let load = move || {
        spawn_local(async move {
            let r = api("GET", "/api/update", None).await;
            // The user may have left the page while the request ran (the dashboard restarts mid-update).
            if data.is_disposed() {
                return;
            }
            match r {
                Ok(v) => {
                    if loaded_sha.get_value().is_none() {
                        loaded_sha.set_value(Some(v["current"].clone()))
                    }
                    data.set(v);
                    away.set(false);
                }
                Err(e) if data.get_untracked().is_null() => msg.err(e),
                Err(_) => away.set(true),
            }
        })
    };
    load();
    let active = move || {
        matches!(
            data.get()["run"]["status"].as_str(),
            Some("queued" | "running")
        )
    };
    let poll = set_interval_with_handle(
        move || {
            if active() || away.get_untracked() {
                load()
            }
        },
        Duration::from_secs(1),
    );
    on_cleanup(move || {
        if let Ok(h) = poll {
            h.clear()
        }
    });
    let start = move |_| run_act(api("POST", "/api/update", None), msg, need, move |_| load());

    let head = move || {
        let d = data.get();
        let (cur, new) = (
            d["current"].as_str().map(String::from),
            d["latest"]["sha"].as_str().map(String::from),
        );
        let fresh = cur.is_some() && cur == new;
        let news = cur.zip(new).filter(|(c, n)| c != n).map(|(c, n)| view! { " " <a href=format!("https://github.com/thomkr01/TommyBot/compare/{c}...{n}") target="_blank" rel="noopener">"See what's new"</a> });
        view! {
            <div class="card">
                <p><b>"Running: "</b><code>{short(&d["current"])}</code></p>
                <p><b>"Newest: "</b>{if d["latest"].is_null() { view! { <span class="muted">"Could not reach GitHub right now."</span> }.into_any() } else { view! { <code>{short(&d["latest"]["sha"])}</code>" "<span class="muted">{d["latest"]["title"].as_str().unwrap_or("").to_string()}</span>{news} }.into_any() }}</p>
                {if !d["updater"].as_bool().unwrap_or(false) {
                    view! { <p class="warn">"The updater is not running, so updates cannot start. On the server run "<code>"git pull && docker compose up -d updater"</code>" once."</p> }.into_any()
                } else if fresh {
                    view! { <p class="ok">"TommyBot is up to date."</p> }.into_any()
                } else if active() {
                    ().into_any()
                } else {
                    view! { <button class="go" on:click=start>"Update now"</button> }.into_any()
                }}
            </div>
        }
    };

    let run = move || {
        let d = data.get();
        let r = &d["run"];
        let status = r["status"].as_str()?.to_string();
        let est: Vec<i64> = d["estimate"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .filter(|e: &Vec<i64>| e.len() == STEPS.len())
            .unwrap_or_else(|| vec![10; STEPS.len()]);
        let step = r["step"].as_u64().unwrap_or(0) as usize;
        let live = status == "running";
        let (share, left) = progress(&est, step, r["step_elapsed"].as_i64().unwrap_or(0));
        let rows = STEPS
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let class = match status.as_str() {
                    "done" => "ok",
                    "running" if i == step => "run",
                    "failed" | "rolled_back" if i == step => "err",
                    _ if i < step => "ok",
                    _ => "",
                };
                let took = r["step_times"]
                    .get(i)
                    .and_then(Value::as_i64)
                    .map(|s| format!(" · {}", dur(s)));
                view! { <li class=class>{*name}<span class="muted">{took}</span></li> }
            })
            .collect_view();
        let summary = match status.as_str() {
            "queued" => view! { <p class="muted">"Waiting for the updater to pick it up..."</p> }.into_any(),
            "running" => view! {
                <progress max="1" value=share.to_string() aria-label="update progress"></progress>
                <p class="muted">{format!("About {} left", dur(left))}{move || away.get().then_some(". The dashboard is restarting, this page reconnects by itself.")}</p>
            }.into_any(),
            "done" => {
                let reload = loaded_sha.get_value().is_some_and(|s| s != d["current"]);
                view! {
                    <p class="ok">{format!("Updated to {} in {}.", short(&r["to_sha"]), dur(r["took"].as_i64().unwrap_or(0)))}</p>
                    {reload.then(|| view! { <button on:click=|_| { web_sys::window().unwrap().location().reload().ok(); }>"Reload the dashboard"</button> })}
                }.into_any()
            }
            _ => view! { <p class="err">{r["error"].as_str().unwrap_or("The update failed.").to_string()}</p> }.into_any(),
        };
        Some(
            view! { <div class="card"><h3>{if live { "Updating" } else { "Last update" }}</h3><ul class="checks">{rows}</ul>{summary}</div> },
        )
    };

    view! {
        <div class="head"><small>"TommyBot"</small><h1>"Update"</h1></div>
        <StepUp need enrolled />
        {head}
        {run}
        <Toast msg />
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_follows_the_estimate_and_never_runs_out() {
        let est = [2, 60, 10, 10, 18];
        assert_eq!(progress(&est, 0, 0), (0.0, 100));
        assert_eq!(progress(&est, 1, 30), (0.32, 68));
        // Slower than estimated: the bar waits at the step's end and says about a second is left for it.
        assert_eq!(progress(&est, 4, 99), (1.0, 1));
    }
}

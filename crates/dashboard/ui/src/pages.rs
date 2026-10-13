use crate::common::*;
use crate::form::Section;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
}

#[component]
pub fn Wizard(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    const STEPS: [&str; 5] = ["Auto setup", "Logging channel", "Main roles", "Permits", "Done"];
    let step = RwSignal::new(0usize);
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    let auto = move |_| {
        let g = g.clone();
        run_act(async move {
            let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
            api("POST", &format!("/api/guilds/{g}/wizard/auto-setup"), Some(json!({"expected_version": c["version"]}))).await
        }, msg, need, move |v| msg.set(format!("Created: {}", v["created"])));
    };
    let g = guild.clone();
    view! {
        <h1>"Setup wizard"</h1>
        <div class="row">
            {STEPS.iter().enumerate().map(|(i, t)| view! { <button class:ghost=move || step.get() != i on:click=move |_| step.set(i)>{format!("{}. {t}", i + 1)}</button> }).collect_view()}
        </div>
        {move || match step.get() {
            0 => view! { <div class="card"><p class="muted">"Creates the quarantine role and the log channels."</p><button on:click=auto.clone()>"Run auto setup"</button><p>{move || msg.get()}</p></div> }.into_any(),
            1 => view! { <Section guild=g.clone() name="logging".into() enrolled /> }.into_any(),
            2 => view! { <Section guild=g.clone() name="lockdown".into() enrolled /> }.into_any(),
            3 => view! { <div class="card"><a href=format!("#/{g}/permits")>"Review permits"</a></div> }.into_any(),
            _ => view! { <div class="card"><a href=format!("#/{g}/overview")>"Check the security score"</a></div> }.into_any(),
        }}
        <StepUp need enrolled />
    }
}

#[component]
pub fn Score(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let rows = RwSignal::new(Vec::<Value>::new());
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    let load = move || { let g = g.clone(); spawn_local(async move {
        match api("GET", &format!("/api/guilds/{g}/score"), None).await {
            Ok(v) => rows.set(v.as_array().cloned().unwrap_or_default()),
            Err(e) => msg.set(e),
        }
    }) };
    load();
    view! {
        <h1>"Security score"</h1>
        <p class="err">{move || msg.get()}</p>
        <For each=move || rows.get() key=|r| s(&r["id"]) let:r>
            {
                let (g, load) = (guild.clone(), load.clone());
                let class = match r["severity"].as_str() { Some("ok") => "ok", Some("warn") => "warn", _ => "err" };
                let fix = r["fix"]["label"].as_str().map(|l| {
                    let role = r["fix"]["role_id"].clone();
                    let l = l.to_string();
                    view! { <button on:click=move |_| { let (g, role, load) = (g.clone(), role.clone(), load.clone());
                        run_act(async move { api("POST", &format!("/api/guilds/{g}/score/fix"), Some(json!({"role_id": role}))).await }, msg, need, move |_| load()) }>{l}</button> }
                });
                view! { <div class="card"><b class=class>{s(&r["title"])}</b><p class="muted">{s(&r["detail"])}</p>{fix}</div> }
            }
        </For>
        <StepUp need enrolled />
    }
}

#[component]
pub fn Revert(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (actor, hours) = (RwSignal::new(String::new()), RwSignal::new("24".to_string()));
    let rows = RwSignal::new(Vec::<Value>::new());
    let picked = RwSignal::new(Vec::<i64>::new());
    let report = RwSignal::new(String::new());
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    let load = move |_| { let g = g.clone(); spawn_local(async move {
        let q = format!("/api/guilds/{g}/actions?actor={}&hours={}", actor.get_untracked().trim(), hours.get_untracked().trim());
        match api("GET", &q, None).await {
            Ok(v) => { msg.set(String::new()); picked.set(vec![]); rows.set(v.as_array().cloned().unwrap_or_default()) }
            Err(e) => msg.set(e),
        }
    }) };
    let revert = move |_| {
        let (g, ids) = (guild.clone(), picked.get_untracked());
        if ids.is_empty() { return msg.set("Select at least one action".into()) }
        run_act(async move { api("POST", &format!("/api/guilds/{g}/revert"), Some(json!({"ids": ids}))).await }, msg, need, move |v| {
            let fails: Vec<String> = v["failed"].as_array().map(|a| a.iter().map(|f| format!("{}: {}", s(&f["id"]), s(&f["error"]))).collect()).unwrap_or_default();
            report.set(format!("Done: {}\nFailed: {}\nSkipped: {}\nRe-invite these users manually: {}", v["done"], fails.join("; "), v["skipped"], v["reinvite"]));
            msg.set(String::new());
        });
    };
    view! {
        <h1>"Revert center"</h1>
        <div class="row">
            <input placeholder="Actor user ID" prop:value=actor on:input=move |e| actor.set(event_target_value(&e)) />
            <input placeholder="Hours" prop:value=hours on:input=move |e| hours.set(event_target_value(&e)) />
            <button on:click=load>"Load"</button>
        </div>
        <p class="err">{move || msg.get()}</p>
        <For each=move || rows.get() key=|r| s(&r["id"]) let:r>
            {
                let id = r["id"].as_i64().unwrap_or(0);
                view! { <div class="card row">
                    <input type="checkbox" disabled=r["reverted"].as_bool().unwrap_or(false) on:change=move |e| picked.update(|p| { p.retain(|x| *x != id); if event_target_checked(&e) { p.push(id) } }) />
                    <b>{s(&r["action"])}</b>
                    <span class="muted">{format!("target {} at {}{}", s(&r["target_id"]), s(&r["at"]), if r["reverted"].as_bool().unwrap_or(false) { " (reverted)" } else { "" })}</span>
                </div> }
            }
        </For>
        <button on:click=revert>"Revert selected"</button>
        <pre>{move || report.get()}</pre>
        <StepUp need enrolled />
    }
}

#[component]
pub fn Rescue(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let has_key = RwSignal::new(false);
    let key = RwSignal::new(String::new());
    let claim = RwSignal::new(String::new());
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    spawn_local(async move {
        match api("GET", &format!("/api/guilds/{g}/rescue"), None).await {
            Ok(v) => has_key.set(v["has_key"].as_bool().unwrap_or(false)),
            Err(e) => msg.set(e),
        }
    });
    let g = guild.clone();
    let regen = move |_| {
        let g = g.clone();
        run_act(async move { api("POST", &format!("/api/guilds/{g}/rescue/regenerate"), None).await }, msg, need, move |v| { key.set(s(&v["key"])); has_key.set(true); });
    };
    let claim_it = move |_| {
        let (g, k) = (guild.clone(), claim.get_untracked());
        run_act(async move { api("POST", "/api/rescue/claim", Some(json!({"guild_id": g, "key": k.trim()}))).await }, msg, need, move |_| msg.set("Claimed.".into()));
    };
    view! {
        <h1>"Rescue key"</h1>
        <div class="card">
            <p class="muted">{move || if has_key.get() { "A rescue key exists. Regenerating invalidates the old one." } else { "No rescue key yet." }}</p>
            <button on:click=regen>"Regenerate key (owner)"</button>
            <Show when=move || !key.get().is_empty()>
                <p><b>"Shown once. Store it somewhere safe:"</b></p>
                <pre>{move || key.get()}</pre>
            </Show>
        </div>
        <div class="card">
            <b>"Claim ownership with a key"</b>
            <div class="row"><input placeholder="Rescue key" prop:value=claim on:input=move |e| claim.set(event_target_value(&e)) /><button on:click=claim_it>"Claim"</button></div>
        </div>
        <p>{move || msg.get()}</p>
        <StepUp need enrolled />
    }
}

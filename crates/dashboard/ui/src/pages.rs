use crate::common::*;
use crate::form::{name_of, Section};
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
}

#[component]
pub fn Wizard(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    const STEPS: [&str; 9] = ["Auto Setup", "Logging", "Automod", "Antinuke", "Whitelist", "Join Gate", "Join Raid", "Verification", "Rescue Key"];
    let step = RwSignal::new(0usize);
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    let g2 = guild.clone();
    let auto = move |_| {
        let (g, g2) = (g.clone(), g2.clone());
        run_act(async move {
            let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
            api("POST", &format!("/api/guilds/{g}/wizard/auto-setup"), Some(json!({"expected_version": c["version"]}))).await
        }, msg, need, move |v| { let (g, c) = (g2.clone(), v["created"].clone()); spawn_local(async move {
            let dc = api("GET", &format!("/api/guilds/{g}/discord"), None).await.unwrap_or(Value::Null);
            let n = |k: &str, key: &str| name_of(&dc, key, c[k].as_str().unwrap_or(""));
            msg.set(format!("Created: quarantine role {}, log channel {}, mod log channel {}", n("quarantine_role", "roles"), n("logs", "channels"), n("modlogs", "channels")));
        }) });
    };
    let g = guild.clone();
    view! {
        <div class="card">
            <div class="split">
                <div><h2>"Setup process"</h2><p class="muted">"Follow the steps to set up the bot. You can jump to any step."</p></div>
                <Ring pct=Signal::derive(move || (step.get() + 1) * 100 / STEPS.len()) color="var(--accent)" />
            </div>
            <div class="steps">
                {STEPS.iter().enumerate().map(|(i, t)| view! { <button class:on=move || step.get() == i on:click=move |_| step.set(i)><i>{i + 1}</i>{*t}</button> }).collect_view()}
            </div>
        </div>
        {move || match step.get() {
            0 => view! { <div class="card"><p class="muted">"Creates the quarantine role and the log channels."</p><button on:click=auto.clone()>"Run auto setup"</button><p class="info">{move || msg.get()}</p></div> }.into_any(),
            1 => view! { <Section guild=g.clone() name="logging".into() enrolled /><Section guild=g.clone() name="lockdown".into() enrolled /> }.into_any(),
            8 => view! { <Rescue guild=g.clone() enrolled /> }.into_any(),
            i => view! { <Section guild=g.clone() name=["", "", "automod", "antinuke", "whitelist", "joingate", "joinraid", "verification"][i].into() enrolled /> }.into_any(),
        }}
        <StepUp need enrolled />
    }
}

/// Progress ring: `pct` filled, number in the middle.
#[component]
pub fn Ring(pct: Signal<usize>, color: &'static str) -> impl IntoView {
    view! { <div class="ring" style=move || format!("--p:{};--c:{color}", pct.get())><b>{move || format!("{}%", pct.get())}</b></div> }
}

const SYSTEMS: [(&str, &str, &str); 5] = [
    ("automod", "Auto Mod", "Heat-based spam, link and invite filtering with strikes."),
    ("antinuke", "Anti Nuke", "Watches staff actions and guards the server against nukes."),
    ("joingate", "Join Gate", "Instant filters for new members and bots joining."),
    ("joinraid", "Join Raid", "Detects raids by join rate and reacts to them."),
    ("panic", "Panic Mode", "Locks things down when several staff trip the anti-nuke at once."),
];

#[component]
pub fn Score(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let rows = RwSignal::new(Vec::<Value>::new());
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let guilds = expect_context::<ReadSignal<Vec<Value>>>();
    let members = RwSignal::new(Value::Null);
    let g = guild.clone();
    spawn_local(async move { if let Ok(d) = api("GET", &format!("/api/guilds/{g}/discord"), None).await { members.set(d["members"].clone()) } });
    let g = guild.clone();
    let load = move || { let g = g.clone(); spawn_local(async move {
        match api("GET", &format!("/api/guilds/{g}/score"), None).await {
            Ok(v) => rows.set(v.as_array().cloned().unwrap_or_default()),
            Err(e) => msg.set(e),
        }
    }) };
    load();
    let pct = Signal::derive(move || { let r = rows.get(); if r.is_empty() { 0 } else { r.iter().filter(|x| x["severity"] == "ok").count() * 100 / r.len() } });
    let (id, g2) = (guild.clone(), guild.clone());
    view! {
        <div class="card"><div class="split">
            <div>
                <h2>"Details"</h2>
                {move || guilds.get().into_iter().find(|x| x["id"] == id.as_str()).map(|x| view! {
                    <span class="stat"><span>"Server name"</span><b>{s(&x["name"])}</b></span>
                    <span class="stat"><span>"Server ID"</span><b>{s(&x["id"])}</b></span>
                    <span class="stat"><span>"Your access"</span><b>{s(&x["level"])}</b></span> })}
                {move || (!members.get().is_null()).then(|| view! { <span class="stat"><span>"Members"</span><b>{s(&members.get())}</b></span> })}
            </div>
            <div><h2>"Security"</h2><Ring pct color="var(--warn)" /></div>
        </div></div>
        <h2 style="text-align:center">"Quick systems overview"</h2>
        <div class="cards">
            {SYSTEMS.iter().map(|(k, t, d)| view! { <div class="card sys"><h3>{*t}</h3><p>{*d}</p><div class="foot"><a href=format!("#/{g2}/{k}")><button>"Settings"</button></a></div></div> }).collect_view()}
        </div>
        <h2 style="margin-top:24px">"Security checks"</h2>
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

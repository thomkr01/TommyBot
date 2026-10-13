use crate::common::*;
use crate::form::{name_of, Section};
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
}

#[component]
pub fn Wizard(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    const STEPS: [&str; 9] = ["Auto Setup", "Miscellaneous", "Automod", "Antinuke", "Whitelist", "Join Gate", "Join Raid", "Verification", "Rescue Key"];
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
    let me = expect_context::<ReadSignal<Option<Value>>>();
    let support = move || me.get().and_then(|m| m["support_url"].as_str().map(String::from)).unwrap_or_else(|| "https://github.com/thomkr01/TommyBot".into());
    view! {
        <div class="wizbar"><button on:click=move |_| step.set(0)>"Start wizard"</button><a href=support target="_blank" rel="noopener">"Support"</a></div>
        <div class="card">
            <div class="split">
                <div><h2>"Setup process"</h2><p>"This wizard helps simplify TommyBot's setup for you in detailed explained steps that you can follow to ensure the proper behavior you want from TommyBot."</p></div>
                <Ring pct=Signal::derive(move || (step.get() + 1) * 100 / STEPS.len()) color=Some("var(--primary)") />
            </div>
            <h2>"Steps"</h2>
            <div class="steps">
                {STEPS.iter().enumerate().map(|(i, t)| view! { <button class:on=move || step.get() == i on:click=move |_| step.set(i)><i>{i + 1}</i>{*t}</button> }).collect_view()}
            </div>
            <div class="chips"><span class="chip blue">"You can choose a certain step if you want."</span><span class="chip orange">"The full setup could take around 3 minutes of your time."</span></div>
        </div>
        <p class="muted">{move || [
            "Creates the quarantine role and log channels for you.",
            "General bot behavior: moderation commands, replies and the default timeout.",
            "Turn Auto Mod on and set its strike and heat basics. Filters have their own pages.",
            "Turn Anti Nuke on and pick the quarantine role. Per-action limits are under Filters.",
            "Members and roles the automatic systems should ignore.",
            "Instant filters for new members and bots.",
            "Reaction to a burst of joins.",
            "The gate new members must pass to get access.",
            "Your rescue key for locked-out recovery.",
        ][step.get()]}</p>
        {move || match step.get() {
            0 => view! { <div class="card"><button on:click=auto.clone()>"Run auto setup"</button><p class="info">{move || msg.get()}</p></div> }.into_any(),
            8 => view! { <Rescue guild=g.clone() enrolled /> }.into_any(),
            i => view! { <Section guild=g.clone() name=["", "misc", "automod", "antinuke", "whitelist", "joingate", "joinraid", "verification"][i].into() part=if (2..=4).contains(&i) { "general" } else { "" } enrolled /> }.into_any(),
        }}
        <p><Show when=move || step.get() < STEPS.len() - 1><button on:click=move |_| step.update(|i| *i += 1)>"Next"</button></Show></p>
        <StepUp need enrolled />
    }
}

/// Progress ring: `pct` filled, number in the middle.
#[component]
pub fn Ring(pct: Signal<usize>, #[prop(default = None)] color: Option<&'static str>) -> impl IntoView {
    // Without a fixed colour the ring is red under 50, orange under 80 and green above.
    let c = move || color.unwrap_or(match pct.get() { 0..50 => "var(--bad)", 50..80 => "var(--warn)", _ => "var(--ok)" });
    view! { <div class="ring" style=move || format!("--p:{};--c:{}", pct.get(), c())><b>{move || format!("{}%", pct.get())}</b></div> }
}

/// (section, title, description, has a master switch, switch default when the key is missing).
const SYSTEMS: [(&str, &str, &str, bool, bool); 5] = [
    ("automod", "Auto Mod", "Anti Spam, Anti Advertising, Anti Phishing and all the message filters.", true, false),
    ("antinuke", "Anti Nuke", "The system that monitors your staff members and guards your server away from getting nuked.", true, true),
    ("joingate", "Join Gate", "All of the instant filters that scan new members and bots joining your server.", true, true),
    ("joinraid", "Join Raid", "The advanced system that monitors your server joins and reacts if a raid is targeting your server.", true, true),
    ("verification", "Verification", "TommyBot's verification gate that your new members need to pass in order to get access to your server.", true, false),
];

/// Master switch of a system: reads and writes its `enabled` key.
#[component]
fn SystemToggle(guild: String, section: &'static str, default: bool) -> impl IntoView {
    let (cfg, version, on) = (RwSignal::new(json!({})), RwSignal::new(0i64), RwSignal::new(default));
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{g}/config"), None).await {
            version.set(v["version"].as_i64().unwrap_or(0));
            on.set(v["config"][section]["enabled"].as_bool().unwrap_or(default));
            if v["config"][section].is_object() { cfg.set(v["config"][section].clone()); }
        }
    });
    let change = move |e| {
        let x = event_target_checked(&e);
        cfg.update(|c| c["enabled"] = json!(x));
        let (g, v, value) = (guild.clone(), version.get_untracked(), cfg.get_untracked());
        on.set(x);
        run_act(async move { api("PUT", &format!("/api/guilds/{g}/config/{section}"), Some(json!({"expected_version": v, "value": value}))).await }, msg, need, move |r| version.set(r["version"].as_i64().unwrap_or(0)));
    };
    view! { <input type="checkbox" class="sw" aria-label="enable system" prop:checked=move || on.get() on:change=change /><span class="err">{move || msg.get()}</span> }
}

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
    let notes = RwSignal::new(false);
    view! {
        <div class="card"><div class="split">
            <div>
                <h2>"Details"</h2>
                {move || guilds.get().into_iter().find(|x| x["id"] == id.as_str()).map(|x| view! {
                    <Stat label="Server name" value=s(&x["name"]) />
                    <Stat label="Server ID" value=s(&x["id"]) />
                    // The bot runs a single gateway shard.
                    <Stat label="Shard ID" value="0".into() />
                    <span class="stat"><span>"Type"</span><b class="chip">"STANDARD"</b></span>
                    <span class="stat"><span>"Your access"</span><b>{s(&x["level"])}</b></span> })}
                {move || (!members.get().is_null()).then(|| view! { <Stat label="Members" value=s(&members.get()) /> })}
            </div>
            <div><h2>"Security"</h2><Ring pct /><p><button class="ghost" on:click=move |_| notes.update(|n| *n = !*n)>"Notes"</button></p></div>
        </div></div>
        <h2 style="text-align:center">"Quick systems overview"</h2>
        <div class="cards">
            {SYSTEMS.iter().map(|(k, t, d, sw, def)| view! { <div class="card sys"><h3>{*t}</h3><p>{*d}</p><div class="foot"><a href=format!("#/{g2}/{k}")><button>"Settings"</button></a>{sw.then(|| view! { <SystemToggle guild=g2.clone() section=*k default=*def /> })}</div></div> }).collect_view()}
        </div>
        <p class="err">{move || msg.get()}</p>
        <Show when=move || notes.get()><h2 style="margin-top:24px">"Security notes"</h2></Show>
        <For each=move || if notes.get() { rows.get() } else { vec![] } key=|r| s(&r["id"]) let:r>
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

/// A detail with a copy button.
#[component]
fn Stat(label: &'static str, value: String) -> impl IntoView {
    let copied = RwSignal::new(false);
    let v = value.clone();
    let copy = move |_| {
        if let Some(w) = web_sys::window() {
            let _ = w.navigator().clipboard().write_text(&v);
            copied.set(true);
        }
    };
    view! { <span class="stat"><span>{label}</span><b>{value}</b><button class="ghost copy" on:click=copy>{move || if copied.get() { "Copied!" } else { "Copy" }}</button></span> }
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

/// Logging page: Wick and Discord tabs, each with essential and customize sub-tabs.
#[component]
pub fn Logging(guild: String, tab: String, sub: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let tab = if tab == "discord" { "discord" } else { "wick" };
    let sub = if sub == "customize" { "customize" } else { "essential" };
    let part = format!("{tab}_{sub}");
    view! {
        <div class="tabs">
            <a class:on=tab == "wick" href=format!("#/{guild}/logging/wick/{sub}")>"wick"</a>
            <a class:on=tab == "discord" href=format!("#/{guild}/logging/discord/{sub}")>"discord"</a>
        </div>
        <div class="tabs">
            <a class:on=sub == "essential" href=format!("#/{guild}/logging/{tab}/essential")>"essential"</a>
            <a class:on=sub == "customize" href=format!("#/{guild}/logging/{tab}/customize")>"customize"</a>
        </div>
        <Section guild=guild.clone() name="logging".to_string() part enrolled />
    }
}

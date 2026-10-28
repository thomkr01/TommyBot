use crate::common::*;
use crate::form::{name_of, strs, Section};
use crate::wizard::{Card, ANTINUKE, AUTOMOD, JOINGATE, JOINRAID, MISC, VERIFICATION, WHITELIST};
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
}

/// Log channels the wizard's Custom choice can add to the essentials: (channel name, label).
const OPTIONAL_LOGS: [(&str, &str); 10] = [
    ("reports", "Reports"),
    ("appeals", "Appeals"),
    ("automod-logs", "Automod"),
    ("antinuke-logs", "Antinuke"),
    ("verification-logs", "Verification"),
    ("joingate-logs", "Joingate"),
    ("joinraid-logs", "Joinraid"),
    ("panic-logs", "Panic Mode"),
    ("server-logs", "Server"),
    ("discord-logs", "Discord events"),
];

#[component]
pub fn Wizard(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    const STEPS: [&str; 10] = ["Auto Setup", "Misc", "Automod", "Antinuke", "Whitelist", "Join Gate", "Join Raid", "Verification", "Rescue", "Wrap"];
    const CARDS: [(&str, &[Card]); 7] = [("misc", MISC), ("automod", AUTOMOD), ("antinuke", ANTINUKE), ("whitelist", WHITELIST), ("joingate", JOINGATE), ("joinraid", JOINRAID), ("verification", VERIFICATION)];
    let step = RwSignal::new(0usize);
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    let custom = RwSignal::new(false);
    let picks = RwSignal::new(OPTIONAL_LOGS.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>());
    let auto = move |logs: Option<Vec<String>>| {
        let (g, g2) = (g.clone(), g.clone());
        run_act(async move {
            let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
            api("POST", &format!("/api/guilds/{g}/wizard/auto-setup"), Some(json!({"expected_version": c["version"], "logs": logs}))).await
        }, msg, need, move |v| { let (g, c, failed) = (g2.clone(), v["created"].clone(), v["quarantine_failed"].clone()); spawn_local(async move {
            let dc = api("GET", &format!("/api/guilds/{g}/discord"), None).await.unwrap_or(Value::Null);
            let role = |k: &str| name_of(&dc, "roles", c[k].as_str().unwrap_or(""));
            let logs = strs(&c["logs"]).join(", #");
            let logs = if logs.is_empty() { "the log channels were already set".into() } else { format!("log channels #{logs} (reports and appeals in a Staff category)") };
            let member = if c["member_role"].is_string() { format!(", main role {}", role("member_role")) } else { String::new() };
            let member = if c["staff"] == true { format!("{member}, Admins and Moderators roles with their permits") } else { member };
            let quarantine = match failed.as_u64() { Some(0) => String::new(), Some(n) => format!(" Could not set the quarantine role in {n} channels; give TommyBot Manage Roles there."), None => " Could not set the quarantine role in the channels.".into() };
            msg.set(format!("Done: quarantine role {}{member}, {logs}.{quarantine}", role("quarantine_role")));
        }) });
    };
    let guilds = expect_context::<ReadSignal<Vec<Value>>>();
    let g_id = guild.clone();
    let g = guild.clone();
    let me = expect_context::<ReadSignal<Option<Value>>>();
    let support = move || me.get().and_then(|m| m["support_url"].as_str().map(String::from)).unwrap_or_else(|| "https://github.com/thomkr01/TommyBot".into());
    view! {
        <div class="wizbar"><button on:click=move |_| step.set(0)>"Start wizard"</button><a href=support target="_blank" rel="noopener">"Support"</a></div>
        <div class="card">
            <div class="split">
                <div><h2>{move || format!("{} Setup Process", guilds.get().iter().find(|x| x["id"] == g_id.as_str()).and_then(|x| x["name"].as_str().map(String::from)).unwrap_or_default())}</h2><p>"This wizard helps simplify TommyBot's setup for you in detailed explained steps that you can follow to ensure the proper behavior you want from TommyBot."</p></div>
                <Ring pct=Signal::derive(move || (step.get() + 1) * 100 / STEPS.len()) color=Some("var(--primary)") />
                <svg class="wizart" viewBox="0 0 120 120" aria-hidden="true"><path d="M60 8 104 26v32c0 28-19 46-44 54C35 104 16 86 16 58V26z" fill="var(--primary)" opacity=".25" stroke="var(--primary)" stroke-width="4"/><path d="m40 60 14 14 28-30" fill="none" stroke="var(--primary)" stroke-width="8" stroke-linecap="round" stroke-linejoin="round"/></svg>
            </div>
            <h2>"Steps"</h2>
            <div class="steps">
                {STEPS.iter().enumerate().map(|(i, t)| view! { <button class:on=move || step.get() == i on:click=move |_| step.set(i)><i>{i + 1}</i>{*t}</button> }).collect_view()}
            </div>
            <div class="chips"><span class="chip blue">"You can choose a certain step if you want."</span><span class="chip orange">"The full setup could take around 3 minutes of your time."</span></div>
        </div>
        {move || match step.get() {
            0 => {
                let (all, run) = (auto.clone(), auto.clone());
                view! { <div class="card">
                    <h2>"Initial Setup"</h2>
                    <p>"TommyBot needs to do some magic in your server: it creates its log channels, the Quarantine role (set up in all of your channels), a Member main role and Admins and Moderators staff roles when you have none. Anything you already set is kept."</p>
                    <ul class="checks">{["Ensuring Quarantine role", "Setting up Quarantine role across all channels", "Ensuring Member role", "Ensuring staff roles and permits", "Ensuring log channels"].map(|t| view! { <li class:ok=move || msg.get().starts_with("Done")>{t}</li> }).collect_view()}</ul>
                    <p>"The essential logs (TommyBot logs and Mod logs) are always created."</p>
                    <button on:click=move |_| all(None)>"All log channels"</button>
                    <button on:click=move |_| custom.update(|c| *c = !*c)>"Custom"</button>
                    <Show when=move || custom.get()>
                        {OPTIONAL_LOGS.iter().map(|(name, label)| view! { <label><input type="checkbox" prop:checked=move || picks.get().iter().any(|p| p == name) on:change=move |e| picks.update(|l| if event_target_checked(&e) { l.push(name.to_string()) } else { l.retain(|p| p != name) }) /> {*label}</label> }).collect_view()}
                        <button on:click={let run = run.clone(); move |_| run(Some(picks.get()))}>"Run with selected"</button>
                    </Show>
                    <p class="info">{move || msg.get()}</p>
                </div> }.into_any()
            }
            8 => view! { <Rescue guild=g.clone() enrolled /> }.into_any(),
            9 => view! { <div class="card"><h2>"All done!"</h2><p>"TommyBot is set up. Every setting can be changed later on its own page, and the Overview shows your security score."</p><a href=format!("#/{}/overview", g)><button class="go">"Go to Overview"</button></a></div> }.into_any(),
            i => { let (name, cards) = CARDS[i - 1]; view! { <Section guild=g.clone() name=name.to_string() cards next=Callback::new(move |_| step.update(|i| *i += 1)) enrolled /> }.into_any() }
        }}
        <p><Show when=move || matches!(step.get(), 0 | 8)><button on:click=move |_| step.update(|i| *i += 1)>"Next"</button></Show></p>
        <StepUp need enrolled />
    }
}

/// Progress ring: `pct` filled, number in the middle.
#[component]
pub fn Ring(pct: Signal<usize>, #[prop(default = None)] color: Option<&'static str>, #[prop(default = false)] small: bool) -> impl IntoView {
    // Without a fixed colour the ring is red under 50, orange under 80 and green above.
    let c = move || color.unwrap_or(match pct.get() { 0..50 => "var(--bad)", 50..80 => "var(--warn)", _ => "var(--ok)" });
    view! { <div class="ring" class:small=small style=move || format!("--p:{};--c:{}", pct.get(), c())><b>{move || format!("{}%", pct.get())}</b></div> }
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
    view! {
        <div class="card"><div class="split">
            <div>
                <h2>"Details"</h2>
                {move || guilds.get().into_iter().find(|x| x["id"] == id.as_str()).map(|x| view! {
                    <Stat label="Server Name" value=s(&x["name"]) />
                    <Stat label="Server ID" value=s(&x["id"]) />
                    // The bot runs a single gateway shard.
                    <Stat label="Shard ID" value="0".into() />
                    <span class="stat"><span>"Type"</span><b class="chip">"STANDARD"</b></span> })}
                {move || (!members.get().is_null()).then(|| view! { <Stat label="Members" value=s(&members.get()) /> })}
            </div>
        </div></div>
        <div class="card"><div class="row"><h2>"Security score"</h2><Ring pct small=true /></div>
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
                view! { <div class="row note"><b class=class>{if class == "ok" { "✓" } else { "!" }}" "{s(&r["title"])}</b><span class="muted">{s(&r["detail"])}</span>{fix}</div> }
            }
        </For></div>
        <h2 style="text-align:center">"Quick Systems Overview"</h2>
        <div class="cards five">
            {SYSTEMS.iter().map(|(k, t, d, sw, def)| view! { <div class="card sys"><h3>{*t}</h3><p>{*d}</p><div class="foot"><a href=format!("#/{g2}/{k}")><button>"Settings"</button></a>{sw.then(|| view! { <SystemToggle guild=g2.clone() section=*k default=*def /> })}</div></div> }).collect_view()}
        </div>
        <p class="err">{move || msg.get()}</p>
        <StepUp need enrolled />
    }
}

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
    let shown = RwSignal::new(false);
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
            <button style="background:var(--warn);color:#000" on:click=regen>"REGENERATE"</button>
            <Show when=move || !key.get().is_empty()>
                <p><b>"Rescue Key (shown once, store it somewhere safe):"</b></p>
                <div class="row"><pre class:blur=move || !shown.get()>{move || key.get()}</pre>
                    <button class="danger" on:click=move |_| shown.update(|v| *v = !*v)>{move || if shown.get() { "Hide" } else { "Show" }}</button>
                    <button class="ghost" on:click=move |_| { if let Some(w) = web_sys::window() { let _ = w.navigator().clipboard().write_text(&key.get_untracked()); } }>"Copy"</button></div>
                <ul class="checks warn"><li>"View this rescue key alone!"</li><li>"Anyone with this rescue key can get dangerous TommyBot permissions."</li><li>"This rescue key is used ONLY when you lose physical owner account access."</li><li>"It grants TommyBot owner permissions, not Discord ownership."</li></ul>
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
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    // One click: private "Logs" and "Staff" categories with a channel per log type that has none yet.
    let setup = move |_| {
        let g = g.clone();
        run_act(async move {
            let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
            api("POST", &format!("/api/guilds/{g}/logging/setup"), Some(json!({"expected_version": c["version"]}))).await
        }, msg, need, |_| { web_sys::window().unwrap().location().reload().ok(); });
    };
    view! {
        <div class="row"><button on:click=setup>"Create log channels"</button><span>{move || msg.get()}</span></div>
        <StepUp need enrolled />
        <div class="tabs">
            <a class:on=tab == "wick" href=format!("#/{guild}/logging/wick/{sub}")>"tommybot"<span class="chip new">"new"</span></a>
            <a class:on=tab == "discord" href=format!("#/{guild}/logging/discord/{sub}")>"discord"<span class="chip new">"new"</span></a>
        </div>
        <div class="tabs">
            <a class:on=sub == "essential" href=format!("#/{guild}/logging/{tab}/essential")>"essential"</a>
            <a class:on=sub == "customize" href=format!("#/{guild}/logging/{tab}/customize")>"customize"</a>
        </div>
        <Section guild=guild.clone() name="logging".to_string() part enrolled />
    }
}

/// Catch channel: one click makes the honeypot channel and turns it on, then the settings form.
#[component]
pub fn Catch(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    let setup = move |_| {
        let sure = "This creates #do-not-write at the top of the server and turns the catch channel on: anyone without a permit who writes there gets the action below. Continue?";
        if !web_sys::window().unwrap().confirm_with_message(sure).unwrap_or(false) { return; }
        let g = g.clone();
        run_act(async move {
            let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
            api("POST", &format!("/api/guilds/{g}/catch/setup"), Some(json!({"expected_version": c["version"]}))).await
        }, msg, need, |_| { web_sys::window().unwrap().location().reload().ok(); });
    };
    view! {
        <div class="row"><button on:click=setup>"Set up channel"</button><span>{move || msg.get()}</span></div>
        <StepUp need enrolled />
        <Section guild=guild.clone() name="catch".to_string() enrolled />
    }
}

/// Tickets settings with a one-click setup that creates the channels and posts the panel.
#[component]
pub fn Tickets(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = guild.clone();
    let setup = move |_| {
        let g = g.clone();
        run_act(async move {
            let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
            api("POST", &format!("/api/guilds/{g}/tickets/setup"), Some(json!({"expected_version": c["version"]}))).await
        }, msg, need, |_| { web_sys::window().unwrap().location().reload().ok(); });
    };
    view! {
        <div class="row"><button on:click=setup>"Set up tickets and post the panel"</button><span>{move || msg.get()}</span></div>
        <StepUp need enrolled />
        <Section guild name="tickets".to_string() part=String::new() enrolled />
    }
}

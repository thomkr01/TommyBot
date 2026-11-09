use crate::common::*;
use crate::form::{name_of, strs, Section};
use crate::wizard::{
    Card, ANTINUKE, AUTOMOD, JOINGATE, JOINRAID, MISC, TICKETS, VERIFICATION, WHITELIST,
};
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str()
        .map(String::from)
        .unwrap_or_else(|| v.to_string())
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
    const STEPS: [&str; 11] = [
        "Auto Setup",
        "Misc",
        "Automod",
        "Antinuke",
        "Whitelist",
        "Join Gate",
        "Join Raid",
        "Verification",
        "Tickets",
        "Rescue",
        "Wrap",
    ];
    const CARDS: [(&str, &[Card]); 8] = [
        ("misc", MISC),
        ("automod", AUTOMOD),
        ("antinuke", ANTINUKE),
        ("whitelist", WHITELIST),
        ("joingate", JOINGATE),
        ("joinraid", JOINRAID),
        ("verification", VERIFICATION),
        ("tickets", TICKETS),
    ];
    let step = RwSignal::new(0usize);
    // The furthest step reached, kept per server in this browser so the ring keeps its progress.
    let key = format!("wizard:{guild}");
    let store = || web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    let furthest = RwSignal::new(
        store()
            .and_then(|s| s.get_item(&key).ok().flatten())
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0),
    );
    Effect::new(move |_| {
        let i = step.get();
        if i > furthest.get_untracked() {
            furthest.set(i);
            if let Some(s) = store() {
                let _ = s.set_item(&key, &i.to_string());
            }
        }
    });
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let g = guild.clone();
    let custom = RwSignal::new(false);
    let picks = RwSignal::new(Vec::<String>::new());
    let running = move || msg.get().starts_with("Running");
    let auto = move |logs: Option<Vec<String>>| {
        let (g, g2) = (g.clone(), g.clone());
        msg.ok("Running Auto Setup... this takes a little while on servers with many channels.");
        run_act(
            async move {
                let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
                api(
                    "POST",
                    &format!("/api/guilds/{g}/wizard/auto-setup"),
                    Some(json!({"expected_version": c["version"], "logs": logs})),
                )
                .await
            },
            msg,
            need,
            move |v| {
                let (g, c, failed) = (
                    g2.clone(),
                    v["created"].clone(),
                    v["quarantine_failed"].clone(),
                );
                spawn_local(async move {
                    let dc = api("GET", &format!("/api/guilds/{g}/discord"), None)
                        .await
                        .unwrap_or(Value::Null);
                    let role = |k: &str| name_of(&dc, "roles", c[k].as_str().unwrap_or(""));
                    let logs = strs(&c["logs"]).join(", #");
                    let logs = if logs.is_empty() {
                        "the log channels were already set".into()
                    } else {
                        format!("log channels #{logs} (reports and appeals in a Staff category)")
                    };
                    let member = if c["member_role"].is_string() {
                        format!(", main role {}", role("member_role"))
                    } else {
                        String::new()
                    };
                    let member = if c["staff"] == true {
                        format!("{member}, Admins and Moderators roles with their permits")
                    } else {
                        member
                    };
                    let quarantine = match failed.as_u64() { Some(0) => String::new(), Some(n) => format!(" Could not set the quarantine role in {n} channels; give TommyBot Manage Roles there."), None => " Could not set the quarantine role in the channels.".into() };
                    msg.ok(format!("Done: quarantine role {}{member}, {logs}.{quarantine} Looks like you are good to go! It seems like your server has what the automatic setup works on adding, you are good to proceed!", role("quarantine_role")));
                })
            },
        );
    };
    let guilds = expect_context::<ReadSignal<Vec<Value>>>();
    let g_id = guild.clone();
    let g = guild.clone();
    let me = expect_context::<ReadSignal<Option<Value>>>();
    let support = move || {
        me.get()
            .and_then(|m| m["support_url"].as_str().map(String::from))
            .unwrap_or_else(|| "https://github.com/thomkr01/TommyBot".into())
    };
    // Same side arrows as the settings cards of steps 1-8.
    let arrows = move |body: AnyView| {
        view! { <div class="card wiz">
            <button class="nav back" aria-label="previous" disabled=move || step.get() == 0 on:click=move |_| step.update(|i| *i = i.saturating_sub(1))>{crate::icons::icon("page_prev")}</button>
            <div class="wizbody">{body}</div>
            <button class="nav" aria-label="next" on:click=move |_| step.update(|i| *i += 1)>{crate::icons::icon("page_next")}</button>
        </div> }.into_any()
    };
    view! {
        <div class="wizbar"><button on:click=move |_| { step.update(|i| *i = (*i + 1).min(STEPS.len() - 1)); scroll_to("wizstep") }>{move || if step.get() == 0 { "Start wizard" } else { "Next step" }}</button><a href=support target="_blank" rel="noopener">"Support"</a></div>
        <div class="card">
            <div class="split">
                <div><h2>{move || format!("{} Setup Process", guilds.get().iter().find(|x| x["id"] == g_id.as_str()).and_then(|x| x["name"].as_str().map(String::from)).unwrap_or_default())}</h2><p>"This wizard helps simplify TommyBot's setup for you in detailed explained steps that you can follow to ensure the proper behavior you want from TommyBot."</p></div>
                <div class="brand"><Ring pct=Signal::derive(move || (furthest.get().max(step.get()) + 1) * 100 / STEPS.len()) /></div>
                <svg class="wizart" viewBox="0 0 120 120" aria-hidden="true"><defs><linearGradient id="tb-brand" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#FFA36C"/><stop offset=".35" stop-color="#FF6F91"/><stop offset=".7" stop-color="#9B7BFF"/><stop offset="1" stop-color="#4FC3F7"/></linearGradient></defs><path d="M60 8 104 26v32c0 28-19 46-44 54C35 104 16 86 16 58V26z" fill="url(#tb-brand)" fill-opacity=".25" stroke="url(#tb-brand)" stroke-width="4"/><path class="tick" d="m40 60 14 14 28-30" fill="none" stroke="url(#tb-brand)" stroke-width="8" stroke-linecap="round" stroke-linejoin="round"/></svg>
            </div>
            <h2>"Steps"</h2>
            <div class="steps">
                {STEPS.iter().enumerate().map(|(i, t)| view! { <button class:on=move || step.get() == i on:click=move |_| step.set(i)><i>{i + 1}</i>{*t}</button> }).collect_view()}
            </div>
            <div class="chips"><span class="chip blue">"You can choose a certain step if you want."</span><span class="chip orange">"The full setup could take around 3 minutes of your time."</span></div>
        </div>
        {move || view! { <div class="enter" id="wizstep">{match step.get() {
            0 => {
                let (all, run) = (auto.clone(), auto.clone());
                arrows(view! { <div>
                    <h2>"Initial Setup"</h2>
                    <p>"TommyBot needs to do some magic in your server: it creates #tommybot-logs and #mod-logs, the Quarantine role (set up in all of your channels), a Member main role and Admins and Moderators staff roles when you have none. Anything you already set is kept."</p>
                    <ul class="checks">{["Ensuring Quarantine role", "Setting up Quarantine role across all channels", "Ensuring Member role", "Ensuring staff roles and permits", "Ensuring #tommybot-logs channel", "Ensuring #mod-logs channel"].into_iter().enumerate().map(|(i, t)| view! { <li style=format!("--i:{i}") class:run=running class:ok=move || msg.get().starts_with("Done")>{t}</li> }).collect_view()}</ul>
                    <p>"It makes #tommybot-logs and #mod-logs; every other log goes to #tommybot-logs until you give it a channel under Logging > Customize. Custom also makes a channel for the types you tick."</p>
                    <button disabled=running on:click=move |_| all(None)>{move || if running() { "Setting up..." } else { "Auto Setup" }}</button>
                    <button class="ghost" class:on=custom on:click=move |_| custom.update(|c| *c = !*c)>"Custom"</button>
                    <Show when=move || custom.get()>
                        {OPTIONAL_LOGS.iter().map(|(name, label)| view! { <label><input type="checkbox" prop:checked=move || picks.get().iter().any(|p| p == name) on:change=move |e| picks.update(|l| if event_target_checked(&e) { l.push(name.to_string()) } else { l.retain(|p| p != name) }) /> {*label}</label> }).collect_view()}
                        <button disabled=running on:click={let run = run.clone(); move |_| run(Some(picks.get()))}>"Run with selected"</button>
                    </Show>
                    <Toast msg />
                </div> }.into_any())
            }
            9 => arrows(view! { <Rescue guild=g.clone() enrolled /> }.into_any()),
            10 => view! { <div class="card"><h2>"All done!"</h2><p>"TommyBot is set up. Every setting can be changed later on its own page, and the Overview shows your security score."</p><a class="btn go" href=format!("#/{}/overview", g)>"Go to Overview"</a></div> }.into_any(),
            i => { let (name, cards) = CARDS[i - 1]; view! { <Section guild=g.clone() name=name.to_string() cards next=Callback::new(move |_| step.update(|i| *i += 1)) enrolled /> }.into_any() }
        }}</div> }}
        <StepUp need enrolled />
    }
}

/// Progress ring: `pct` filled, number in the middle.
#[component]
pub fn Ring(
    pct: Signal<usize>,
    #[prop(default = None)] color: Option<&'static str>,
    #[prop(default = false)] small: bool,
) -> impl IntoView {
    // Without a fixed colour the ring is red under 50, orange under 80 and green above.
    let c = move || {
        color.unwrap_or(match pct.get() {
            0..50 => "var(--bad)",
            50..80 => "var(--warn)",
            _ => "var(--ok)",
        })
    };
    view! { <div class="ring" class:small=small style=move || format!("--p:{};--c:{}", pct.get(), c())><b>{move || format!("{}%", pct.get())}</b></div> }
}

/// (section, title, description, has a master switch, switch default when the key is missing).
const SYSTEMS: [(&str, &str, &str, bool, bool); 5] = [
    ("automod", "Auto Mod", "Anti Spam, Anti Advertising, Anti Phishing and all the message filters.", true, false),
    ("antinuke", "Anti Nuke", "The system that monitors your staff members and guards your server away from getting nuked.", true, true),
    ("joingate", "Join Gate", "All of the instant filters that scan new members and bots joining your server.", true, false),
    ("joinraid", "Join Raid", "The advanced system that monitors your server joins and reacts if a raid is targeting your server.", true, true),
    ("verification", "Verification", "TommyBot's verification gate that your new members need to pass in order to get access to your server.", true, false),
];

/// Master switch of a system: reads and writes its `enabled` key.
#[component]
fn SystemToggle(guild: String, section: &'static str, default: bool) -> impl IntoView {
    let (cfg, version, on) = (
        RwSignal::new(json!({})),
        RwSignal::new(0i64),
        RwSignal::new(default),
    );
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    // Locked while the config loads and while a save runs, so the knob never shows a state the server does not have.
    let busy = RwSignal::new(true);
    let g = guild.clone();
    spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{g}/config"), None).await {
            version.set(v["version"].as_i64().unwrap_or(0));
            on.set(v["config"][section]["enabled"].as_bool().unwrap_or(default));
            if v["config"][section].is_object() {
                cfg.set(v["config"][section].clone());
            }
        }
        busy.set(false);
    });
    let change = move |e| {
        let x = event_target_checked(&e);
        let before = cfg.get_untracked();
        cfg.update(|c| c["enabled"] = json!(x));
        let (g, v, value) = (guild.clone(), version.get_untracked(), cfg.get_untracked());
        on.set(x);
        busy.set(true);
        run_act(
            async move {
                let r = api(
                    "PUT",
                    &format!("/api/guilds/{g}/config/{section}"),
                    Some(json!({"expected_version": v, "value": value})),
                )
                .await;
                if r.is_err() {
                    cfg.set(before);
                    on.set(!x);
                }
                busy.set(false);
                r
            },
            msg,
            need,
            move |r| {
                msg.ok(format!("{section} turned {}", if x { "on" } else { "off" }));
                version.set(r["version"].as_i64().unwrap_or(0))
            },
        );
    };
    view! { <input type="checkbox" class="sw" aria-label=format!("enable {section}") aria-busy=move || busy.get().to_string() prop:disabled=move || busy.get() prop:checked=move || on.get() on:change=change /><Toast msg /> }
}

#[component]
pub fn Score(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let rows = RwSignal::new(Vec::<Value>::new());
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let guilds = expect_context::<ReadSignal<Vec<Value>>>();
    let members = RwSignal::new(Value::Null);
    let g = guild.clone();
    spawn_local(async move {
        if let Ok(d) = api("GET", &format!("/api/guilds/{g}/discord"), None).await {
            members.set(d["members"].clone())
        }
    });
    let g = guild.clone();
    let load = move || {
        let g = g.clone();
        spawn_local(async move {
            match api("GET", &format!("/api/guilds/{g}/score"), None).await {
                Ok(v) => rows.set(v.as_array().cloned().unwrap_or_default()),
                Err(e) => msg.err(e),
            }
        })
    };
    load();
    let pct = Signal::derive(move || {
        let r = rows.get();
        if r.is_empty() {
            0
        } else {
            r.iter().filter(|x| x["severity"] == "ok").count() * 100 / r.len()
        }
    });
    let (id, g2) = (guild.clone(), guild.clone());
    view! {
        <div class="card"><div class="split">
            <div>
                <h2>"Details"</h2>
                {move || guilds.get().into_iter().find(|x| x["id"] == id.as_str()).map(|x| view! {
                    <Stat label="Server Name" value=s(&x["name"]) />
                    <Stat label="Server ID" value=s(&x["id"]) />
                    })}
                {move || (!members.get().is_null()).then(|| view! { <Stat label="Members" value=s(&members.get()) /> })}
            </div>
        </div></div>
        <div class="card"><div class="row"><h2>"Security score"</h2><Ring pct small=true /></div>
        <For each=move || rows.get() key=|r| s(&r["id"]) let:r>
            {
                let (g, load) = (guild.clone(), load.clone());
                let class = match r["severity"].as_str() { Some("ok") => "ok", Some("warn") => "warn", _ => "err" };
                let fix = r["fix"]["label"].as_str().map(|l| {
                    let body = r["fix"].clone();
                    let l = l.to_string();
                    view! { <button on:click=move |_| { let (g, body, load, l) = (g.clone(), body.clone(), load.clone(), l.clone());
                        if !confirm(&format!("{l}?")) { return }
                        run_act(async move { api("POST", &format!("/api/guilds/{g}/score/fix"), Some(body)).await }, msg, need, move |_| { msg.ok(format!("Fixed: {l}. The check below now shows the new state, and the change is in the config history.")); load() }) }>{l.clone()}</button> }
                });
                view! { <div class="row note"><b class=class>{crate::icons::icon(if class == "ok" { "success" } else { "warning" })}" "{s(&r["title"])}</b><span class="muted">{s(&r["detail"])}</span>{fix}</div> }
            }
        </For></div>
        <h2 style="text-align:center">"Quick Systems Overview"</h2>
        <div class="cards five">
            {SYSTEMS.iter().map(|(k, t, d, sw, def)| view! { <div class="card sys"><h3>{*t}</h3><p>{*d}</p><div class="foot"><a class="btn" href=format!("#/{g2}/{k}")>"Settings"</a>{sw.then(|| view! { <SystemToggle guild=g2.clone() section=*k default=*def /> })}</div></div> }).collect_view()}
        </div>
        <Toast msg />
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
    let (actor, hours) = (
        RwSignal::new(String::new()),
        RwSignal::new("24".to_string()),
    );
    let rows = RwSignal::new(Vec::<Value>::new());
    let picked = RwSignal::new(Vec::<i64>::new());
    let report = RwSignal::new(String::new());
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let g = guild.clone();
    let load = move |_| {
        let g = g.clone();
        spawn_local(async move {
            let q = format!(
                "/api/guilds/{g}/actions?actor={}&hours={}",
                actor.get_untracked().trim(),
                hours.get_untracked().trim()
            );
            match api("GET", &q, None).await {
                Ok(v) => {
                    msg.clear();
                    picked.set(vec![]);
                    rows.set(v.as_array().cloned().unwrap_or_default())
                }
                Err(e) => msg.err(e),
            }
        })
    };
    let revert = move |_| {
        let (g, ids) = (guild.clone(), picked.get_untracked());
        if ids.is_empty() {
            return msg.err("Select at least one action");
        }
        run_act(
            async move {
                api(
                    "POST",
                    &format!("/api/guilds/{g}/revert"),
                    Some(json!({"ids": ids})),
                )
                .await
            },
            msg,
            need,
            move |v| {
                let fails: Vec<String> = v["failed"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|f| format!("{}: {}", s(&f["id"]), s(&f["error"])))
                            .collect()
                    })
                    .unwrap_or_default();
                report.set(format!(
                    "Done: {}\nFailed: {}\nSkipped: {}\nRe-invite these users manually: {}",
                    v["done"],
                    fails.join("; "),
                    v["skipped"],
                    v["reinvite"]
                ));
                msg.clear();
            },
        );
    };
    view! {
        <div class="row">
            <input placeholder="Actor user ID" aria-label="actor user ID" prop:value=actor on:input=move |e| actor.set(event_target_value(&e)) />
            <input placeholder="Hours" aria-label="hours back" prop:value=hours on:input=move |e| hours.set(event_target_value(&e)) />
            <button on:click=load>"Load"</button>
        </div>
        <Toast msg />
        <For each=move || rows.get() key=|r| s(&r["id"]) let:r>
            {
                let id = r["id"].as_i64().unwrap_or(0);
                view! { <div class="card row">
                    <input type="checkbox" aria-label=format!("select action {id}") disabled=r["reverted"].as_bool().unwrap_or(false) on:change=move |e| picked.update(|p| { p.retain(|x| *x != id); if event_target_checked(&e) { p.push(id) } }) />
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
    let qr = RwSignal::new(String::new());
    let shown = RwSignal::new(false);
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let g = guild.clone();
    spawn_local(async move {
        match api("GET", &format!("/api/guilds/{g}/rescue"), None).await {
            Ok(v) => has_key.set(v["has_key"].as_bool().unwrap_or(false)),
            Err(e) => msg.err(e),
        }
    });
    let g = guild.clone();
    let regen = move |_| {
        if has_key.get_untracked()
            && !confirm(
                "Make a new rescue key? The old key and its authenticator code stop working.",
            )
        {
            return;
        }
        let g = g.clone();
        run_act(
            async move { api("POST", &format!("/api/guilds/{g}/rescue/regenerate"), None).await },
            msg,
            need,
            move |v| {
                key.set(s(&v["key"]));
                has_key.set(true);
                qr.set(
                    qrcode::QrCode::new(v["uri"].as_str().unwrap_or(""))
                        .map(|q| {
                            q.render::<qrcode::render::svg::Color>()
                                .min_dimensions(200, 200)
                                .build()
                        })
                        .unwrap_or_default(),
                );
            },
        );
    };
    view! {
        <div class="card">
            <p class="muted">{move || if has_key.get() { "A rescue key exists. Regenerating invalidates the old one." } else { "No rescue key yet." }}</p>
            <button class="warn" on:click=regen>"REGENERATE"</button>
            <Show when=move || !key.get().is_empty()>
                <p><b>"Rescue Key (shown once, store it somewhere safe):"</b></p>
                <div class="row"><pre class:blur=move || !shown.get()>{move || key.get()}</pre>
                    <button class="danger" on:click=move |_| shown.update(|v| *v = !*v)>{move || if shown.get() { "Hide" } else { "Show" }}</button>
                    <button class="ghost" on:click=move |_| { if let Some(w) = web_sys::window() { let _ = w.navigator().clipboard().write_text(&key.get_untracked()); } }>"Copy"</button></div>
                <p><b>"Scan this with Google Authenticator or Authy too: a rescue needs the key and its 6-digit code."</b></p>
                <div class="qr" class:blur=move || !shown.get() inner_html=move || qr.get()></div>
                <ul class="checks warn"><li>"View this rescue key alone!"</li><li>"Never share a screenshot of this QR code."</li><li>"Anyone with this rescue key can get dangerous TommyBot permissions."</li><li>"This rescue key is used ONLY when you lose physical owner account access."</li><li>"It grants TommyBot owner permissions, not Discord ownership."</li></ul>
            </Show>
        </div>
        <Toast msg />
        <StepUp need enrolled />
    }
}

/// Owner-only "delete all stored data" for the server, confirmed by typing the server's name.
#[component]
pub fn PurgeData(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let guilds = expect_context::<ReadSignal<Vec<Value>>>();
    let gid = StoredValue::new(guild);
    let name = Memo::new(move |_| {
        guilds
            .get()
            .iter()
            .find(|x| x["id"] == gid.get_value().as_str())
            .map(|x| s(&x["name"]))
            .unwrap_or_default()
    });
    let (open, typed) = (RwSignal::new(false), RwSignal::new(String::new()));
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let matches = move || !typed.get().trim().is_empty() && typed.get().trim() == name.get().trim();
    let purge = move |_| {
        let g = gid.get_value();
        open.set(false);
        run_act(
            async move { api("POST", &format!("/api/guilds/{g}/purge"), None).await },
            msg,
            need,
            move |_| {
                typed.set(String::new());
                msg.ok("All stored data for this server was deleted. TommyBot now runs on default settings.")
            },
        );
    };
    let r = NodeRef::new();
    focus_on_open(r);
    view! {
        <div class="card">
            <h3>"Delete server data"</h3>
            <p class="muted">"Deletes everything TommyBot stores for this server: settings, cases, logs, backups, tickets and permits. This cannot be undone. Data is also deleted automatically 30 days after TommyBot is removed."</p>
            <button class="danger" on:click=move |_| open.set(true)>"Delete all data"</button>
        </div>
        <AnimatedShow when=open hide_class="out" hide_delay=OUT>
            <div class="modal" node_ref=r role="dialog" aria-modal="true" aria-label="Delete server data" on:keydown=move |e| dialog_keys(&e, || open.set(false))><div class="card">
                <p>"Type the server name "<b>{name}</b>" to delete all of its stored data."</p>
                <input aria-label="server name" autocomplete="off" prop:value=typed on:input=move |e| typed.set(event_target_value(&e)) />
                <div class="row"><button class="ghost" on:click=move |_| open.set(false)>"Abort"</button><button class="danger" disabled=move || !matches() on:click=purge>"Delete"</button></div>
            </div></div>
        </AnimatedShow>
        <Toast msg />
        <StepUp need enrolled />
    }
}

/// "Lost control over TommyBot in a server?": claims Extra Owner with the rescue key or the
/// authenticator's 6-digit code. Works from the Servers page, before any server is open.
#[component]
pub fn LostControl(enrolled: ReadSignal<bool>) -> impl IntoView {
    let (open, guild, secret, code) = (
        RwSignal::new(false),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
    );
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let claim = move |_| {
        let body = json!({"guild_id": guild.get_untracked(), "key": secret.get_untracked().trim(), "code": code.get_untracked().trim()});
        run_act(
            async move { api("POST", "/api/rescue/claim", Some(body)).await },
            msg,
            need,
            move |_| msg.ok("Claimed. Refresh your servers."),
        );
    };
    view! {
        <button class="link err" class:on=open aria-expanded=move || open.get().to_string() on:click=move |_| open.update(|o| *o = !*o)>"Lost control over TommyBot in a server?"</button>
        <Show when=move || open.get()>
            <div class="card">
                <h2>"RESCUE PROCESS"</h2>
                <p class="muted">"You have lost access to your physical owner account. Enter the server ID, the rescue key and the 6-digit code from the authenticator you scanned with it."</p>
                <div class="row"><input placeholder="Server ID" aria-label="server ID" prop:value=guild on:input=move |e| guild.set(event_target_value(&e)) /><input placeholder="Rescue key" aria-label="rescue key" autocomplete="off" prop:value=secret on:input=move |e| secret.set(event_target_value(&e)) /><input placeholder="6-digit code" aria-label="6-digit code" autocomplete="one-time-code" inputmode="numeric" prop:value=code on:input=move |e| code.set(event_target_value(&e)) /><button on:click=claim>"CONFIRM"</button></div>
                <Toast msg />
            </div>
        </Show>
        <StepUp need enrolled />
    }
}

/// Logging page: TommyBot and Discord tabs, each with essential and customize sub-tabs.
#[component]
pub fn Logging(
    guild: String,
    tab: String,
    sub: String,
    enrolled: ReadSignal<bool>,
) -> impl IntoView {
    let tab = if tab == "discord" {
        "discord"
    } else {
        "tommybot"
    };
    let sub = if sub == "customize" {
        "customize"
    } else {
        "essential"
    };
    let part = format!("{tab}_{sub}");
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    // Bumped after a setup so the form below reloads the config the setup changed.
    let fresh = RwSignal::new(0u32);
    let g = guild.clone();
    // Optional: private "Logs" and "Staff" categories with a channel per log type that has none yet.
    let setup = move |_| {
        let g = g.clone();
        run_act(
            async move {
                let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
                let all: Vec<&str> = OPTIONAL_LOGS.iter().map(|(n, _)| *n).collect();
                api(
                    "POST",
                    &format!("/api/guilds/{g}/logging/setup"),
                    Some(json!({"expected_version": c["version"], "logs": all})),
                )
                .await
            },
            msg,
            need,
            move |_| {
                msg.ok("Done");
                fresh.update(|n| *n += 1)
            },
        );
    };
    view! {
        <div class="row"><button on:click=setup>"Create a channel per log type"</button><Toast msg /></div>
        <StepUp need enrolled />
        <div class="tabs">
            <a class:on=tab == "tommybot" href=format!("#/{guild}/logging/tommybot/{sub}")>"tommybot"</a>
            <a class:on=tab == "discord" href=format!("#/{guild}/logging/discord/{sub}")>"discord"</a>
        </div>
        <div class="tabs">
            <a class:on=sub == "essential" href=format!("#/{guild}/logging/{tab}/essential")>"essential"</a>
            <a class:on=sub == "customize" href=format!("#/{guild}/logging/{tab}/customize")>"customize"</a>
        </div>
        {move || { fresh.track(); let (g, part) = (guild.clone(), part.clone()); view! {
            {(tab == "tommybot" && sub != "customize").then(|| view! { <RouteNote guild=g.clone() /> })}
            <Section guild=g name="logging".to_string() part enrolled />
        } }}
    }
}

/// Note under TommyBot logs listing the types that have their own channel.
#[component]
fn RouteNote(guild: String) -> impl IntoView {
    let moved = RwSignal::new(None::<Vec<String>>);
    spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{guild}/config"), None).await {
            let routes = v["config"]["logging"]["routes"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let mut names: Vec<String> = routes
                .iter()
                .filter(|(k, r)| *k != "mod_command" && !r.is_null())
                .map(|(k, _)| k.replace('_', " "))
                .collect();
            names.sort();
            moved.set(Some(names));
        }
    });
    view! {
        {move || moved.get().filter(|n| !n.is_empty()).map(|n| view! { <details class="card" open=true><summary class="info">"Note"</summary>
            <p>"These log types have their own channel, so they skip the TommyBot logs channel:"</p>
            <div class="chips">{n.into_iter().map(|t| view! { <span class="chip blue">{t}</span> }).collect_view()}</div>
            <p class="muted">"Everything else, except mod logs, still lands in the TommyBot logs channel."</p></details> })}
    }
}

/// Catch channel: one click makes the honeypot channel and turns it on, then the settings form.
#[component]
pub fn Catch(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    // Bumped after a setup so the form below reloads the config the setup changed.
    let fresh = RwSignal::new(0u32);
    let g = guild.clone();
    let setup = move |_| {
        let sure = "This creates #do-not-write at the top of the server and turns the catch channel on: anyone without a permit who writes there gets the action below. Continue?";
        if !web_sys::window()
            .unwrap()
            .confirm_with_message(sure)
            .unwrap_or(false)
        {
            return;
        }
        let g = g.clone();
        run_act(
            async move {
                let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
                api(
                    "POST",
                    &format!("/api/guilds/{g}/catch/setup"),
                    Some(json!({"expected_version": c["version"]})),
                )
                .await
            },
            msg,
            need,
            move |_| {
                msg.ok("Done");
                fresh.update(|n| *n += 1)
            },
        );
    };
    view! {
        <div class="row"><button on:click=setup>"Set up channel"</button><Toast msg /></div>
        <StepUp need enrolled />
        {move || { fresh.track(); view! { <Section guild=guild.clone() name="catch".to_string() enrolled /> } }}
    }
}

/// Tickets settings with a one-click setup that creates the category and the ticket log.
#[component]
pub fn Tickets(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    // Bumped after a setup so the form below reloads the config the setup changed.
    let fresh = RwSignal::new(0u32);
    let g = guild.clone();
    let setup = move |_| {
        let g = g.clone();
        run_act(
            async move {
                let c = api("GET", &format!("/api/guilds/{g}/config"), None).await?;
                api(
                    "POST",
                    &format!("/api/guilds/{g}/tickets/setup"),
                    Some(json!({"expected_version": c["version"]})),
                )
                .await
            },
            msg,
            need,
            move |_| {
                msg.ok("Done");
                fresh.update(|n| *n += 1)
            },
        );
    };
    view! {
        <div class="row"><button on:click=setup>"Set up the ticket category and log"</button><Toast msg /></div>
        <StepUp need enrolled />
        {move || { fresh.track(); view! { <Section guild=guild.clone() name="tickets".to_string() enrolled /> } }}
    }
}

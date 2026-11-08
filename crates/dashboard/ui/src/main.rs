use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

mod appeals;
mod channels;
mod commands;
mod update;
mod records;
mod common;
mod discord;
mod embeds;
mod form;
mod icons;
mod lockdown;
mod md;
mod pages;
mod permits;
mod recovery;
mod search;
mod wizard;
use lockdown::{DiscordRules, LockdownManage, LockdownSettings};
use appeals::{AppealForm, AppealList, Blacklist};
use records::{CaseList, NoteList, RaidLog, ReportList, TicketLog};
use common::*;
use embeds::Embeds;
use form::Section;
use permits::Permits;
/// The server list's empty state; its text is the marker that shows the help card.
const NO_SERVERS: &str = "No servers";

use pages::{Catch, Logging, LostControl, PurgeData, Rescue, Revert, Score, Tickets, Wizard};
use search::{Search, SearchBox};
use recovery::{Backups, Panic};
use channels::ChannelSetup;
use commands::Commands;
use update::Update;

/// Sidebar with only with the pages TommyBot has.
const TOP: &[(&str, &str)] = &[("wizard", "Wizard"), ("overview", "Overview"), ("misc", "Miscellaneous"), ("permits", "Permits"), ("logging", "Logging"), ("backups", "Backups"), ("channel_setup", "Channel Setup"), ("lockdown", "Lockdown"), ("appeal_list", "Appeals")];
const NAV: &[(&str, &[(&str, &str)])] = &[
    ("Auto Mod", &[("automod", "General"), ("filters", "Filters"), ("discord_automod", "Discord"), ("whitelist", "Whitelist"), ("automod_panic", "Panic Mode")]),
    ("Anti Nuke", &[("antinuke", "General"), ("antinuke_filters", "Filters"), ("antinuke_panic", "Panic Mode")]),
    ("Server Joins", &[("joingate", "Join Gate"), ("joinraid", "Join Raid"), ("verification", "Verification")]),
    ("Moderation", &[("cases", "Cases"), ("notes", "Notes"), ("report_list", "Reports"), ("tickets", "Tickets"), ("catch", "Catch Channel")]),
    ("Tools", &[("revert", "Revert Center"), ("history", "History"), ("embeds", "Embed Builder"), ("rescue", "Rescue Key")]),
];
/// Groups that start open; the rest stay folded until visited.
const OPEN_GROUPS: [&str; 4] = ["Auto Mod", "Anti Nuke", "Server Joins", "Moderation"];
const LOCK_TABS: &[(&str, &str)] = &[("lockdown", "Manage"), ("lockdown_settings", "Settings")];
const APPEAL_TABS: &[(&str, &str)] = &[("appeal_list", "List"), ("appeals", "Settings"), ("appeal_blacklist", "Blacklist")];
const RAID_TABS: &[(&str, &str)] = &[("joinraid", "Settings"), ("raid_log", "Raid Log")];
const REPORT_TABS: &[(&str, &str)] = &[("report_list", "List"), ("report_blacklist", "Blacklist")];
const WL_TABS: &[(&str, &str)] = &[("whitelist", "Spam"), ("whitelist/invites", "Invite Links"), ("whitelist/mentions", "Mentions"), ("whitelist/everyone", "@everyone")];
const SPAM_TABS: &[(&str, &str)] = &[("filters/spam_text", "Text messages"), ("filters/spam_mentions", "Mentions"), ("filters/spam_attach", "Attachments")];
/// (sub page, title, description) for the Filters index.
const FILTER_PAGES: &[(&str, &str, &str)] = &[
    ("spam_text", "Anti Spam", "Target message, mention, attachment spam and a lot more."),
    ("invites", "Invite Links", "Target Discord Server invites sent by advertisers."),
    ("malicious", "Malicious Links", "Target phishing/malicious/scam websites sent in your server."),
    ("nsfw", "NSFW Links", "Target NSFW websites posted in SFW channels."),
    ("words", "Word Blacklist", "Target premade profanity words or add your own."),
    ("links", "Link Blacklist", "Target websites found in messages that you manage."),
];

/// (small label, big title) page header; no title means the server name.
fn title(p: &str) -> (String, Option<String>) {
    let two = |a: &str, b: &str| (a.to_string(), Some(b.to_string()));
    match p {
        "permits" => two("Permits", "Customize your permissions."),
        "logging" => ("Custom Logging".into(), None),
        _ if p.starts_with("lockdown") => two("Lockdown", "Manage your lockdown modes"),
        _ if p.starts_with("appeal") => ("Appeals".into(), None),
        _ if p.starts_with("report") => ("Reports".into(), None),
        "wizard" => ("Wizard".into(), Some(String::new())),
        "discord_automod" => ("Discord built-in AutoMod".into(), None),
        "automod" => two("Automod", "General Settings"),
        "filters" => two("Automod", "Filters"),
        "whitelist" => two("Automod", "Whitelist"),
        "antinuke" => two("Anti Nuke", "General Settings"),
        "antinuke_filters" => two("Anti Nuke", "Filters"),
        "joingate" => two("Join gate", "General Settings"),
        "joinraid" | "raid_log" => ("Join Raid".into(), None),
        "verification" => two("Verification", "General Settings"),
        "automod_panic" => ("Auto Mod Panic Mode".into(), None),
        "antinuke_panic" => ("Anti Nuke Panic Mode".into(), None),
        _ => (NAV.iter().flat_map(|(_, i)| i.iter()).chain(TOP).find(|(k, _)| *k == p).map_or_else(|| p.replace('_', " "), |(_, t)| t.to_string()), None),
    }
}

#[component]
fn Tabs(guild: String, items: &'static [(&'static str, &'static str)], active: String) -> impl IntoView {
    view! { <div class="tabs">{items.iter().map(|(p, t)| view! { <a class:on=*p == active href=format!("#/{guild}/{p}")>{*t}</a> }).collect_view()}</div> }
}

#[component]
fn Bell(route: ReadSignal<Vec<String>>) -> impl IntoView {
    let rows = RwSignal::new(Vec::<Value>::new());
    let appeals = RwSignal::new(Vec::<Value>::new());
    let fetch = move || spawn_local(async move { if let Ok(v) = api("GET", "/api/appeal/notifications", None).await { appeals.set(v.as_array().cloned().unwrap_or_default()) } });
    fetch();
    let guild = move || route.get_untracked().first().cloned().filter(|g| g.bytes().all(|b| b.is_ascii_digit()));
    let load = move |_| {
        // Opening the bell shows the verdicts once; the next fetch no longer lists them.
        if !appeals.get_untracked().is_empty() { spawn_local(async move { let _ = api("POST", "/api/appeal/notifications", None).await; }); }
        let Some(g) = guild() else { return };
        spawn_local(async move { if let Ok(v) = api("GET", &format!("/api/guilds/{g}/history"), None).await { rows.set(v.as_array().cloned().unwrap_or_default()) } });
    };
    view! { <details class="ico" on:toggle=load><summary aria-label="notifications">{icons::icon("bell")}{move || (!appeals.get().is_empty()).then(|| format!(" {}", appeals.get().len()))}</summary><div>
        {move || appeals.get().into_iter().map(|a| view! { <a href=format!("#/appeal/{}", a["guild"].as_str().unwrap_or(""))>{format!("Your {} appeal #{} was {}", a["kind"].as_str().unwrap_or(""), a["id"], if a["status"] == "accepted" { "approved" } else { "denied" })}</a> }).collect_view()}
        {move || if rows.get().is_empty() { view! { <p class="muted">"No recent changes"</p> }.into_any() } else {
            let history = format!("#/{}/history", guild().unwrap_or_default());
            rows.get().into_iter().take(5).map(|r| view! { <a href=history.clone()>{format!("v{} · {}", r["version"], r["at"].as_str().unwrap_or("").get(..16).unwrap_or(""))}</a> }).collect_view().into_any() }}
    </div></details> }
}

fn toggle_theme() {
    let Some(root) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.document_element()) else { return };
    let next = if root.get_attribute("data-theme").as_deref() == Some("light") { "dark" } else { "light" };
    let _ = root.set_attribute("data-theme", next);
}

pub(crate) fn hash() -> Vec<String> {
    let h = web_sys::window().unwrap().location().hash().unwrap_or_default();
    h.trim_start_matches('#').split('/').filter(|s| !s.is_empty()).map(String::from).collect()
}

#[component]
fn History(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (diff, set_diff) = signal(String::new());
    let version = RwSignal::new(0i64);
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let load = {
        let g = guild.clone();
        move || { let g = g.clone(); spawn_local(async move {
            if let Ok(v) = api("GET", &format!("/api/guilds/{g}/history"), None).await {
                version.set(v[0]["version"].as_i64().unwrap_or(0));
                set_rows.set(v.as_array().cloned().unwrap_or_default());
            }
        }) }
    };
    load();
    // Import is previewed first: the server answers with the diff and the ids it will clear.
    let preview = RwSignal::new(None::<(Value, Value)>);
    let shown = Memo::new(move |prev: Option<&Option<(Value, Value)>>| preview.get().or(prev.cloned().flatten()));
    let export = { let g = guild.clone(); move |_| { let g = g.clone(); spawn_local(async move {
        match api("GET", &format!("/api/guilds/{g}/config"), None).await {
            Ok(v) => download(&format!("tommybot-settings-{g}.json"), &format!("data:application/json;charset=utf-8,{}",
                js_sys::encode_uri_component(&serde_json::to_string_pretty(&json!({"version": v["version"], "config": v["config"]})).unwrap_or_default()))),
            Err(e) => msg.err(e),
        }}) } };
    let send = { let g = guild.clone(); move |cfg: Value, apply: bool| { let g = g.clone(); async move {
        api("POST", &format!("/api/guilds/{g}/import"), Some(json!({"expected_version": version.get_untracked(), "config": cfg, "apply": apply}))).await
    } } };
    let import = { let send = send.clone(); move |e: leptos::ev::Event| {
        let input = event_target::<web_sys::HtmlInputElement>(&e);
        let Some(file) = input.files().and_then(|l| l.get(0)) else { return };
        input.set_value("");
        let send = send.clone();
        spawn_local(async move {
            let text = wasm_bindgen_futures::JsFuture::from(file.text()).await.ok().and_then(|t| t.as_string()).unwrap_or_default();
            let Ok(f) = serde_json::from_str::<Value>(&text) else { return msg.err("This file is not valid JSON") };
            let cfg = f["config"].clone();
            run_act(send(cfg.clone(), false), msg, need, move |p| preview.set(Some((cfg, p))));
        });
    } };
    let apply = { let load = load.clone(); move |_| {
        let Some((cfg, _)) = preview.get_untracked() else { return };
        preview.set(None);
        let load = load.clone();
        run_act(send(cfg, true), msg, need, move |v| {
            load();
            let cleared = csv(&v["cleared"]);
            msg.ok(if cleared.is_empty() { format!("Imported as version {}", v["version"]) } else { format!("Imported as version {}. Set these again, their ids were cleared: {cleared}", v["version"]) });
        });
    } };
    let file = NodeRef::<leptos::html::Input>::new();
    let (g1, g2) = (guild.clone(), guild);
    view! {
        <div class="row">
            <button class="ghost" on:click=export>"Export settings"</button>
            <button class="ghost" on:click=move |_| { if let Some(i) = file.get() { i.click() } }>"Import settings"</button>
            <input type="file" accept=".json,application/json" hidden=true node_ref=file on:change=import />
        </div>
        <AnimatedShow when=Signal::derive(move || preview.get().is_some()) hide_class="out" hide_delay=OUT>{let apply = apply.clone(); move || shown.get().map(|(_, p)| {
            let apply = apply.clone();
            let r = NodeRef::new();
            focus_on_open(r);
            let changes = p["diff"].as_array().cloned().unwrap_or_default();
            let cleared = csv(&p["cleared"]);
            let lines = changes.iter().map(|c| format!("{}: {}", c["path"].as_str().unwrap_or(""), if c["new"].is_null() { "removed".into() } else { c["new"].to_string() })).collect::<Vec<_>>().join("\n");
            view! { <div class="modal" node_ref=r role="dialog" aria-modal="true" aria-label="Import settings" on:keydown=move |e| dialog_keys(&e, || preview.set(None))><div class="card">
                <b>"Import settings"</b>
                {if changes.is_empty() { view! { <p class="muted">"This file changes nothing on this server."</p> }.into_any() } else { view! { <p>{format!("{} changes:", changes.len())}</p><pre>{lines}</pre> }.into_any() }}
                {(!cleared.is_empty()).then(|| view! { <p class="info">{format!("Ids from the other server are cleared; set these again after the import: {cleared}")}</p> })}
                <div class="row"><button class="ghost" on:click=move |_| preview.set(None)>"Abort"</button><button disabled=changes.is_empty() on:click=apply>"Import"</button></div>
            </div></div> }
        })}</AnimatedShow>
        <For each=move || rows.get() key=|r| r["version"].as_i64() let:r>
            {
                let v = r["version"].as_i64().unwrap_or(0);
                let (g1, g2, load) = (g1.clone(), g2.clone(), load.clone());
                view! {
                    <div class="card row">
                        <b>{format!("v{v}")}</b>
                        <span class="muted">{format!("by {} at {}", r["changed_by"].as_str().unwrap_or("?"), r["at"].as_str().unwrap_or(""))}</span>
                        <button class="ghost" on:click=move |_| { let g = g1.clone(); spawn_local(async move {
                            let cur = version.get_untracked();
                            match api("GET", &format!("/api/guilds/{g}/history/{v}/{cur}"), None).await {
                                Ok(d) => set_diff.set(format!("Changes from v{v} to current (v{cur}):\n{}", serde_json::to_string_pretty(&d).unwrap_or_default())),
                                Err(e) => set_diff.set(e),
                            }}) }>"Diff vs current"</button>
                        <button on:click=move |_| { let (g, load) = (g2.clone(), load.clone()); run_save(async move {
                            let r = api("POST", &format!("/api/guilds/{g}/history/{v}/rollback"), Some(json!({"expected_version": version.get_untracked()}))).await;
                            load(); r }, version, msg, need) }>"Roll back"</button>
                    </div>
                }
            }
        </For>
        <Toast msg />
        <StepUp need enrolled />
        <pre>{move || diff.get()}</pre>
    }
}

#[component]
fn App() -> impl IntoView {
    let (route, set_route) = signal(hash());
    window_event_listener(leptos::ev::hashchange, move |_| set_route.set(hash()));
    let (me, set_me) = signal(None::<Value>);
    let (enrolled, set_enrolled) = signal(false);
    let (guilds, set_guilds) = signal(Vec::<Value>::new());
    provide_context(guilds);
    provide_context(me);
    let search = Search { q: RwSignal::new(String::new()), index: RwSignal::new(vec![]) };
    provide_context(search);
    let pages = StoredValue::new(TOP.iter().chain(NAV.iter().flat_map(|(_, i)| i.iter())).copied().collect::<Vec<_>>());
    let guild = Signal::derive(move || route.get().first().cloned().filter(|g| g.bytes().all(|b| b.is_ascii_digit())));
    let (nav_open, set_nav_open) = signal(false);
    window_event_listener(leptos::ev::keydown, move |e| if e.key() == "Escape" { set_nav_open.set(false) });
    let (filter, set_filter) = signal(String::new());
    let (guilds_note, set_guilds_note) = signal(String::from("Loading servers..."));
    let load_guilds = move || async move {
        match api("GET", "/api/guilds", None).await {
            Ok(g) => {
                let list = g.as_array().cloned().unwrap_or_default();
                set_guilds_note.set(if list.is_empty() { NO_SERVERS.into() } else { String::new() });
                set_guilds.set(list);
            }
            Err(e) => set_guilds_note.set(format!("Could not load servers: {e}")),
        }
    };
    spawn_local(async move {
        if let Ok(v) = api("GET", "/api/me", None).await {
            CSRF.with(|c| *c.borrow_mut() = v["csrf"].as_str().unwrap_or("").to_string());
            set_enrolled.set(v["totp_enrolled"].as_bool().unwrap_or(false));
            set_me.set(Some(v));
            load_guilds().await;
        }
    });
    view! {
        <Show when=move || me.get().is_some() fallback=move || if route.get().first().is_some_and(|a| a == "commands") { view! { <main><Commands /></main> }.into_any() } else { view! { <main><img class="hero" src="/logo.svg" alt="" width="128" height="128" /><h1>"TommyBot"</h1><p class="muted">"Fast, free server security: anti-nuke, automod, join gate and logs, all set up from here."</p><a class="btn" href=format!("/auth/login?next=/{}", js_sys::encode_uri_component(&web_sys::window().unwrap().location().hash().unwrap_or_default()))>"Log in with Discord"</a>" "<a class="ghost" href="#/commands">"Commands"</a></main> }.into_any() }>
            <header>
                <button class="ghost menu-btn" aria-label="menu" aria-expanded=move || nav_open.get().to_string() on:click=move |_| set_nav_open.update(|o| *o = !*o)>{icons::icon("menu")}</button>
                <a class="logo" href="#/"><img src="/logo.svg" alt="" width="32" height="32" />"TommyBot"</a><Show when=move || guild.get().is_some()><SearchBox guild pages=pages.get_value() /></Show><span class="sp"></span>
                <a class="ghost" href="#/commands">"Commands"</a>
                <Show when=move || me.get().is_some_and(|m| m["bot_owner"] == true)><a class="ghost" href="#/update">"Update"</a></Show>
                <Bell route />
                <button class="ghost" aria-label="toggle light and dark theme" on:click=|_| toggle_theme()>{icons::icon("moon")}</button>
                <details>
                    <summary aria-label="account menu">{move || me.get().and_then(|m| m["username"].as_str().and_then(|u| u.chars().next()).map(|c| c.to_uppercase().to_string())).unwrap_or_default()}</summary>
                    <div><a href="#/">"Servers"</a><button on:click=move |_| spawn_local(async { let _ = api("POST", "/auth/logout", None).await; web_sys::window().unwrap().location().reload().ok(); })>"Logout"</button></div>
                </details>
            </header>
            <div class="shell">
                <Show when=move || nav_open.get()><div class="scrim" on:click=move |_| set_nav_open.set(false)></div></Show>
                <nav class:open=move || nav_open.get() on:click=move |_| set_nav_open.set(false)>
                    {move || match route.get().first() {
                        None => view! { <a href="#/">"Servers"</a> }.into_any(),
                        Some(a) if a == "commands" || a == "update" => view! { <a href="#/">"Servers"</a> }.into_any(),
                        Some(a) if a == "appeal" => ().into_any(),
                        Some(g) => {
                            let g = g.clone();
                            let page = route.get().get(1).cloned().unwrap_or_default();
                            let hits = search::counts(search);
                            let link = |p: &str, t: &str| { let n: usize = hits.iter().filter(|(k, _)| *k == p || k.starts_with(&format!("{p}/"))).map(|(_, n)| n).sum(); let on = page == p || (p == "appeal_list" && page.starts_with("appeal")) || (p == "report_list" && page.starts_with("report")) || (p == "lockdown" && page.starts_with("lockdown")); view! { <a class:on=on href=format!("#/{g}/{p}")>{t.to_string()}{(n > 0).then(|| view! { <span class="chip hits">{n}</span> })}</a> } };
                            let gi = guilds.get().into_iter().find(|x| x["id"] == g.as_str());
                            let name = gi.as_ref().and_then(|x| x["name"].as_str()).unwrap_or("Server").to_string();
                            let icon = match gi.as_ref().and_then(|x| x["icon"].as_str()) {
                                Some(i) => view! { <img src=format!("https://cdn.discordapp.com/icons/{g}/{i}.png?size=64") /> }.into_any(),
                                None => view! { <i>{name.chars().next().unwrap_or('?').to_string()}</i> }.into_any(),
                            };
                            view! {
                                <a class="srv" href="#/" title="Switch server">{icon}{name}<button class="refresh" on:click=move |e| { e.prevent_default(); e.stop_propagation(); spawn_local(load_guilds()) }>"refresh server"</button></a>
                                {TOP.iter().map(|(p, t)| link(p, t)).collect_view()}
                                {NAV.iter().map(|(grp, items)| view! { <details open=OPEN_GROUPS.contains(grp) || items.iter().any(|(p, _)| *p == page)><summary>{*grp}</summary>{items.iter().map(|(p, t)| link(p, t)).collect_view()}</details> }).collect_view()}
                            }.into_any()
                        }
                    }}
                </nav>
                <main>
                    {move || match route.get().as_slice() {
                        [] => view! {
                            <div class="head"><h1>"Servers"</h1><input placeholder="Filter: [Type name or ID]" aria-label="filter servers" prop:value=filter on:input=move |e| set_filter.set(event_target_value(&e)) /><LostControl enrolled /><button class="ghost" on:click=move |_| { set_guilds_note.set("Loading servers...".into()); spawn_local(load_guilds()) }>"Refresh servers"</button></div>
                            {move || if guilds_note.get() == NO_SERVERS { view! { <div class="card empty">
                                <h2>"No servers you can manage."</h2>
                                <ol>
                                    <li>"To manage a server you need to be either:"<ul>
                                        <li>"The "<b>"physical"</b>" server owner (the one with the crown)."</li>
                                        <li>"An "<b>"Extra Owner"</b>" (added by the server owner in Permits)."</li>
                                        <li>"A "<b>"Trusted Admin"</b>" (added by the server owner in Permits)."</li>
                                        <li>"A member whose permit can view the dashboard."</li></ul></li>
                                    <li>"Is the server freshly made, or TommyBot just added? "<button class="go" on:click=move |_| { set_guilds_note.set("Loading servers...".into()); spawn_local(load_guilds()) }>"Refresh"</button></li>
                                </ol></div> }.into_any() } else { view! { <p class="muted">{guilds_note.get()}</p> }.into_any() }}
                            <div class="cards">{guilds.get().iter().filter(|g| { let f = filter.get().to_lowercase(); g["name"].as_str().unwrap_or("").to_lowercase().contains(&f) || g["id"].as_str().unwrap_or("").contains(&f) }).map(|g| view! { <div class="card"><a class="gcard" href=format!("#/{}/overview", g["id"].as_str().unwrap_or(""))>{match g["icon"].as_str() {
                                Some(i) => view! { <img src=format!("https://cdn.discordapp.com/icons/{}/{i}.png?size=64", g["id"].as_str().unwrap_or("")) /> }.into_any(),
                                None => view! { <i>{g["name"].as_str().unwrap_or("?").chars().next().unwrap_or('?').to_string()}</i> }.into_any() }}
                                <b>{g["name"].as_str().unwrap_or("").to_string()}</b><span class="muted">{g["level"].as_str().unwrap_or("").to_string()}</span></a></div> }).collect_view()}</div>
                        }.into_any(),
                        [a] if a == "commands" => view! { <Commands /> }.into_any(),
                        [a] if a == "update" => view! { <Update enrolled /> }.into_any(),
                        [a, g] if a == "appeal" =>view! { <AppealForm guild=g.clone() /> }.into_any(),
                        [g] => view! { <Head guild=g.clone() page=title("overview").0 /><Score guild=g.clone() enrolled /> }.into_any(),
                        [g, p, rest @ ..] => {
                            let sec = |name: &str, part: &str| view! { <Section guild=g.clone() name=name.to_string() part=part.to_string() enrolled /> }.into_any();
                            let page = match (p.as_str(), rest.first()) {
                                ("permits", _) => view! { <Permits guild=g.clone() enrolled /> }.into_any(),
                                ("logging", _) => view! { <Logging guild=g.clone() tab=rest.first().cloned().unwrap_or_default() sub=rest.get(1).cloned().unwrap_or_default() enrolled /> }.into_any(),
                                ("embeds", _) => view! { <Embeds guild=g.clone() /> }.into_any(),
                                ("history", _) => view! { <History guild=g.clone() enrolled /> }.into_any(),
                                ("overview", _) => view! { <Score guild=g.clone() enrolled /> }.into_any(),
                                ("wizard", _) => view! { <Wizard guild=g.clone() enrolled /> }.into_any(),
                                ("revert", _) => view! { <Revert guild=g.clone() enrolled /> }.into_any(),
                                ("backups", _) => view! { <Backups guild=g.clone() enrolled /> }.into_any(),
                                ("channel_setup", _) => view! { <ChannelSetup guild=g.clone() enrolled /> }.into_any(),
                                ("automod_panic", _) => view! { <Panic guild=g.clone() part="heat,general" enrolled /> }.into_any(),
                                ("antinuke_panic", _) => view! { <Panic guild=g.clone() part="nuke,general" enrolled /> }.into_any(),
                                ("tickets", Some(l)) if l == "log" && rest.len() == 3 => view! { <TicketLog guild=g.clone() channel=rest[1].clone() message=rest[2].clone() /> }.into_any(),
                                ("tickets", _) => view! { <Tickets guild=g.clone() enrolled /> }.into_any(),
                                ("rescue", _) => view! { <Rescue guild=g.clone() enrolled /> }.into_any(),
                                ("misc", _) => view! { {sec("misc", "")}<PurgeData guild=g.clone() enrolled /> }.into_any(),
                                ("lockdown", _) => view! { <Tabs guild=g.clone() items=LOCK_TABS active=p.clone() /><LockdownManage guild=g.clone() /> }.into_any(),
                                ("lockdown_settings", _) => view! { <Tabs guild=g.clone() items=LOCK_TABS active=p.clone() /><LockdownSettings guild=g.clone() enrolled /> }.into_any(),
                                ("discord_automod", _) => view! { <DiscordRules guild=g.clone() /> }.into_any(),
                                ("appeal_list", _) => view! { <Tabs guild=g.clone() items=APPEAL_TABS active=p.clone() /><AppealList guild=g.clone() /> }.into_any(),
                                ("appeal_blacklist", _) => view! { <Tabs guild=g.clone() items=APPEAL_TABS active=p.clone() /><Blacklist guild=g.clone() base="appeals" purge=true /> }.into_any(),
                                ("joinraid", _) => view! { <Tabs guild=g.clone() items=RAID_TABS active=p.clone() />{sec("joinraid", "")} }.into_any(),
                                ("raid_log", _) => view! { <Tabs guild=g.clone() items=RAID_TABS active=p.clone() /><RaidLog guild=g.clone() /> }.into_any(),
                                ("catch", _) => view! { <Catch guild=g.clone() enrolled /> }.into_any(),
                                ("cases", _) => view! { <CaseList guild=g.clone() /> }.into_any(),
                                ("notes", _) => view! { <NoteList guild=g.clone() /> }.into_any(),
                                ("report_list", _) => view! { <Tabs guild=g.clone() items=REPORT_TABS active=p.clone() /><ReportList guild=g.clone() /> }.into_any(),
                                ("report_blacklist", _) => view! { <Tabs guild=g.clone() items=REPORT_TABS active=p.clone() /><Blacklist guild=g.clone() base="reports" /> }.into_any(),
                                ("appeals", _) => view! { <Tabs guild=g.clone() items=APPEAL_TABS active=p.clone() />{sec("appeals", "")} }.into_any(),
                                ("filters", None) => view! { <div class="stack">{FILTER_PAGES.iter().map(|(sub, t, d)| view! { <a class="card link" href=format!("#/{g}/filters/{sub}")><span class="ico"></span><span><h3>{*t}</h3><span class="muted">{*d}</span></span><span class="sp"></span>"›"</a> }).collect_view()}</div> }.into_any(),
                                ("filters", Some(sub)) => {
                                    let tabs = sub.starts_with("spam_").then(|| view! { <Tabs guild=g.clone() items=SPAM_TABS active=format!("filters/{sub}") /> });
                                    let name = FILTER_PAGES.iter().find(|(s, ..)| sub.starts_with(s) || (*s == "spam_text" && sub.starts_with("spam_"))).map_or("", |(_, t, _)| *t);
                                    view! { <p><a class="ghost" href=format!("#/{g}/filters")>"Go Back"</a></p><h2 style="text-align:center">{name}</h2>{tabs}<h3 style="text-align:center">{match sub.as_str() { "spam_text" => "Text Message Filters", "spam_mentions" => "Mention Filters", "spam_attach" => "Attachment Filters", _ => "" }}</h3>{sec("automod", sub)} }.into_any()
                                }
                                ("whitelist", sub) => {
                                    let active = sub.map_or_else(|| "whitelist".to_string(), |s| format!("whitelist/{s}"));
                                    view! { <Tabs guild=g.clone() items=WL_TABS active />{sec("whitelist", sub.map_or("general", String::as_str))} }.into_any()
                                }
                                ("automod", _) => sec("automod", "general"),
                                ("antinuke", _) => sec("antinuke", "general"),
                                ("antinuke_filters", _) => sec("antinuke", "limits"),
                                _ => sec(p, ""),
                            };
                            { let (page_name, big) = title(p); let head = match big { Some(b) => view! { <Head guild=g.clone() page=page_name big=b /> }.into_any(), None => view! { <Head guild=g.clone() page=page_name /> }.into_any() }; view! { {head}{page} }.into_any() }
                        }
                    }}
                </main>
            </div>
        </Show>
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        console_error_panic_hook::hook(info);
        if let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) {
            let _ = body.insert_adjacent_text("beforeend", &format!("Dashboard crashed: {info}"));
        }
    }));
    leptos::mount::mount_to_body(App);
}

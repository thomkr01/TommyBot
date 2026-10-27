use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

mod appeals;
mod channels;
mod records;
mod common;
mod embeds;
mod form;
mod lockdown;
mod pages;
mod permits;
mod recovery;
mod wizard;
use lockdown::{DiscordRules, LockdownManage, LockdownSettings};
use appeals::{AppealForm, AppealList, Blacklist};
use records::{CaseList, NoteList, RaidLog, ReportList};
use common::*;
use embeds::Embeds;
use form::Section;
use permits::Permits;
use pages::{Logging, Rescue, Revert, Score, Wizard};
use recovery::{Backups, Panic};
use channels::ChannelSetup;

/// Sidebar like Wick's, only with the pages TommyBot has. Untyped sections stay reachable under "Raw JSON".
const TOP: &[(&str, &str)] = &[("wizard", "Wizard"), ("overview", "Overview"), ("misc", "Miscellaneous"), ("permits", "Permits"), ("logging", "Logging"), ("backups", "Backups"), ("channel_setup", "Channel Setup"), ("lockdown", "Lockdown"), ("appeal_list", "Appeals")];
const NAV: &[(&str, &[(&str, &str)])] = &[
    ("Auto Mod", &[("automod", "General"), ("filters", "Filters"), ("discord_automod", "Discord"), ("whitelist", "Whitelist"), ("automod_panic", "Panic Mode")]),
    ("Anti Nuke", &[("antinuke", "General"), ("antinuke_filters", "Filters"), ("antinuke_panic", "Panic Mode")]),
    ("Server Joins", &[("joingate", "Join Gate"), ("joinraid", "Join Raid"), ("verification", "Verification")]),
    ("Moderation", &[("cases", "Cases"), ("notes", "Notes"), ("report_list", "Reports")]),
    ("Tools", &[("revert", "Revert Center"), ("history", "History"), ("embeds", "Embed Builder"), ("rescue", "Rescue Key")]),
    ("Raw JSON", &[("condition_automod", "Condition Automod"), ("moderation", "Moderation"), ("warn_points", "Warn Points"), ("punish", "Punish"), ("utility", "Utility")]),
];
/// Groups Wick has; the rest are TommyBot extras that stay folded until visited.
const WICK_GROUPS: [&str; 3] = ["Auto Mod", "Anti Nuke", "Server Joins"];
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

/// (small label, big title) like Wick's page header; no title means the server name.
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
    let load = move |_| {
        let Some(g) = route.get_untracked().first().cloned().filter(|g| g.bytes().all(|b| b.is_ascii_digit())) else { return };
        spawn_local(async move { if let Ok(v) = api("GET", &format!("/api/guilds/{g}/history"), None).await { rows.set(v.as_array().cloned().unwrap_or_default()) } });
    };
    view! { <details class="ico" on:toggle=load><summary aria-label="notifications">"🔔"</summary><div>
        {move || if rows.get().is_empty() { view! { <a href="#/">"No recent changes"</a> }.into_any() } else {
            rows.get().into_iter().take(5).map(|r| view! { <a href="#/">{format!("v{} · {}", r["version"], r["at"].as_str().unwrap_or("").get(..16).unwrap_or(""))}</a> }).collect_view().into_any() }}
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
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
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
    let (g1, g2) = (guild.clone(), guild);
    view! {
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
        <p>{move || msg.get()}</p>
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
    let (nav_open, set_nav_open) = signal(false);
    let (guilds_note, set_guilds_note) = signal(String::from("Loading servers..."));
    let load_guilds = move || async move {
        match api("GET", "/api/guilds", None).await {
            Ok(g) => {
                let list = g.as_array().cloned().unwrap_or_default();
                set_guilds_note.set(if list.is_empty() { "No servers: the bot must be in the server, and you must be its owner, an extra owner, a trusted admin or hold a permit with dashboard_view.".into() } else { String::new() });
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
        <Show when=move || me.get().is_some() fallback=|| view! { <main><h1>"TommyBot"</h1><a href=format!("/auth/login?next=/{}", js_sys::encode_uri_component(&web_sys::window().unwrap().location().hash().unwrap_or_default()))><button>"Log in with Discord"</button></a></main> }>
            <header>
                <button class="ghost menu-btn" aria-label="menu" on:click=move |_| set_nav_open.update(|o| *o = !*o)>"☰"</button>
                <a class="logo" href="#/">"TommyBot"</a><span class="sp"></span>
                <details class="ico"><summary aria-label="language">"🌐"</summary><div><button>"English"</button></div></details>
                <Bell route />
                <button class="ghost" aria-label="toggle light and dark theme" on:click=|_| toggle_theme()>"🌓"</button>
                <details>
                    <summary>{move || me.get().and_then(|m| m["username"].as_str().and_then(|u| u.chars().next()).map(|c| c.to_uppercase().to_string())).unwrap_or_default()}</summary>
                    <div><a href="#/">"Servers"</a><button on:click=move |_| spawn_local(async { let _ = api("POST", "/auth/logout", None).await; web_sys::window().unwrap().location().reload().ok(); })>"Logout"</button></div>
                </details>
            </header>
            <div class="shell">
                <nav class:open=move || nav_open.get() on:click=move |_| set_nav_open.set(false)>
                    {move || match route.get().first() {
                        None => view! { <a href="#/">"Servers"</a> }.into_any(),
                        Some(a) if a == "appeal" => ().into_any(),
                        Some(g) => {
                            let g = g.clone();
                            let page = route.get().get(1).cloned().unwrap_or_default();
                            let link = |p: &str, t: &str| { let on = page == p || (p == "appeal_list" && page.starts_with("appeal")) || (p == "report_list" && page.starts_with("report")) || (p == "lockdown" && page.starts_with("lockdown")); view! { <a class:on=on href=format!("#/{g}/{p}")>{t.to_string()}{matches!(p, "logging" | "discord_automod").then(|| view! { <span class="chip new">"new"</span> })}</a> } };
                            let gi = guilds.get().into_iter().find(|x| x["id"] == g.as_str());
                            let name = gi.as_ref().and_then(|x| x["name"].as_str()).unwrap_or("Server").to_string();
                            let icon = match gi.as_ref().and_then(|x| x["icon"].as_str()) {
                                Some(i) => view! { <img src=format!("https://cdn.discordapp.com/icons/{g}/{i}.png?size=64") /> }.into_any(),
                                None => view! { <i>{name.chars().next().unwrap_or('?').to_string()}</i> }.into_any(),
                            };
                            view! {
                                <a class="srv" href="#/" title="Switch server">{icon}{name}<button class="refresh" on:click=move |e| { e.prevent_default(); e.stop_propagation(); spawn_local(load_guilds()) }>"refresh server"</button></a>
                                {TOP.iter().map(|(p, t)| link(p, t)).collect_view()}
                                {NAV.iter().map(|(grp, items)| view! { <details open=WICK_GROUPS.contains(grp) || items.iter().any(|(p, _)| *p == page)><summary>{*grp}</summary>{items.iter().map(|(p, t)| link(p, t)).collect_view()}</details> }).collect_view()}
                            }.into_any()
                        }
                    }}
                </nav>
                <main>
                    {move || match route.get().as_slice() {
                        [] => view! {
                            <div class="head"><h1>"Servers"</h1><button class="ghost" on:click=move |_| { set_guilds_note.set("Loading servers...".into()); spawn_local(load_guilds()) }>"Refresh servers"</button></div>
                            <p class="muted">{move || guilds_note.get()}</p>
                            <div class="cards">{guilds.get().iter().map(|g| view! { <div class="card"><a class="gcard" href=format!("#/{}/overview", g["id"].as_str().unwrap_or(""))>{match g["icon"].as_str() {
                                Some(i) => view! { <img src=format!("https://cdn.discordapp.com/icons/{}/{i}.png?size=64", g["id"].as_str().unwrap_or("")) /> }.into_any(),
                                None => view! { <i>{g["name"].as_str().unwrap_or("?").chars().next().unwrap_or('?').to_string()}</i> }.into_any() }}
                                <b>{g["name"].as_str().unwrap_or("").to_string()}</b><span class="muted">{g["level"].as_str().unwrap_or("").to_string()}</span></a></div> }).collect_view()}</div>
                        }.into_any(),
                        [a, g] if a == "appeal" => view! { <AppealForm guild=g.clone() /> }.into_any(),
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
                                ("rescue", _) => view! { <Rescue guild=g.clone() enrolled /> }.into_any(),
                                ("lockdown", _) => view! { <Tabs guild=g.clone() items=LOCK_TABS active=p.clone() /><LockdownManage guild=g.clone() /> }.into_any(),
                                ("lockdown_settings", _) => view! { <Tabs guild=g.clone() items=LOCK_TABS active=p.clone() /><LockdownSettings guild=g.clone() enrolled /> }.into_any(),
                                ("discord_automod", _) => view! { <DiscordRules guild=g.clone() /> }.into_any(),
                                ("appeal_list", _) => view! { <Tabs guild=g.clone() items=APPEAL_TABS active=p.clone() /><AppealList guild=g.clone() /> }.into_any(),
                                ("appeal_blacklist", _) => view! { <Tabs guild=g.clone() items=APPEAL_TABS active=p.clone() /><Blacklist guild=g.clone() base="appeals" purge=true /> }.into_any(),
                                ("joinraid", _) => view! { <Tabs guild=g.clone() items=RAID_TABS active=p.clone() />{sec("joinraid", "")} }.into_any(),
                                ("raid_log", _) => view! { <Tabs guild=g.clone() items=RAID_TABS active=p.clone() /><RaidLog guild=g.clone() /> }.into_any(),
                                ("cases", _) => view! { <CaseList guild=g.clone() /> }.into_any(),
                                ("notes", _) => view! { <NoteList guild=g.clone() /> }.into_any(),
                                ("report_list", _) => view! { <Tabs guild=g.clone() items=REPORT_TABS active=p.clone() /><ReportList guild=g.clone() /> }.into_any(),
                                ("report_blacklist", _) => view! { <Tabs guild=g.clone() items=REPORT_TABS active=p.clone() /><Blacklist guild=g.clone() base="reports" /> }.into_any(),
                                ("appeals", _) => view! { <Tabs guild=g.clone() items=APPEAL_TABS active=p.clone() />{sec("appeals", "")} }.into_any(),
                                ("filters", None) => view! { <div class="stack">{FILTER_PAGES.iter().map(|(sub, t, d)| view! { <a class="card link" href=format!("#/{g}/filters/{sub}")><span class="ico"></span><span><h3>{*t}</h3><span class="muted">{*d}</span></span><span class="sp"></span>"›"</a> }).collect_view()}</div> }.into_any(),
                                ("filters", Some(sub)) => {
                                    let tabs = sub.starts_with("spam_").then(|| view! { <Tabs guild=g.clone() items=SPAM_TABS active=format!("filters/{sub}") /> });
                                    let name = FILTER_PAGES.iter().find(|(s, ..)| sub.starts_with(s) || (*s == "spam_text" && sub.starts_with("spam_"))).map_or("", |(_, t, _)| *t);
                                    view! { <p><a href=format!("#/{g}/filters")><button class="ghost">"Go Back"</button></a></p><h2 style="text-align:center">{name}</h2>{tabs}<h3 style="text-align:center">{match sub.as_str() { "spam_text" => "Text Message Filters", "spam_mentions" => "Mention Filters", "spam_attach" => "Attachment Filters", _ => "" }}</h3>{sec("automod", sub)} }.into_any()
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

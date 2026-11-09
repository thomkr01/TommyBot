use crate::{common::*, form::Field};
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

/// Adds or removes `key` in `c.custom[i][field]` (perms / whitelist); keys the catalog doesn't list stay untouched.
fn toggle(c: &mut Value, i: usize, field: &str, key: &str, on: bool) {
    let list = &mut c["custom"][i][field];
    if !list.is_array() {
        *list = json!([]);
    }
    let a = list.as_array_mut().unwrap();
    a.retain(|x| x != key);
    if on {
        a.push(json!(key));
    }
}

fn has(c: &Value, i: usize, field: &str, key: &str) -> bool {
    c["custom"][i][field]
        .as_array()
        .is_some_and(|a| a.iter().any(|x| x == key))
}

#[component]
pub fn Permits(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let cfg = RwSignal::new(json!({}));
    let dc = RwSignal::new(json!({}));
    let catalog = RwSignal::new(json!([]));
    let version = RwSignal::new(0i64);
    let msg = Msg::default();
    let need = RwSignal::new(false);
    let open = RwSignal::new(None::<i32>); // permit being edited
    let tab = RwSignal::new(0u8);
    let orig = RwSignal::new(json!({}));
    let deleting = RwSignal::new(None::<usize>);
    let g = guild.clone();
    spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{g}/config"), None).await {
            version.set(v["version"].as_i64().unwrap_or(0));
            cfg.set(v["config"]["permits"].clone());
            orig.set(v["config"]["permits"].clone());
        }
        if let Ok(v) = api("GET", "/api/permit-catalog", None).await {
            catalog.set(v);
        }
        if let Ok(v) = api("GET", &format!("/api/guilds/{g}/discord"), None).await {
            dc.set(v);
        }
    });
    let save = move |_| {
        let (g, p, v) = (guild.clone(), cfg.get_untracked(), version.get_untracked());
        orig.set(p.clone());
        run_save(
            async move {
                api(
                    "PUT",
                    &format!("/api/guilds/{g}/permits"),
                    Some(json!({"expected_version": v, "value": p})),
                )
                .await
            },
            version,
            msg,
            need,
        );
    };
    let set_field = move |i: usize, field: &'static str, val: String| {
        cfg.update(|c| {
            c["custom"][i][field] = if field == "rank" {
                json!(val.parse::<i64>().unwrap_or(0))
            } else {
                json!(val)
            }
        })
    };
    // Dialog target: -1 Extra Owner, -2 Trusted Admin, 0.. a custom permit.
    let snap = RwSignal::new(json!({}));
    let section = RwSignal::new(None::<String>);
    let start = move |i: i32| {
        snap.set(cfg.get_untracked());
        tab.set(0);
        section.set(None);
        open.set(Some(i));
    };
    let abort = move |_| {
        cfg.set(snap.get_untracked());
        open.set(None);
    };
    let dialog = move |i: i32| {
        let n = i.max(0) as usize;
        let ids = (i < 0).then_some(if i == -1 { "eo" } else { "ta" });
        let r = NodeRef::new();
        focus_on_open(r);
        view! { <div class="modal" node_ref=r role="dialog" aria-modal="true" aria-label="Edit permit" on:keydown=move |e| dialog_keys(&e, || { cfg.set(snap.get_untracked()); open.set(None) })><div class="card">
            <div class="tabs">
                <button class:on=move || tab.get() == 0 on:click=move |_| tab.set(0)>"General"</button>
                {(i >= 0).then(|| view! { <button class:on=move || tab.get() == 1 on:click=move |_| { section.set(None); tab.set(1) }>"Permissions"</button> })}
            </div>
            <Show when=move || tab.get() == 0>
                <div class="row"><label>"Permit Name"</label><input aria-label="permit name" disabled=i < 0 prop:value=move || match ids { Some("eo") => "Extra Owner".to_string(), Some(_) => "Trusted Admin".to_string(), None => cfg.get()["custom"][n]["name"].as_str().unwrap_or("").to_string() } on:change=move |e| set_field(n, "name", event_target_value(&e)) /></div>
                {match ids {
                    Some(k) => view! { <Field f=json!({"path": k, "kind": "users", "title": "Permit Users"}) val=cfg dc /> }.into_any(),
                    None => view! {
                        <Field f=json!({"path": format!("custom.{n}.roles"), "kind": "roles", "title": "Permit Roles"}) val=cfg dc />
                        <div class="row"><label>"Rank"</label><input aria-label="rank" prop:value=move || cfg.get()["custom"][n]["rank"].to_string() on:change=move |e| set_field(n, "rank", event_target_value(&e)) /></div>
                        <p class="muted">"A higher rank number outranks a lower one."</p> }.into_any(),
                }}
            </Show>
            <Show when=move || tab.get() == 1>
                <For each=move || catalog.get().as_array().cloned().unwrap_or_default() key=|s| s["id"].as_str().unwrap_or("").to_string() let:s>
                    {
                        let id = s["id"].as_str().unwrap_or("").to_string();
                        let (id2, title) = (id.clone(), s["title"].as_str().unwrap_or("").to_string());
                        view! {
                            <Show when=move || section.get().is_none()><button class="ghost menu" on:click={let id = id.clone(); move |_| section.set(Some(id.clone()))}>{title.clone()}</button></Show>
                            <Show when={let id = id2.clone(); move || section.get().as_deref() == Some(id.as_str())}>
                                <button class="link back" on:click=move |_| section.set(None)>"Go Back"</button>
                                {s["items"].as_array().cloned().unwrap_or_default().into_iter().map(|it| {
                                    let key = it["key"].as_str().unwrap_or("").to_string();
                                    let field = if it["flag"].as_bool().unwrap_or(false) { "whitelist" } else { "perms" };
                                    let locked = it["locked"].as_bool().unwrap_or(false);
                                    let k = key.clone();
                                    view! {
                                        <div class="row tog">
                                            <input type="checkbox" class="sw" aria-label=it["label"].as_str().unwrap_or("").to_string() disabled=locked
                                                prop:checked=move || has(&cfg.get(), n, field, &k)
                                                on:change=move |e| { let on = event_target_checked(&e); let k = key.clone(); cfg.update(|c| toggle(c, n, field, &k, on)) } />
                                            <label>{it["label"].as_str().unwrap_or("").to_string()}<br/><span class="muted">{it["help"].as_str().unwrap_or("").to_string()}</span></label>
                                        </div>
                                    }
                                }).collect_view()}
                            </Show>
                        }
                    }
                </For>
            </Show>
            <div class="row">
                <button class="ghost" on:click=abort>"Abort"</button>
                <span class="sp"></span>
                <button on:click=move |_| open.set(None)>"Apply"</button>
            </div>
        </div></div> }
    };
    let delete = move |i: usize| {
        cfg.update(|c| {
            c["custom"].as_array_mut().map(|a| a.remove(i));
        })
    };
    // The last dialog stays rendered while it fades out.
    let del =
        Memo::new(move |prev: Option<&Option<usize>>| deleting.get().or(prev.copied().flatten()));
    let dlg = Memo::new(move |prev: Option<&Option<i32>>| open.get().or(prev.copied().flatten()));
    view! {
            <div class="row"><button class="go" title="Create New Permit" aria-label="create new permit" on:click=move |_| { let before = cfg.get_untracked(); cfg.update(|c| { if !c["custom"].is_array() { c["custom"] = json!([]); } c["custom"].as_array_mut().unwrap().push(json!({"name": "New permit", "rank": 0, "perms": [], "roles": [], "whitelist": [], "limits": {}})); }); start(cfg.get_untracked()["custom"].as_array().map_or(0, |a| a.len() as i32 - 1)); snap.set(before) }>"+"</button></div>
            <table class="permits"><thead><tr><th>"Name"</th><th>"Actions"</th></tr></thead><tbody>
                <tr><td><b>"Extra Owner"</b>" "<span class="chip">"OWNER"</span>" "<span class="chip">"IMMUNITY"</span></td><td><button class="ghost" title="Edit Permit" on:click=move |_| start(-1)>"Edit"</button></td></tr>
                <tr><td><b>"Trusted Admin"</b>" "<span class="chip">"IMMUNITY"</span></td><td><button class="ghost" title="Edit Permit" on:click=move |_| start(-2)>"Edit"</button></td></tr>
                {move || (0..cfg.get()["custom"].as_array().map_or(0, Vec::len)).map(|i| view! {
                    <tr><td>{format!("{} ", cfg.get()["custom"][i]["rank"])}<b>{cfg.get()["custom"][i]["name"].as_str().unwrap_or("").to_string()}</b></td><td>
                        <button class="danger" title="Delete Permit" on:click=move |_| { deleting.set(Some(i)); }>"Delete"</button>
                        <button class="ghost" title="Edit Permit" on:click=move |_| start(i as i32)>"Edit"</button></td></tr>
                }).collect_view()}
            </tbody></table>
            <AnimatedShow when=Signal::derive(move || deleting.get().is_some()) hide_class="out" hide_delay=OUT>{move || del.get().map(|n| { let r = NodeRef::new(); focus_on_open(r); view! { <div class="modal" node_ref=r role="dialog" aria-modal="true" aria-label="Delete permit" on:keydown=move |e| dialog_keys(&e, || deleting.set(None))><div class="card"><p>{format!("Delete the permit \"{}\"?", cfg.get_untracked()["custom"][n]["name"].as_str().unwrap_or(""))}</p>
                <div class="row"><button class="ghost" on:click=move |_| deleting.set(None)>"Abort"</button><button class="danger" on:click=move |_| { delete(n); deleting.set(None) }>"Delete"</button></div></div></div> } })}
    </AnimatedShow>
            <AnimatedShow when=Signal::derive(move || open.get().is_some()) hide_class="out" hide_delay=OUT>{move || dlg.get().map(|i| dialog(i))}</AnimatedShow>
            <AnimatedShow when=Signal::derive(move || cfg.get() != orig.get()) hide_class="out" hide_delay=OUT>
                <div class="bar"><b>"Changes detected!"</b><button class="ghost" on:click=move |_| cfg.set(orig.get_untracked())>"Cancel"</button><button on:click=save.clone()>"Save"</button></div>
            </AnimatedShow>
            <Toast msg />
            <StepUp need enrolled />
        }
}

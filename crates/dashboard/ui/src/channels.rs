//! Channel Setup: the server's channels plus a draft of changes. Nothing changes in Discord until Push.
//! Every edit sends the whole draft and the page shows the server's reply (tree, issues, change count).
use crate::common::*;
use crate::icons::icon;
use leptos::{html, prelude::*};
use serde_json::{json, Value};

/// Icon for a Discord channel type. Kept in one place so #164's `ChannelTree` mapping can replace it.
fn kind_icon(kind: u64) -> &'static str {
    match kind {
        2 => "channel_voice",
        4 => "category",
        5 => "announcement",
        13 => "channel_stage",
        15 => "forum",
        _ => "channel_text",
    }
}

fn kind_name(kind: u64) -> &'static str {
    match kind {
        2 => "voice channel",
        4 => "category",
        5 => "announcement channel",
        13 => "stage channel",
        15 => "forum",
        _ => "text channel",
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn arr(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(String::from))
        .collect()
}

fn is_cat(n: &Value) -> bool {
    n["kind"] == 4
}

/// The name as Discord shows it: `#name` for text-like channels.
fn label(n: &Value) -> String {
    match n["kind"].as_u64() {
        Some(2 | 4 | 13) => s(&n["name"]),
        _ => format!("#{}", s(&n["name"])),
    }
}

fn find<'a>(t: &'a [Value], id: &str) -> Option<&'a Value> {
    t.iter().find(|n| n["id"] == id)
}

/// The channel's category, when that category is in the tree (otherwise it is shown as loose).
fn parent(t: &[Value], n: &Value) -> Option<String> {
    let p = n["parent"].as_str()?;
    t.iter()
        .any(|c| is_cat(c) && c["id"] == p)
        .then(|| p.to_string())
}

/// Ids in tree order (the server sends Discord's order): the categories (`cat`), or the channels in
/// `parent` (None: no category).
fn siblings(t: &[Value], parent_id: Option<&str>, cat: bool) -> Vec<String> {
    t.iter()
        .filter(|n| is_cat(n) == cat && (cat || parent(t, n).as_deref() == parent_id))
        .map(|n| s(&n["id"]))
        .collect()
}

/// The draft item for `id` (an omitted field means unchanged).
fn item<'a>(d: &'a mut Value, id: &str) -> &'a mut Value {
    let it = &mut d["items"][id];
    if !it.is_object() {
        *it = json!({});
    }
    it
}

/// Access as the draft takes it: `limited` false is Everyone, `roles` empty and limited is staff only.
fn access(roles: &[String], staff: bool, limited: bool) -> Value {
    json!({"roles": roles, "staff": staff, "limited": limited})
}

/// Puts `id` at index `at` among the siblings in `parent` and renumbers the ones that moved.
fn place(d: &mut Value, t: &[Value], id: &str, parent_id: Option<String>, at: usize) {
    let Some(n) = find(t, id) else { return };
    let cat = is_cat(n);
    let mut list = siblings(t, parent_id.as_deref(), cat);
    list.retain(|x| x != id);
    list.insert(at.min(list.len()), id.to_string());
    if !cat && parent(t, n) != parent_id {
        item(d, id)["parent"] = json!(parent_id);
    }
    for (i, x) in list.iter().enumerate() {
        if find(t, x).and_then(|n| n["position"].as_i64()) != Some(i as i64) {
            item(d, x)["position"] = json!(i);
        }
    }
}

/// Where a row dropped on `target` goes: just before it. A channel dropped on a category goes first in
/// it; a category dropped on a channel goes before that channel's category.
fn drop_target(t: &[Value], dragged: &str, target: &str) -> Option<(Option<String>, usize)> {
    let (d, tg) = (find(t, dragged)?, find(t, target)?);
    if dragged == target {
        return None;
    }
    if is_cat(d) {
        let c = if is_cat(tg) {
            target.to_string()
        } else {
            parent(t, tg)?
        };
        let list: Vec<String> = siblings(t, None, true)
            .into_iter()
            .filter(|x| x != dragged)
            .collect();
        return Some((None, list.iter().position(|x| *x == c)?));
    }
    if is_cat(tg) {
        return Some((Some(target.to_string()), 0));
    }
    let p = parent(t, tg);
    let list: Vec<String> = siblings(t, p.as_deref(), false)
        .into_iter()
        .filter(|x| x != dragged)
        .collect();
    Some((p, list.iter().position(|x| x == target)?))
}

/// Applies an issue's fix to the draft: `{strip: target}` adds the target to the item's strip list,
/// `{access}` sets the item's access.
fn apply_fix(d: &mut Value, id: &str, fix: &Value) {
    let it = item(d, id);
    if let Some(r) = fix["strip"].as_str() {
        let mut cur = strs(&it["strip"]);
        if !cur.iter().any(|x| x == r) {
            cur.push(r.to_string());
        }
        it["strip"] = json!(cur);
    }
    if fix["access"].is_object() {
        it["access"] = fix["access"].clone();
    }
}

/// A key for a new draft item: `new-<n>`, one higher than any in the draft.
fn new_id(d: &Value) -> String {
    let n = d["items"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, _)| k.strip_prefix("new-")?.parse::<u64>().ok())
        .max();
    format!("new-{}", n.map_or(1, |n| n + 1))
}

fn summary(t: &[Value], n: u64) -> String {
    let parts: Vec<String> = [
        ("new", "new"),
        ("deleted", "deleted"),
        ("renamed", "renamed"),
        ("moved", "moved"),
    ]
    .iter()
    .filter_map(|(k, l)| {
        let c = t
            .iter()
            .filter(|x| x[*k] == true && (*k == "deleted" || x["deleted"] != true))
            .count();
        (c > 0).then(|| format!("{c} {l}"))
    })
    .collect();
    let more = if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    };
    format!("{n} change{}{more}", if n == 1 { "" } else { "s" })
}

/// A count that the server may send as a number or as a list.
fn count(v: &Value) -> usize {
    v.as_u64()
        .map_or_else(|| v.as_array().map_or(0, Vec::len), |n| n as usize)
}

#[component]
pub fn ChannelSetup(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let data = RwSignal::new(Value::Null);
    let dc = RwSignal::new(Value::Null);
    let sel = RwSignal::new(None::<String>);
    // (id, true when the name is edited in the right panel rather than in the tree)
    let editing = RwSignal::new(None::<(String, bool)>);
    let (drag, over) = (RwSignal::new(None::<String>), RwSignal::new(None::<String>));
    let adding = RwSignal::new(false);
    // Some(true): push stopped by issues; Some(false): confirm the push.
    let dialog = RwSignal::new(None::<bool>);
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let seq = StoredValue::new(0u32);
    let g = StoredValue::new(guild);
    let path = move || format!("/api/guilds/{}/channel-setup", g.get_value());
    let load = move || {
        if data.is_disposed() {
            return;
        }
        let (p, gid) = (path(), g.get_value());
        leptos::task::spawn_local(async move {
            let r = api("GET", &p, None).await;
            let d = api("GET", &format!("/api/guilds/{gid}/discord"), None).await;
            if data.is_disposed() {
                return;
            }
            match r {
                Ok(v) => data.set(v),
                Err(e) => msg.err(e),
            }
            if let Ok(d) = d {
                dc.set(d);
            }
        });
    };
    load();
    // Only the newest reply is shown, so a slow older one cannot undo a newer edit.
    let send = move |method: &'static str, body: Option<Value>| {
        let n = seq.get_value() + 1;
        seq.set_value(n);
        let p = path();
        run_act(
            async move {
                let r = api(method, &p, body).await;
                // The refused edit is already in the local draft: show the server's draft again.
                if r.is_err() {
                    if let Ok(v) = api("GET", &p, None).await {
                        data.set(v);
                    }
                }
                r
            },
            msg,
            need,
            move |v| {
                if seq.get_value() == n {
                    data.set(v)
                }
            },
        );
    };
    let edit = move |f: &dyn Fn(&mut Value, &[Value])| {
        let (mut d, t) = data.with_untracked(|v| (v["draft"].clone(), arr(&v["tree"])));
        f(&mut d, &t);
        // The next edit builds on this one before the reply is back.
        data.update_untracked(|v| v["draft"] = d.clone());
        send("PUT", Some(json!({"draft": d})));
    };
    let tree = Memo::new(move |_| arr(&data.get()["tree"]));
    let changes = move || data.with(|v| v["changes"].as_u64().unwrap_or(0));

    let rename = move |id: String, name: String| {
        if editing.get_untracked().is_none_or(|(e, _)| e != id) {
            return;
        }
        editing.set(None);
        let name = name.trim().to_string();
        if name.is_empty()
            || find(&tree.get_untracked(), &id).is_some_and(|n| n["name"] == name.as_str())
        {
            return;
        }
        edit(&move |d: &mut Value, _: &[Value]| item(d, &id)["name"] = json!(name));
    };
    let set_delete = move |id: String, on: bool| {
        edit(&move |d: &mut Value, _: &[Value]| item(d, &id)["delete"] = json!(on))
    };
    let set_access = move |id: String, a: Value| {
        edit(&move |d: &mut Value, _: &[Value]| {
            let it = item(d, &id);
            it["access"] = a.clone();
            it["reset"] = json!(false);
        })
    };
    let reset = move |id: String| {
        edit(&move |d: &mut Value, _: &[Value]| {
            let it = item(d, &id);
            it["reset"] = json!(true);
            if let Some(o) = it.as_object_mut() {
                o.remove("access");
            }
        })
    };
    let move_to = move |id: String, parent_id: Option<String>, at: usize| {
        edit(&move |d: &mut Value, t: &[Value]| place(d, t, &id, parent_id.clone(), at))
    };
    let create = move |kind: u8| {
        adding.set(false);
        let t = tree.get_untracked();
        let id = data.with_untracked(|v| new_id(&v["draft"]));
        let parent_id = (kind != 4)
            .then(|| {
                sel.get_untracked().and_then(|x| {
                    find(&t, &x).and_then(|n| {
                        if is_cat(n) {
                            Some(x.clone())
                        } else {
                            parent(&t, n)
                        }
                    })
                })
            })
            .flatten();
        let name = match kind {
            4 => "New Category",
            2 => "New Voice",
            5 => "new-announcements",
            _ => "new-channel",
        };
        let at = siblings(&t, parent_id.as_deref(), kind == 4).len();
        {
            let id = id.clone();
            edit(&move |d: &mut Value, _: &[Value]| {
                let it = item(d, &id);
                it["kind"] = json!(kind);
                it["name"] = json!(name);
                it["parent"] = json!(parent_id);
                it["position"] = json!(at);
            });
        }
        sel.set(Some(id.clone()));
        editing.set(Some((id, true)));
    };
    let role_of = move |id: &str| {
        dc.with(|d| {
            d["roles"]
                .as_array()
                .and_then(|a| a.iter().find(|r| r["id"] == id).cloned())
        })
        .unwrap_or(Value::Null)
    };
    let role_style = |r: &Value| {
        r["color"]
            .as_u64()
            .filter(|c| *c > 0)
            .map(|c| format!("--rc:#{c:06x}"))
    };
    let chips = move |acc: &Value| {
        if acc["limited"] != true {
            return ().into_any();
        }
        let roles = strs(&acc["roles"]);
        if roles.is_empty() {
            return view! { <span class="mini">"Staff only"</span> }.into_any();
        }
        roles.iter().map(|id| { let r = role_of(id); view! { <span class="mini r" style=role_style(&r)>{r["name"].as_str().unwrap_or(id).to_string()}</span> } }).collect_view().into_any()
    };
    let name_input = move |id: String, cur: String| {
        let r = NodeRef::<html::Input>::new();
        // The value is set here too: the effect can run before `prop:value` is applied, and then there is nothing to select.
        let v = cur.clone();
        Effect::new(move || {
            if let Some(i) = r.get() {
                i.set_value(&v);
                let _ = i.focus();
                i.select();
            }
        });
        let id2 = id.clone();
        view! { <input class="nm" node_ref=r aria-label="name" maxlength="100" prop:value=cur on:click=|e| e.stop_propagation()
        on:keydown=move |e| { if e.key() == "Enter" { rename(id.clone(), event_target_value(&e)) } else if e.key() == "Escape" { editing.set(None) } }
        on:blur=move |e| rename(id2.clone(), event_target_value(&e)) /> }
    };
    let acts = move |id: String, deleted: bool, panel: bool| {
        if deleted {
            return view! { <button class="link" title="Undo delete" aria-label="undo delete" on:click=move |_| set_delete(id.clone(), false)>{icon("restore")}</button> }.into_any();
        }
        let id2 = id.clone();
        view! {
            <button class="link" title="Rename" aria-label="rename" on:click=move |_| editing.set(Some((id.clone(), panel)))>{icon("edit")}</button>
            <button class="link" title="Delete" aria-label="delete" on:click=move |_| set_delete(id2.clone(), true)>{icon("trash")}</button>
        }
        .into_any()
    };

    // `sel` and `over` are read per row: rebuilding the tree on every dragover would replace the dragged row.
    let row = move |n: Value| {
        let t = tree.get_untracked();
        let id = s(&n["id"]);
        let (cat, deleted) = (is_cat(&n), n["deleted"] == true);
        let ed = editing.get().is_some_and(|(e, p)| e == id && !p);
        let issues = arr(&n["issues"]);
        let risk = issues
            .iter()
            .map(|i| s(&i["title"]))
            .collect::<Vec<_>>()
            .join(" ");
        let (i1, i2, i3, i4, i5, i6) = (
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
        );
        let name = if ed {
            name_input(id.clone(), s(&n["name"])).into_any()
        } else {
            view! { <button class="link nm">{s(&n["name"])}</button> }.into_any()
        };
        view! {
            <div class="tr" class:cat=cat class:child={!cat && parent(&t, &n).is_some()} class:sel=move || sel.get().as_deref() == Some(i6.as_str()) class:del=deleted class:risk={!issues.is_empty()} class:over=move || over.get().as_deref() == Some(id.as_str())
                draggable={ if ed { "false" } else { "true" } }
                on:click=move |_| sel.set(Some(i1.clone()))
                on:dragstart=move |e| { if let Some(dt) = e.data_transfer() { let _ = dt.set_data("text/plain", &i2); } drag.set(Some(i2.clone())) }
                on:dragover=move |e| { if drag.get_untracked().is_some() { e.prevent_default(); over.set(Some(i3.clone())) } }
                on:dragend=move |_| { drag.set(None); over.set(None) }
                on:drop=move |e| {
                    e.prevent_default();
                    over.set(None);
                    let Some(d) = drag.get_untracked() else { return };
                    drag.set(None);
                    if let Some((p, at)) = drop_target(&tree.get_untracked(), &d, &i4) { move_to(d, p, at) }
                }>
                <span class="h" title="Drag to move">{icon("menu")}</span>
                {icon(kind_icon(n["kind"].as_u64().unwrap_or(0)))}
                {name}
                {(n["new"] == true).then(|| view! { <span class="chip blue">"New"</span> })}
                {(n["exception"] == true).then(|| view! { <span class="exc"><Tip text="Has its own settings. Category changes do not change it.".to_string() /></span> })}
                {(!issues.is_empty()).then(|| view! { <span class="risk-i"><Tip icon="warning" text=risk /></span> })}
                <span class="sp"></span>
                {chips(&n["access"])}
                <span class="acts">{acts(i5, deleted, false)}</span>
            </div>
        }
    };

    let panel = move || {
        let t = tree.get();
        let Some(n) = sel.get().and_then(|id| find(&t, &id).cloned()) else {
            return view! { <div class="card"><p class="muted">"Pick a channel or category to see who can see it."</p></div> }.into_any();
        };
        let id = s(&n["id"]);
        let (cat, deleted, kind) = (
            is_cat(&n),
            n["deleted"] == true,
            n["kind"].as_u64().unwrap_or(0),
        );
        let ed = editing.get().is_some_and(|(e, p)| e == id && p);
        let title = if ed {
            name_input(id.clone(), s(&n["name"])).into_any()
        } else {
            view! { <h2>{s(&n["name"])}</h2> }.into_any()
        };
        let head = view! { <div class="ttl">{title}<span class="muted">{kind_name(kind)}</span><span class="sp"></span><span class="acts">{acts(id.clone(), deleted, true)}</span></div> };
        if deleted {
            return view! { <div class="card">{head}<p class="muted">"Deleted when you push. Undo keeps it."</p></div> }.into_any();
        }
        let acc = n["access"].clone();
        let limited = acc["limited"] == true;
        let picked = strs(&acc["roles"]);
        let staff = acc["staff"].as_bool().unwrap_or(true);
        let (can, staff_roles) = data.with(|v| {
            (
                v["can_unstaff"].as_bool().unwrap_or(false),
                strs(&v["staff_roles"]),
            )
        });
        let gid = g.get_value();
        let p = parent(&t, &n);
        let list = siblings(&t, p.as_deref(), cat);
        let at = list.iter().position(|x| *x == id).unwrap_or(0);
        let last = list.len().saturating_sub(1);
        let sid = StoredValue::new(id.clone());
        let pv = StoredValue::new(p.clone());
        let cur = StoredValue::new((picked.clone(), staff));
        let role_chips = picked.iter().map(|r| {
            let (role, r2) = (role_of(r), r.clone());
            let name = role["name"].as_str().unwrap_or(r).to_string();
            view! { <span class="mini r big" style=role_style(&role)>{name.clone()}" "<button class="link" aria-label=format!("remove {name}") on:click=move |_| { let (mut roles, st) = cur.get_value(); roles.retain(|x| *x != r2); set_access(sid.get_value(), access(&roles, st, true)) }>"×"</button></span> }
        }).collect_view();
        let options = dc
            .with(|d| arr(&d["roles"]))
            .into_iter()
            .filter(|r| {
                r["id"] != gid.as_str()
                    && r["managed"] != true
                    && !picked.contains(&s(&r["id"]))
                    && !staff_roles.contains(&s(&r["id"]))
            })
            .map(|r| view! { <option value=s(&r["id"])>{s(&r["name"])}</option> })
            .collect_view();
        let follow = if cat {
            t.iter().filter(|c| c["exception"] == true && parent(&t, c).as_deref() == Some(id.as_str())).map(|c| {
                let cid = s(&c["id"]);
                view! { <p class="line"><span class="exc">{icon("info")}</span>{format!("{} has its own settings. ", label(c))}<button class="link info" on:click=move |_| reset(cid.clone())>"Reset to category"</button></p> }
            }).collect_view().into_any()
        } else if p.is_none() {
            ().into_any()
        } else if n["exception"] == true {
            view! { <p class="line"><span class="exc">{icon("info")}</span>"Has its own settings. Category changes do not change it. "<button class="link info" on:click=move |_| reset(sid.get_value())>"Reset to category"</button></p> }.into_any()
        } else {
            view! { <p class="line muted">"Follows the category."</p> }.into_any()
        };
        let cats = (!cat).then(|| {
            let opts = t.iter().filter(|c| is_cat(c) && c["deleted"] != true).map(|c| { let cid = s(&c["id"]); let on = p.as_deref() == Some(cid.as_str()); view! { <option value=cid prop:selected=on>{s(&c["name"])}</option> } }).collect_view();
            view! { <select aria-label="category" on:change=move |e| { let v = event_target_value(&e); let np = (!v.is_empty()).then_some(v); let at = siblings(&tree.get_untracked(), np.as_deref(), false).len(); move_to(sid.get_value(), np, at) }>
                <option value="" prop:selected=p.is_none()>"No category"</option>{opts}</select> }
        });
        let staff_tip = if can {
            "Staff are the roles of the Admins and Moderators permits. Turn this off to hide it from them too."
        } else {
            "Staff are the roles of the Admins and Moderators permits. Only the owner or an extra owner can hide a channel from staff."
        };
        view! { <div class="card">
            {head}
            <div class="lbl">"Who can see it"</div>
            <div class="seg" role="group" aria-label="who can see it">
                <button class="ghost" class:on=!limited aria-pressed=(!limited).to_string() on:click=move |_| if limited { set_access(sid.get_value(), access(&[], true, false)) }>"Everyone"</button>
                <button class="ghost" class:on=limited aria-pressed=limited.to_string() on:click=move |_| if !limited { set_access(sid.get_value(), access(&[], true, true)) }>"Only these roles"</button>
            </div>
            {limited.then(|| view! {
                <div class="chips">{role_chips}
                    <select class="add" aria-label="add a role" prop:value="" on:change=move |e| { let r = event_target_value(&e); if r.is_empty() { return } let (mut roles, st) = cur.get_value(); roles.push(r); set_access(sid.get_value(), access(&roles, st, true)) }>
                        <option value="">"+ Role"</option>{options}
                    </select>
                </div>
                <div class="row2">
                    <input type="checkbox" class="sw" aria-label="staff can see it" prop:checked=staff disabled={ !can } title=(!can).then_some("Only the owner or an extra owner can change this")
                        on:change=move |e| { let (roles, _) = cur.get_value(); set_access(sid.get_value(), access(&roles, event_target_checked(&e), true)) } />
                    <span>"Staff can see it"</span><Tip text=staff_tip.to_string() />
                </div>
            })}
            {follow}
            <div class="lbl">"Position"</div>
            <div class="row2">
                <button class="ghost" disabled={at == 0} on:click=move |_| move_to(sid.get_value(), pv.get_value(), at.saturating_sub(1))>{icon("arrow_up")}"Move up"</button>
                <button class="ghost" disabled={at >= last} on:click=move |_| move_to(sid.get_value(), pv.get_value(), at + 1)>{icon("arrow_down")}"Move down"</button>
                {cats}
            </div>
        </div> }.into_any()
    };

    let fix = move || {
        let issues = data.with_untracked(|v| arr(&v["issues"]));
        dialog.set(None);
        edit(&move |d: &mut Value, _: &[Value]| {
            for i in &issues {
                apply_fix(d, &s(&i["channel"]), &i["fix"]);
            }
        });
    };
    let push = move || {
        dialog.set(None);
        let p = path();
        run_act(
            async move { api("POST", &format!("{p}/push"), None).await },
            msg,
            need,
            move |v| {
                let failed = strs(&v["failed"]);
                let done = count(&v["done"]);
                let backup = v["backup"]
                    .as_i64()
                    .map_or(String::new(), |b| format!(" Backup #{b} was taken first."));
                if failed.is_empty() {
                    msg.ok(format!("Pushed {done} change(s).{backup}"));
                } else {
                    msg.err(format!(
                        "Pushed {done} change(s), {} failed.{backup}\n{}",
                        failed.len(),
                        failed.join("\n")
                    ));
                }
                load();
            },
        );
    };
    let dlg =
        Memo::new(move |prev: Option<&Option<bool>>| dialog.get().or(prev.copied().flatten()));
    let dialog_view = move |stopped: bool| {
        let r = NodeRef::new();
        focus_on_open(r);
        let (t, issues, n) = data.with_untracked(|v| {
            (
                arr(&v["tree"]),
                arr(&v["issues"]),
                v["changes"].as_u64().unwrap_or(0),
            )
        });
        let sum = summary(&t, n);
        let body = if stopped {
            let fixable = issues.iter().any(|i| i["fix"].is_object());
            let list = issues.iter().map(|i| {
                let ch = s(&i["channel"]);
                let name = find(&t, &ch).map_or(ch, label);
                view! { <div class="issue"><b>{name}</b>" "{s(&i["title"])}<small>{s(&i["detail"])}</small></div> }
            }).collect_view();
            view! {
                <h2><span class="risk-i">{icon("warning")}</span>{format!("Push stopped: {} unsafe change{}", issues.len(), if issues.len() == 1 { "" } else { "s" })}</h2>
                <p class="muted">"Nothing was sent to Discord. Fix this first, then push again."</p>
                {list}
                <p class="muted">{format!("Also in this push: {sum}. A backup is taken first.")}</p>
                <div class="row end"><button class="ghost" on:click=move |_| dialog.set(None)>"Close"</button><button class="go" disabled={ !fixable } on:click=move |_| fix()>"Fix"</button></div>
            }.into_any()
        } else {
            view! {
                <h2>"Push to Discord?"</h2>
                <p>{format!("This push makes {sum}.")}</p>
                <p class="muted">"A backup is taken first, so Backups can undo it."</p>
                <div class="row end"><button class="ghost" on:click=move |_| dialog.set(None)>"Cancel"</button><button class="go" on:click=move |_| push()>"Push"</button></div>
            }.into_any()
        };
        view! { <div class="modal" node_ref=r role="dialog" aria-modal="true" aria-label={ if stopped { "Push stopped" } else { "Push to Discord" } } on:keydown=move |e| dialog_keys(&e, move || dialog.set(None))><div class="card">{body}</div></div> }
    };

    view! {
        <div class="cs">
            <div class="cs-grid">
                <div class="card">
                    <div class="ttl"><h2>{icon("channel_text")}"Channels"</h2><span class="sp"></span>
                        <button class="ghost" on:click=move |_| create(4)>"+ Category"</button>
                        <button class="ghost" aria-expanded=move || adding.get().to_string() on:click=move |_| adding.update(|a| *a = !*a)>"+ Channel"</button>
                    </div>
                    <Show when=move || adding.get()>
                        <div class="chips end"><span class="muted">"Kind:"</span>
                            <button class="ghost" on:click=move |_| create(0)>{icon("channel_text")}"Text"</button>
                            <button class="ghost" on:click=move |_| create(2)>{icon("channel_voice")}"Voice"</button>
                            <button class="ghost" on:click=move |_| create(5)>{icon("announcement")}"Announcement"</button>
                        </div>
                    </Show>
                    {move || if data.with(Value::is_null) { view! { <div class="skel"></div> }.into_any() } else { tree.get().into_iter().map(row).collect_view().into_any() }}
                </div>
                {panel}
            </div>
            <Fade when=Signal::derive(move || dialog.get().is_some())>{move || dlg.get().map(dialog_view)}</Fade>
            <Fade when=Signal::derive(move || { changes() > 0 })>
                <div class="bar cs-bar"><span class="dot"></span><span><b>{move || format!("{} change{}", changes(), if changes() == 1 { "" } else { "s" })}</b>" not pushed yet"</span>
                    <button class="ghost" on:click=move |_| { if confirm(&format!("Discard all {} change(s)? Discord is not touched.", changes())) { send("DELETE", None) } }>"Discard"</button>
                    <button class="go" on:click=move |_| dialog.set(Some(data.with_untracked(|v| v["issues"].as_array().is_some_and(|a| !a.is_empty()))))>"Push"</button>
                </div>
            </Fade>
            <Toast msg />
            <StepUp need enrolled />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Vec<Value> {
        json!([
            {"id": "r", "name": "rules", "kind": 0, "parent": null, "position": 0},
            {"id": "o", "name": "orphan", "kind": 0, "parent": "gone", "position": 1},
            {"id": "c1", "name": "TEXT", "kind": 4, "parent": null, "position": 0},
            {"id": "a", "name": "general", "kind": 0, "parent": "c1", "position": 0},
            {"id": "b", "name": "memes", "kind": 0, "parent": "c1", "position": 1},
            {"id": "c2", "name": "VOICE", "kind": 4, "parent": null, "position": 1},
            {"id": "v", "name": "General", "kind": 2, "parent": "c2", "position": 0}
        ])
        .as_array()
        .cloned()
        .unwrap()
    }

    #[test]
    fn siblings_follow_the_tree_and_treat_a_missing_category_as_none() {
        assert_eq!(siblings(&tree(), None, false), ["r", "o"]);
        assert_eq!(siblings(&tree(), None, true), ["c1", "c2"]);
        assert_eq!(siblings(&tree(), Some("c1"), false), ["a", "b"]);
    }

    #[test]
    fn place_and_drop_renumber_only_what_moved() {
        let t = tree();
        let mut d = json!({"items": {}});
        place(&mut d, &t, "b", Some("c1".into()), 0);
        assert_eq!(d["items"]["b"]["position"], 0);
        assert_eq!(d["items"]["a"]["position"], 1);
        assert!(d["items"]["b"].get("parent").is_none());
        // A channel dropped on a category goes first in it and changes parent.
        assert_eq!(drop_target(&t, "r", "c2"), Some((Some("c2".into()), 0)));
        let mut d = json!({});
        place(&mut d, &t, "r", Some("c2".into()), 0);
        assert_eq!(d["items"]["r"]["parent"], "c2");
        assert_eq!(d["items"]["v"]["position"], 1);
        // A category dropped on a channel goes before that channel's category.
        assert_eq!(drop_target(&t, "c2", "a"), Some((None, 0)));
        assert_eq!(drop_target(&t, "c2", "r"), None);
        assert_eq!(drop_target(&t, "b", "a"), Some((Some("c1".into()), 0)));
    }

    #[test]
    fn fix_merges_into_the_draft_item() {
        let mut d = json!({"items": {"new-2": {"strip": ["1"]}}});
        apply_fix(&mut d, "new-2", &json!({"strip": "1"}));
        apply_fix(&mut d, "new-2", &json!({"strip": "2"}));
        apply_fix(&mut d, "x", &json!({"access": access(&[], true, true)}));
        assert_eq!(d["items"]["new-2"]["strip"], json!(["1", "2"]));
        assert_eq!(
            d["items"]["x"],
            json!({"access": {"roles": [], "staff": true, "limited": true}})
        );
        assert_eq!(new_id(&d), "new-3");
        assert_eq!(new_id(&json!({})), "new-1");
    }
}

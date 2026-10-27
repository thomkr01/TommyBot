//! Embed builder: compose with a live preview, save as a draft, let the bot post it and edit it in place.
use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn get_at(e: &Value, path: &str) -> String {
    path.split('.').fold(e, |v, k| &v[k]).as_str().unwrap_or("").to_string()
}

/// Writes `v` at `key` or `parent.key`; empty removes the key (and an emptied parent), since Discord rejects "".
fn set_at(e: &mut Value, path: &str, v: String) {
    let (parent, key) = path.split_once('.').map_or((None, path), |(p, k)| (Some(p), k));
    let m = match parent { Some(p) => &mut e[p], None => &mut *e };
    if v.is_empty() { if let Some(m) = m.as_object_mut() { m.remove(key); } } else { m[key] = json!(v); }
    if let Some(p) = parent { if e[p].as_object().is_some_and(|m| m.is_empty()) { e.as_object_mut().map(|o| o.remove(p)); } }
}

fn edit(cur: RwSignal<Value>, f: impl FnOnce(&mut Value)) { cur.update(|c| f(&mut c["embed"])) }

fn blank() -> Value { json!({"id": null, "name": "", "channel": null, "embed": {}}) }

#[component]
pub fn Embeds(guild: String) -> impl IntoView {
    let g = StoredValue::new(guild);
    let rows = RwSignal::new(Vec::<Value>::new());
    let channels = RwSignal::new(Vec::<(String, String)>::new());
    let (cur, msg) = (RwSignal::new(blank()), RwSignal::new(String::new()));
    let load = move || spawn_local(async move {
        match api("GET", &format!("/api/guilds/{}/embeds", g.get_value()), None).await {
            Ok(v) => rows.set(v.as_array().cloned().unwrap_or_default()),
            Err(e) => msg.set(e),
        }
    });
    load();
    spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{}/discord", g.get_value()), None).await {
            channels.set(v["channels"].as_array().into_iter().flatten().filter(|c| c["kind"] != "category")
                .map(|c| (c["id"].as_str().unwrap_or("").to_string(), format!("#{}", c["name"].as_str().unwrap_or("?")))).collect());
        }
    });
    let text = move |label: &'static str, path: &'static str, max: u32| view! {
        <div class="row"><label>{label}</label><input maxlength=max prop:value=move || cur.with(|c| get_at(&c["embed"], path))
            on:input=move |e| { let v = event_target_value(&e); edit(cur, move |em| set_at(em, path, v)) } /></div>
    };
    // Saves the draft, then optionally has the bot post it (`Some(repost)`).
    let save = move |post: Option<bool>| spawn_local(async move {
        let c = cur.get_untracked();
        let base = format!("/api/guilds/{}/embeds", g.get_value());
        let r = async {
            let id = api("POST", &base, Some(json!({"id": c["id"], "name": c["name"], "embed": c["embed"], "channel": c["channel"]}))).await?["id"].clone();
            cur.update(|c| c["id"] = id.clone());
            match post {
                Some(repost) => {
                    let v = api("POST", &format!("{base}/{id}/post"), Some(json!({"repost": repost}))).await?;
                    cur.update(|c| { c["message"] = v["message"].clone(); c["message_channel"] = v["message_channel"].clone(); });
                    Ok::<_, String>(format!("Embed {}.", v["done"].as_str().unwrap_or("posted")))
                }
                None => Ok("Draft saved.".into()),
            }
        }.await;
        msg.set(r.unwrap_or_else(|e| e));
        load();
    });
    let remove = move |_| {
        let id = cur.with_untracked(|c| c["id"].as_i64());
        let Some(id) = id else { return cur.set(blank()) };
        if !web_sys::window().is_some_and(|w| w.confirm_with_message("Delete this embed and the bot's message?").unwrap_or(false)) { return }
        spawn_local(async move {
            match api("POST", &format!("/api/guilds/{}/embeds/{id}/delete", g.get_value()), Some(json!({}))).await {
                Ok(_) => { cur.set(blank()); msg.set("Deleted.".into()); load() }
                Err(e) => msg.set(e),
            }
        });
    };
    let posted = move || cur.with(|c| c["message"].as_str().map(|m| format!("https://discord.com/channels/{}/{}/{m}", g.get_value(), c["message_channel"].as_str().unwrap_or(""))));
    let color = move || format!("#{:06x}", cur.with(|c| c["embed"]["color"].as_u64().unwrap_or(0x0d9df8)));
    view! {
        <div class="card row">
            <select aria-label="saved embeds" on:change=move |e| { let v = event_target_value(&e); msg.set(String::new());
                cur.set(rows.with_untracked(|r| r.iter().find(|x| x["id"].as_i64() == v.parse().ok()).cloned()).unwrap_or_else(blank)) }>
                <option value="" selected=move || cur.with(|c| c["id"].is_null())>"New embed"</option>
                {move || rows.get().into_iter().map(|r| { let id = r["id"].as_i64(); view! {
                    <option value=r["id"].to_string() selected=move || cur.with(|c| c["id"].as_i64() == id)>{format!("{}{}", r["name"].as_str().unwrap_or(""), if r["message"].is_null() { " (draft)" } else { "" })}</option> } }).collect_view()}
            </select>
            <input placeholder="Name" maxlength="100" prop:value=move || cur.with(|c| get_at(c, "name")) on:input=move |e| { let v = event_target_value(&e); cur.update(|c| c["name"] = json!(v)) } />
            <select aria-label="channel" on:change=move |e| { let v = event_target_value(&e); cur.update(|c| c["channel"] = if v.is_empty() { Value::Null } else { json!(v) }) }>
                <option value="">"choose a channel"</option>
                {move || channels.get().into_iter().map(|(id, l)| { let i = id.clone(); view! { <option value=id selected=move || cur.with(|c| c["channel"].as_str() == Some(i.as_str()))>{l}</option> } }).collect_view()}
            </select>
        </div>
        <div class="cards">
            <div class="card">
                {text("Author", "author.name", 256)}
                {text("Title", "title", 256)}
                {text("Title link", "url", 2048)}
                <div class="row"><label>"Description"</label><textarea class="short" maxlength="4096" prop:value=move || cur.with(|c| get_at(&c["embed"], "description"))
                    on:input=move |e| { let v = event_target_value(&e); edit(cur, move |em| set_at(em, "description", v)) }></textarea></div>
                <div class="row"><label>"Colour"</label><input type="color" prop:value=color
                    on:input=move |e| { let v = u32::from_str_radix(event_target_value(&e).trim_start_matches('#'), 16).unwrap_or(0); edit(cur, move |em| em["color"] = json!(v)) } /></div>
                {text("Thumbnail URL", "thumbnail.url", 2048)}
                {text("Image URL", "image.url", 2048)}
                {text("Footer", "footer.text", 2048)}
                <div class="row tog"><input type="checkbox" id="embed-ts" prop:checked=move || cur.with(|c| c["embed"]["timestamp"].is_string())
                    on:change=move |e| { let on = event_target_checked(&e); edit(cur, move |em| set_at(em, "timestamp", if on { js_sys::Date::new_0().to_iso_string().into() } else { String::new() })) } />
                    <label for="embed-ts">"Timestamp (now)"</label></div>
                <div class="row"><label>"Fields"</label></div>
                {move || (0..cur.with(|c| c["embed"]["fields"].as_array().map_or(0, Vec::len))).map(|i| view! {
                    <div class="row">
                        <input placeholder="Name" maxlength="256" prop:value=move || cur.with(|c| get_at(&c["embed"]["fields"][i], "name")) on:input=move |e| { let v = event_target_value(&e); edit(cur, move |em| em["fields"][i]["name"] = json!(v)) } />
                        <input placeholder="Value" maxlength="1024" prop:value=move || cur.with(|c| get_at(&c["embed"]["fields"][i], "value")) on:input=move |e| { let v = event_target_value(&e); edit(cur, move |em| em["fields"][i]["value"] = json!(v)) } />
                        <span><input type="checkbox" aria-label="inline" prop:checked=move || cur.with(|c| c["embed"]["fields"][i]["inline"] == true) on:change=move |e| { let v = event_target_checked(&e); edit(cur, move |em| em["fields"][i]["inline"] = json!(v)) } />" inline"</span>
                        <button class="ghost" on:click=move |_| edit(cur, move |em| { if let Some(a) = em["fields"].as_array_mut() { a.remove(i); if a.is_empty() { em.as_object_mut().map(|o| o.remove("fields")); } } })>"✕"</button>
                    </div> }).collect_view()}
                <button class="ghost" disabled=move || cur.with(|c| c["embed"]["fields"].as_array().is_some_and(|a| a.len() >= 25))
                    on:click=move |_| edit(cur, |em| { let f = json!({"name": "", "value": "", "inline": false}); match em["fields"].as_array_mut() { Some(a) => a.push(f), None => em["fields"] = json!([f]) } })>"Add field"</button>
            </div>
            <div class="card">
                // shortcut: plain text preview, Discord renders the markdown itself; add a renderer if people ask.
                {move || cur.with(|c| { let e = c["embed"].clone(); view! {
                    <div class="embed" style=format!("border-left-color:{}", color())>
                        {e["thumbnail"]["url"].as_str().map(|u| view! { <img class="thumb" src=u.to_string() alt="" /> })}
                        {e["author"]["name"].as_str().map(|a| view! { <small><b>{a.to_string()}</b></small> })}
                        {e["title"].as_str().map(|t| view! { <b>{t.to_string()}</b> })}
                        {e["description"].as_str().map(|d| view! { <p>{d.to_string()}</p> })}
                        <div class="fields">{e["fields"].as_array().into_iter().flatten().map(|f| view! {
                            <div class:wide=f["inline"] != true><b>{get_at(f, "name")}</b><p>{get_at(f, "value")}</p></div> }).collect_view()}</div>
                        {e["image"]["url"].as_str().map(|u| view! { <img src=u.to_string() alt="" /> })}
                        <small class="muted">{[get_at(&e, "footer.text"), get_at(&e, "timestamp").get(..16).unwrap_or("").replace('T', " ")].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" • ")}</small>
                    </div> } })}
                <div class="row">
                    <button on:click=move |_| save(None)>"Save draft"</button>
                    <button on:click=move |_| save(Some(false))>{move || if cur.with(|c| c["message"].is_string()) { "Update message" } else { "Post" }}</button>
                    <Show when=move || cur.with(|c| c["message"].is_string())><button class="ghost" on:click=move |_| save(Some(true))>"Repost"</button></Show>
                    <button class="ghost" on:click=remove>"Delete"</button>
                </div>
                {move || posted().map(|u| view! { <a href=u target="_blank" rel="noopener">"Jump to message"</a> })}
                <p class="muted">{move || msg.get()}</p>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_at_prunes_empty() {
        let mut e = json!({});
        set_at(&mut e, "author.name", "a".into());
        assert_eq!(e, json!({"author": {"name": "a"}}));
        set_at(&mut e, "author.name", String::new());
        set_at(&mut e, "title", String::new());
        assert_eq!(e, json!({}));
        assert_eq!(get_at(&json!({"footer": {"text": "x"}}), "footer.text"), "x");
    }
}

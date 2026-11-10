//! Bot-owner page for the tokens that let the MCP server propose changes (DEPLOY.md 6.4).
use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn when(v: &Value) -> String {
    v.as_str()
        .map(|t| t.chars().take(16).collect::<String>().replace('T', " "))
        .unwrap_or_else(|| "never".into())
}

#[component]
pub fn McpTokens(enrolled: ReadSignal<bool>) -> impl IntoView {
    let list = RwSignal::new(Vec::<Value>::new());
    let name = RwSignal::new(String::new());
    // The new token, shown once: (name, token).
    let made = RwSignal::new(None::<(String, String)>);
    let shown = RwSignal::new(false);
    let copied = RwSignal::new(None::<bool>);
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let load = move || {
        if list.is_disposed() {
            return;
        }
        spawn_local(async move {
            let r = api("GET", "/api/mcp-tokens", None).await;
            if list.is_disposed() {
                return;
            }
            match r {
                Ok(v) => list.set(v.as_array().cloned().unwrap_or_default()),
                Err(e) => msg.err(e),
            }
        })
    };
    load();
    let create = move |_| {
        let n = name.get_untracked();
        run_act(
            async move { api("POST", "/api/mcp-tokens", Some(json!({"name": n}))).await },
            msg,
            need,
            move |v| {
                made.set(Some((s(&v["name"]), s(&v["token"]))));
                shown.set(false);
                copied.set(None);
                name.set(String::new());
                load();
            },
        )
    };
    let revoke = move |id: i64, n: String| {
        if !confirm(&format!(
            "Revoke the token \"{n}\"? The MCP server that uses it can no longer propose changes, and changes waiting for a code are closed."
        )) {
            return;
        }
        run_act(
            async move { api("POST", &format!("/api/mcp-tokens/{id}/revoke"), None).await },
            msg,
            need,
            move |_| {
                msg.ok("Token revoked.");
                load();
            },
        )
    };
    let rows = move || {
        list.get()
            .into_iter()
            .map(|t| {
                let id = t["id"].as_i64().unwrap_or_default();
                let n = s(&t["name"]);
                let live = t["revoked_at"].is_null();
                let n2 = n.clone();
                view! {
                    <tr class:muted=!live>
                        <td>{n}</td>
                        <td>{when(&t["created_at"])}</td>
                        <td>{when(&t["last_used_at"])}</td>
                        <td>{if live {
                            view! { <button class="danger" on:click=move |_| revoke(id, n2.clone())>"Revoke"</button> }.into_any()
                        } else {
                            view! { <span>{format!("Revoked {}", when(&t["revoked_at"]))}</span> }.into_any()
                        }}</td>
                    </tr>
                }
            })
            .collect_view()
    };
    view! {
        <div class="head"><small>"TommyBot"</small><h1>"MCP tokens"</h1></div>
        <StepUp need enrolled />
        <div class="card">
            <p class="muted">"A token lets the MCP server propose changes for Claude. Nothing changes until you type Claude the code the bot sends you by DM. Make one token per place that uses it, and revoke it when you stop using it. Making or revoking a token needs 2FA."</p>
            <div class="row">
                <input placeholder="Name, for example VPS" aria-label="token name" maxlength="40" prop:value=name on:input=move |e| name.set(event_target_value(&e)) />
                <button class="go" disabled=move || name.get().trim().is_empty() on:click=create>"Make token"</button>
            </div>
            <Show when=move || made.get().is_some()>
                <p><b>{move || format!("Token \"{}\" (shown once, it is not stored):", made.get().map(|m| m.0).unwrap_or_default())}</b></p>
                <div class="row"><pre class:blur=move || !shown.get()>{move || made.get().map(|m| m.1).unwrap_or_default()}</pre>
                    <button class="danger" on:click=move |_| shown.update(|v| *v = !*v)>{move || if shown.get() { "Hide" } else { "Show" }}</button>
                    <button class="ghost" on:click=move |_| { let t = made.get_untracked().map(|m| m.1).unwrap_or_default(); spawn_local(async move { copied.set(Some(copy_text(&t).await)) }) }>{move || copy_label(copied.get())}</button></div>
                <p class="muted">"Put it in "<code>".env"</code>" on the server as "<code>"MCP_WRITE_TOKEN="</code>", then run "<code>"docker compose up -d mcp"</code>". Never paste it in a chat."</p>
            </Show>
        </div>
        <div class="card">
            <table>
                <thead><tr><th>"Name"</th><th>"Made"</th><th>"Last used"</th><th></th></tr></thead>
                <tbody>{rows}</tbody>
            </table>
            <Show when=move || list.get().is_empty()><p class="muted">"No tokens yet."</p></Show>
        </div>
        <Toast msg />
    }
}

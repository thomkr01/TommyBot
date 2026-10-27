use crate::common::*;
use crate::form::Section;
use leptos::prelude::*;
use serde_json::Value;

fn lines(v: &Value, key: &str) -> Vec<String> {
    v[key].as_array().map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Role profiles plus the preview and apply of their channel overwrites (preview uses the saved profiles).
#[component]
pub fn ChannelSetup(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let preview = RwSignal::new(None::<Vec<String>>);
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let (gp, ga) = (guild.clone(), guild.clone());
    let show = move |_| { let g = gp.clone(); msg.set(String::new()); run_act(async move { api("GET", &format!("/api/guilds/{g}/channel-setup/preview"), None).await }, msg, need, move |v| preview.set(Some(lines(&v, "lines")))) };
    let apply = Callback::new(move |_: ()| { let g = ga.clone(); preview.set(None); run_act(async move { api("POST", &format!("/api/guilds/{g}/channel-setup/apply"), None).await }, msg, need, move |v| {
        let failed = lines(&v, "failed");
        msg.set(format!("Applied {} overwrite(s), backup #{} taken first (Backups can revert it).{}", v["applied"], v["backup"], if failed.is_empty() { String::new() } else { format!(" Failed:\n{}", failed.join("\n")) }));
    }) });
    view! {
        <Section guild=guild.clone() name="channel_profiles".to_string() enrolled />
        <div class="card">
            <div class="row"><h2>"Apply to the server"</h2><span class="sp"></span><button class="ghost" on:click=show>"Preview changes"</button></div>
            <p class="muted">"Save the profiles first. Apply sets each role's overwrite on the picked channels and categories, skips channels that are already right and can be run again to resume."</p>
            {move || preview.get().map(|lines| view! {
                {if lines.is_empty() { view! { <p class="info">"Every channel already matches the profiles."</p> }.into_any() } else {
                    view! { <p class="muted">{format!("{} overwrite(s) would change:", lines.len())}</p><pre>{lines.join("\n")}</pre> }.into_any() }}
                <div class="row">
                    <button class="ghost" on:click=move |_| preview.set(None)>"Cancel"</button>
                    <Show when={let n = lines.len(); move || n > 0}><button on:click=move |_| apply.run(())>"Apply"</button></Show>
                </div>
            })}
            <pre class="info">{move || msg.get()}</pre>
        </div>
        <StepUp need enrolled />
    }
}

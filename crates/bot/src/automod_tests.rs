//! Tests of automod's message text and its punishment paths (own file to keep automod.rs short).
//! The DB tests run only when DATABASE_URL points at a scratch Postgres, with the mock Discord in `test_support`.
use super::{parts::app_text, *};
use crate::test_support::T;
use serde_json::{json, Value};

/// A guild message by user 50 in channel 7, with `extra` fields on top.
pub(crate) fn msg(extra: Value) -> Message {
    let mut m = json!({
        "id": "100", "channel_id": "7", "type": 0, "content": "", "timestamp": "2026-01-01T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
        "author": {"id": "50", "username": "x", "discriminator": "0", "avatar": null},
    });
    for (k, v) in extra.as_object().unwrap() {
        m[k] = v.clone();
    }
    serde_json::from_value(m).unwrap()
}

#[test]
fn app_posts_are_read_with_their_embeds_and_link_buttons() {
    let embeds = json!([
        {"type": "rich", "title": "Free nitro", "description": "claim at steamcommunity.ru",
         "fields": [{"name": "f", "value": "discord.gg/abc", "inline": false}],
         "footer": {"text": "foot"}, "author": {"name": "auth", "url": "https://a.example"}},
        {"type": "link", "url": "https://news.example/x", "description": "preview text"},
        {"type": "rich", "url": "https://news.example/x", "description": "own embed"},
    ]);
    let components = json!([{"type": 1, "components": [
        {"type": 2, "style": 5, "label": "Go", "url": "https://phish.example"}
    ]}]);
    let hook = msg(
        json!({"webhook_id": "9", "content": "see https://news.example/x",
                          "embeds": embeds, "components": components}),
    );
    let text = app_text(&hook);
    for part in [
        "Free nitro",
        "steamcommunity.ru",
        "discord.gg/abc",
        "foot",
        "auth",
        "https://a.example",
        "https://phish.example",
        "own embed",
    ] {
        assert!(text.contains(part), "{part} missing in {text:?}");
    }
    // Discord's preview of a link in the content is not read twice.
    assert!(!text.contains("preview text"));
    // A member's own post: Discord made its embeds.
    let member = msg(json!({"embeds": embeds}));
    assert_eq!(app_text(&member), "");
    // A member forwarding an app's post: its rich embeds and buttons are read, previews are not.
    let forward = msg(json!({"message_snapshots": [{"message": {
        "embeds": embeds, "components": components, "content": "see https://news.example/x",
        "attachments": [], "mentions": [], "mention_roles": [], "type": 0,
        "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null,
    }}]}));
    let text = app_text(&forward);
    for part in ["steamcommunity.ru", "own embed", "https://phish.example"] {
        assert!(text.contains(part), "{part} missing in {text:?}");
    }
    assert!(!text.contains("preview text"));
}

#[test]
fn only_webhooks_and_apps_add_files_in_an_edit() {
    let edited = json!("2026-01-01T00:01:00.000000+00:00");
    assert!(files_can_be_new(&msg(json!({}))));
    assert!(!files_can_be_new(&msg(json!({"edited_timestamp": edited}))));
    assert!(files_can_be_new(&msg(
        json!({"edited_timestamp": edited, "application_id": "3"})
    )));
    assert!(files_can_be_new(&msg(
        json!({"edited_timestamp": edited, "webhook_id": "9"})
    )));
}

#[test]
fn forwarded_attachments_count_as_the_message_s_own() {
    let file = |id: &str| {
        json!({"id": id, "filename": "a.png", "size": 10, "url": "https://cdn.discordapp.com/a.png",
               "proxy_url": "https://media.discordapp.net/a.png", "content_type": "image/png"})
    };
    let m = msg(json!({
        "attachments": [file("1")],
        "message_snapshots": [{"message": {
            "attachments": [file("2")], "content": "", "embeds": [], "mentions": [], "mention_roles": [],
            "type": 0, "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null,
        }}],
    }));
    let ids: Vec<u64> = attachments(&m).map(|a| a.id.get()).collect();
    assert_eq!(ids, [1, 2]);
}

/// A failed timeout (the mock has no member PATCH) is reported and still runs the strike-cap lockdown.
#[tokio::test]
async fn a_failed_strike_timeout_still_locks_at_the_cap() {
    let Some(t) = T::new(90).await else { return };
    let gid = t.g(0);
    t.server_config(
        0,
        t.owner(),
        json!({"automod": {"enabled": true, "auto_lockdown": {"enabled": true, "strikes": 1}}}),
    )
    .await;
    let cfg = t.app.cfg(gid.get()).await.unwrap();
    let user = Id::new(t.base + 5);
    let r = strike_member(&t.app, &cfg, gid, (user, "x"), Id::new(7), None, None).await;
    assert!(r.unwrap_err().downcast_ref::<Punished>().is_some());
    assert!(t.calls("PATCH") >= 1, "timeout tried");
    assert!(
        t.calls("/roles") >= 1,
        "lockdown planned after the failed timeout"
    );
    t.exec("DELETE FROM automod_strikes WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// The restore job belongs to its membership, and a member who left or waits for verification
/// gets nothing (and no retried job).
#[tokio::test]
async fn main_roles_come_back_only_to_the_same_verified_membership() {
    let Some(t) = T::new(91).await else { return };
    let gid = t.g(0);
    t.server_config(
        0,
        t.owner(),
        json!({"lockdown": {"main_roles": ["11"]}, "misc": {"remove_main_roles_on_timeout": true}}),
    )
    .await;
    let user = t.base + 5;
    let up: twilight_model::gateway::payload::incoming::MemberUpdate =
        serde_json::from_value(json!({
            "guild_id": gid.to_string(), "roles": ["11"], "pending": false, "deaf": false, "mute": false,
            "joined_at": "2026-01-01T00:00:00.000000+00:00",
            "communication_disabled_until": "2099-01-01T00:00:00.000000+00:00",
            "user": {"id": user.to_string(), "username": "x", "discriminator": "0", "avatar": null},
        }))
        .unwrap();
    // The role removal fails at the mock, after the job was queued.
    let _ = crate::timeout_roles::on_member_update(&t.app, &up).await;
    let job: Value = sqlx::query_scalar(
        "SELECT payload FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'restore_main_roles'",
    )
    .bind(gid.get() as i64)
    .fetch_one(&t.app.pool)
    .await
    .unwrap();
    let joined = up.joined_at.unwrap().as_micros();
    assert_eq!(job["joined_at"], json!(joined));
    // Left the server: done, not retried.
    crate::timeout_roles::restore(&t.app, gid, &job)
        .await
        .unwrap();
    // Back (same membership) but held for verification: nothing is given.
    t.set(|m| {
        m.members.insert((gid.get(), user), vec![]);
    });
    sqlx::query(
        "INSERT INTO member_roles (guild_id, user_id, roles, joined_at, pending) VALUES ($1, $2, '{}', now(), true)",
    )
    .bind(gid.get() as i64)
    .bind(user as i64)
    .execute(&t.app.pool)
    .await
    .unwrap();
    let mut same = job.clone();
    same["joined_at"] = Value::Null;
    crate::timeout_roles::restore(&t.app, gid, &same)
        .await
        .unwrap();
    assert_eq!(t.calls("PUT"), 0, "no role added");
    t.exec("DELETE FROM member_roles WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// A member quarantined when the timeout ends gets the main roles into the quarantine's saved list,
/// not on their account, so the release gives them back.
#[tokio::test]
async fn main_roles_of_a_quarantined_member_go_to_the_quarantine() {
    let Some(t) = T::new(95).await else { return };
    let gid = t.g(0);
    t.server_config(0, t.owner(), json!({})).await;
    let user = t.base + 5;
    t.set(|m| {
        m.members.insert((gid.get(), user), vec![]);
    });
    sqlx::query("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, $2, '{12}')")
        .bind(gid.get() as i64)
        .bind(user as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    let job = json!({"user": user, "roles": [11, 12]});
    crate::timeout_roles::restore(&t.app, gid, &job)
        .await
        .unwrap();
    let mut saved: Vec<i64> = sqlx::query_scalar(
        "SELECT saved_roles FROM quarantines WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(gid.get() as i64)
    .bind(user as i64)
    .fetch_one(&t.app.pool)
    .await
    .unwrap();
    saved.sort();
    assert_eq!(saved, [11, 12]);
    assert_eq!(t.calls("PUT"), 0, "no role added during the quarantine");
    t.cleanup().await;
}

/// Heat panic's raiders are held with automod off, also when a server app posts for them.
#[tokio::test]
async fn raiders_are_held_with_automod_off() {
    let Some(t) = T::new(89).await else { return };
    let gid = t.g(0);
    t.server_config(0, t.owner(), json!({"automod": {"enabled": false}}))
        .await;
    let raider = t.base + 5;
    t.app
        .heat_raiders
        .lock()
        .unwrap()
        .insert(gid.get(), ([raider].into(), crate::app::now_secs() + 600));
    let own = msg(json!({"guild_id": gid.to_string(),
        "author": {"id": raider.to_string(), "username": "x", "discriminator": "0", "avatar": null}}));
    let _ = on_message(
        &t.app,
        &MessageCreate {
            channel_type: None,
            message: own,
        },
    )
    .await;
    assert_eq!(
        t.calls("DELETE /channels/7/messages/100"),
        1,
        "their post is removed"
    );
    assert_eq!(
        t.calls(&format!("PATCH /guilds/{gid}/members/{raider}")),
        1,
        "and they are timed out"
    );
    let via_app = msg(json!({"guild_id": gid.to_string(), "id": "101",
        "author": {"id": "1", "username": "bot", "discriminator": "0", "avatar": null, "bot": true},
        "interaction_metadata": {"id": "5", "type": 2, "authorizing_integration_owners": {"0": "1"},
            "user": {"id": raider.to_string(), "username": "x", "discriminator": "0", "avatar": null}}}));
    let _ = on_message(
        &t.app,
        &MessageCreate {
            channel_type: None,
            message: via_app,
        },
    )
    .await;
    assert_eq!(
        t.calls("DELETE /channels/7/messages/101"),
        1,
        "a server app's post for them too"
    );
    assert_eq!(t.calls(&format!("PATCH /guilds/{gid}/members/{raider}")), 2);
    // Their button click updating a shared panel does not delete the panel.
    let panel = msg(json!({"guild_id": gid.to_string(), "id": "102",
        "author": {"id": "1", "username": "bot", "discriminator": "0", "avatar": null, "bot": true},
        "interaction_metadata": {"id": "6", "type": 3, "authorizing_integration_owners": {"0": "1"},
            "user": {"id": raider.to_string(), "username": "x", "discriminator": "0", "avatar": null}}}));
    let _ = on_edit(&t.app, &panel).await;
    assert_eq!(t.calls("DELETE /channels/7/messages/102"), 0);
    t.cleanup().await;
}

#[test]
fn an_app_message_is_filtered_once_per_text() {
    let m = |content: &str| msg(json!({"id": "910001", "webhook_id": "9", "content": content}));
    assert!(new_text(&m("hi")), "the post");
    assert!(!new_text(&m("hi")), "a preview update with the same text");
    assert!(
        new_text(&m("hi discord.gg/x")),
        "a fill or edit with new text"
    );
    assert!(!new_text(&m("hi discord.gg/x")));
    // An image-only deferred fill: same (empty) text, a new file.
    let fill =
        |files: Value| msg(json!({"id": "910002", "application_id": "3", "attachments": files}));
    assert!(new_text(&fill(json!([]))), "the LOADING post");
    let img = json!([{"id": "1", "filename": "a.png", "size": 10, "url": "https://cdn.discordapp.com/a.png",
        "proxy_url": "https://media.discordapp.net/a.png", "content_type": "image/png"}]);
    assert!(new_text(&fill(img.clone())), "the fill adds an image");
    assert!(!new_text(&fill(img)), "the same image again");
}

/// A user-installed app's reply to a button click is deleted for its text, but the clicker did not
/// write it: no strike or timeout. A reply to a command is the member's post and is punished.
#[tokio::test]
async fn a_button_reply_is_deleted_but_punishes_nobody() {
    let Some(t) = T::new(88).await else { return };
    let gid = t.g(0);
    let cfg = json!({"automod": {"enabled": true,
        "filters": {"words": {"filter": {"enabled": true}, "strict": ["scam"]}}}});
    t.server_config(0, t.owner(), cfg).await;
    let reply = |id: &str, kind: u8, user: u64| {
        let m = msg(
            json!({"guild_id": gid.to_string(), "id": id, "content": "scam", "application_id": "3",
            "author": {"id": "3", "username": "app", "discriminator": "0", "avatar": null, "bot": true},
            "interaction_metadata": {"id": "6", "type": kind, "authorizing_integration_owners": {"1": user.to_string()},
                "user": {"id": user.to_string(), "username": "x", "discriminator": "0", "avatar": null}}}),
        );
        MessageCreate {
            channel_type: None,
            message: m,
        }
    };
    let clicker = t.base + 5;
    let _ = on_message(&t.app, &reply("100", 3, clicker)).await;
    assert_eq!(t.calls("DELETE /channels/7/messages/100"), 1, "deleted");
    assert_eq!(t.calls("PATCH"), 0, "nobody timed out");
    let strikes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM automod_strikes WHERE guild_id = $1")
            .bind(gid.get() as i64)
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(strikes, 0, "no strike");
    let _ = on_message(&t.app, &reply("101", 2, t.base + 6)).await;
    assert!(t.calls("PATCH") >= 1, "a command reply strikes the member");
    t.exec("DELETE FROM automod_strikes WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// The catch channel's test mode only logs, so automod still sees the message.
#[tokio::test]
async fn catch_log_mode_leaves_the_message_to_automod() {
    let Some(t) = T::new(92).await else { return };
    let gid = t.g(0);
    t.server_config(
        0,
        t.owner(),
        json!({"catch": {"enabled": true, "channel": "7", "action": "Log"}}),
    )
    .await;
    let m = MessageCreate {
        channel_type: None,
        message: msg(json!({"guild_id": gid.to_string()})),
    };
    assert!(!crate::catch::on_message(&t.app, &m).await.unwrap());
    t.cleanup().await;
}

/// A whitelisted member's native hits neither act nor count towards a server scope.
#[tokio::test]
async fn native_hits_of_whitelisted_members_do_not_fill_a_scope() {
    let Some(t) = T::new(93).await else { return };
    let gid = t.g(0);
    t.server_config(
        0,
        t.owner(),
        json!({"discord_automod": {
            "actions": {"77": ["timeout"]},
            "scopes": {"77": {"scope": "server", "triggers": 2, "duration_s": 60}},
        }}),
    )
    .await;
    t.set(|m| {
        m.rules
            .insert(gid.get(), vec![native_rule(gid.get(), &["x"])]);
    });
    let hit =
        |user: u64| -> twilight_model::gateway::payload::incoming::AutoModerationActionExecution {
            serde_json::from_value(json!({
                "guild_id": gid.to_string(), "action": {"type": 1}, "rule_id": "77",
                "rule_trigger_type": 1, "user_id": user.to_string(), "content": "x",
                "matched_keyword": null, "matched_content": null,
            }))
            .unwrap()
        };
    crate::native::on_native_hit(&t.app, &hit(t.owner()))
        .await
        .unwrap();
    crate::native::on_native_hit(&t.app, &hit(t.base + 5))
        .await
        .unwrap();
    assert_eq!(t.calls("PATCH"), 0, "the owner's hit did not count");
    t.cleanup().await;
}

/// Thomas 2026-11-13: a Discord AutoMod rule changed by someone without a permit triggers no
/// TommyBot action until a trusted change.
#[tokio::test]
async fn a_rule_changed_without_a_permit_triggers_nothing_until_a_trusted_change() {
    let Some(t) = T::new(264).await else { return };
    let gid = t.g(0);
    t.server_config(
        0,
        t.owner(),
        json!({"discord_automod": {"actions": {"77": ["timeout"]}}}),
    )
    .await;
    t.set(|m| {
        m.rules
            .insert(gid.get(), vec![native_rule(gid.get(), &["x"])]);
    });
    let edit = |id: u64, actor: u64| -> twilight_model::guild::audit_log::AuditLogEntry {
        serde_json::from_value(json!({
            "id": id.to_string(), "action_type": 141, "guild_id": gid.to_string(), "target_id": "77",
            "user_id": actor.to_string(), "reason": null, "changes": [],
        }))
        .unwrap()
    };
    let hit: twilight_model::gateway::payload::incoming::AutoModerationActionExecution =
        serde_json::from_value(json!({
            "guild_id": gid.to_string(), "action": {"type": 1}, "rule_id": "77",
            "rule_trigger_type": 1, "user_id": (t.base + 6).to_string(), "content": "x",
            "matched_keyword": null, "matched_content": null,
        }))
        .unwrap();
    crate::native::on_rule_audit(&t.app, &edit(2, t.base + 5))
        .await
        .unwrap();
    // An older trusted entry arriving late (a replay, a parallel task) does not clear it.
    crate::native::on_rule_audit(&t.app, &edit(1, t.owner()))
        .await
        .unwrap();
    // The bot's own sync of the rule keeps the verdict.
    crate::native::on_rule_audit(&t.app, &edit(3, crate::test_support::ME))
        .await
        .unwrap();
    crate::native::on_native_hit(&t.app, &hit).await.unwrap();
    assert_eq!(t.calls("PATCH"), 0, "no action after an untrusted change");
    crate::native::rule_changed(gid.get(), 77);
    // A fresh entry: the gateway update just now is its own, not a later change.
    let now = ((crate::automod::now_ms() - 1_420_070_400_000) as u64) << 22;
    crate::native::on_rule_audit(&t.app, &edit(now, t.owner()))
        .await
        .unwrap();
    crate::native::on_native_hit(&t.app, &hit).await.unwrap();
    assert!(t.calls("PATCH") >= 1, "acts again after the owner's change");
    t.cleanup().await;
}

/// Native rule 77 as Discord's JSON, blocking `words`.
fn native_rule(guild: u64, words: &[&str]) -> Value {
    json!({
        "id": "77", "guild_id": guild.to_string(), "name": "r", "creator_id": "5",
        "event_type": 1, "trigger_type": 1, "trigger_metadata": {"keyword_filter": words},
        "actions": [{"type": 1}], "enabled": true, "exempt_roles": [], "exempt_channels": [],
    })
}

fn native_hit(
    guild: u64,
    rule: u64,
    user: u64,
) -> twilight_model::gateway::payload::incoming::AutoModerationActionExecution {
    serde_json::from_value(json!({
        "guild_id": guild.to_string(), "action": {"type": 1}, "rule_id": rule.to_string(),
        "rule_trigger_type": 1, "user_id": user.to_string(), "content": "x",
        "matched_keyword": null, "matched_content": null,
    }))
    .unwrap()
}

fn rule_edit(guild: u64, id: u64, actor: u64) -> twilight_model::guild::audit_log::AuditLogEntry {
    serde_json::from_value(json!({
        "id": id.to_string(), "action_type": 141, "guild_id": guild.to_string(),
        "target_id": "77", "user_id": actor.to_string(), "reason": null, "changes": [],
    }))
    .unwrap()
}

/// Plan item 1: the bot acts on a native rule only in a state a trusted person approved. Rules that
/// exist at GuildCreate are approved once, also when the log fails; a hit approves nothing. A
/// change with no audit entry (the bot lacks View Audit Log) stops actions until a trusted change.
#[tokio::test]
async fn a_native_rule_acts_only_in_an_approved_state() {
    let Some(t) = T::new(741).await else { return };
    let g = t.g(0).get();
    t.server_config(
        0,
        t.owner(),
        json!({"discord_automod": {"actions": {"77": ["timeout"]}},
               "logging": {"routes": {"auto_mod": "88"}}}),
    )
    .await;
    t.set(|m| {
        m.rules.insert(g, vec![native_rule(g, &["x"])]);
        m.fail_rules = true;
        m.fail_channels.push(88);
    });
    let hit = |user: u64| {
        let app = &t.app;
        async move { crate::native::on_native_hit(app, &native_hit(g, 77, user)).await }
    };
    assert!(crate::native::on_guild_create(&t.app, t.g(0))
        .await
        .is_err());
    t.set(|m| m.fail_rules = false);
    hit(t.base + 1).await.unwrap();
    assert_eq!(t.calls("PATCH"), 0, "a hit approves nothing");
    crate::native::on_guild_create(&t.app, t.g(0))
        .await
        .unwrap();
    hit(t.base + 2).await.unwrap();
    assert_eq!(t.calls("PATCH"), 1, "approved once at GuildCreate");
    assert_eq!(
        t.count(
            "SELECT count(*) FROM automod_rule_approved WHERE guild_id = $1 AND approved_by = 0"
        )
        .await,
        2,
        "rule 77 and the guild's done marker"
    );
    // Someone changes the rule in Discord; no audit entry comes.
    let changed = native_rule(g, &["y"]);
    t.set(|m| {
        m.rules.insert(g, vec![changed.clone()]);
    });
    crate::native::rule_seen(&serde_json::from_value(changed).unwrap());
    hit(t.base + 3).await.unwrap();
    assert_eq!(t.calls("PATCH"), 1, "no action on a state nobody approved");
    // The owner's change in Discord approves the rule as it is now.
    crate::native::on_rule_audit(&t.app, &rule_edit(g, 1, t.owner()))
        .await
        .unwrap();
    hit(t.base + 4).await.unwrap();
    assert_eq!(t.calls("PATCH"), 2, "acts again after the owner's change");
    t.cleanup().await;
}

/// A trusted entry approves the fetched rule only if no gateway update came after it; a failed
/// fetch is logged and approves nothing. GuildCreate drops the rules held from the old session.
#[tokio::test]
async fn a_trusted_entry_approves_only_its_own_state() {
    let Some(t) = T::new(743).await else { return };
    let g = t.g(0).get();
    t.server_config(
        0,
        t.owner(),
        json!({"discord_automod": {"actions": {"77": ["timeout"]}},
               "logging": {"routes": {"auto_mod": "88"}}}),
    )
    .await;
    t.app.log_public.lock().unwrap().insert(
        twilight_model::id::Id::new(88),
        (false, std::time::Instant::now()),
    );
    t.set(|m| {
        m.rules.insert(g, vec![native_rule(g, &["x"])]);
    });
    let approved = || {
        t.count("SELECT count(*) FROM automod_rule_approved WHERE guild_id = $1 AND rule_id = 77")
    };
    // An entry from a minute ago, and a gateway update just now: a later change.
    let old = ((crate::automod::now_ms() - 60_000 - 1_420_070_400_000) as u64) << 22;
    crate::native::rule_changed(g, 77);
    crate::native::on_rule_audit(&t.app, &rule_edit(g, old, t.owner()))
        .await
        .unwrap();
    assert_eq!(approved().await, 0, "left for the later change's entry");
    // A fresh entry, its fetch fails.
    let fresh = ((crate::automod::now_ms() - 1_420_070_400_000) as u64) << 22;
    t.set(|m| m.fail_rules = true);
    crate::native::rule_changed(g, 77);
    crate::native::on_rule_audit(&t.app, &rule_edit(g, fresh, t.owner()))
        .await
        .unwrap();
    assert_eq!(approved().await, 0, "a failed fetch approves nothing");
    assert_eq!(t.calls("POST /channels/88/messages"), 1, "and is logged");
    t.set(|m| m.fail_rules = false);
    crate::native::rule_changed(g, 77);
    crate::native::on_rule_audit(&t.app, &rule_edit(g, fresh + 1, t.owner()))
        .await
        .unwrap();
    assert_eq!(approved().await, 1, "its own state is approved");
    crate::native::on_native_hit(&t.app, &native_hit(g, 77, t.base + 1))
        .await
        .unwrap();
    assert_eq!(t.calls("PATCH"), 1);
    // A new session missed a change: GuildCreate drops the held state, the hit fetches it.
    t.set(|m| {
        m.rules.insert(g, vec![native_rule(g, &["y"])]);
    });
    t.exec("INSERT INTO automod_rule_approved (guild_id, rule_id, fingerprint, approved_by) VALUES ($1, 0, '', 0)").await;
    crate::native::on_guild_create(&t.app, t.g(0))
        .await
        .unwrap();
    crate::native::on_native_hit(&t.app, &native_hit(g, 77, t.base + 2))
        .await
        .unwrap();
    assert_eq!(t.calls("PATCH"), 1, "the missed change is not approved");
    t.cleanup().await;
}

/// The bot hashes twilight's model of a rule, the dashboard and the sync Discord's JSON.
#[test]
fn twilight_and_discord_json_give_one_fingerprint() {
    let raw = json!({
        "id": "77", "guild_id": "1", "name": "r", "creator_id": "5", "event_type": 1,
        "trigger_type": 1, "enabled": true, "exempt_roles": ["3"], "exempt_channels": [],
        "trigger_metadata": {"keyword_filter": ["a"], "regex_patterns": [], "allow_list": []},
        "actions": [{"type": 1, "metadata": {}}, {"type": 2, "metadata": {"channel_id": "4"}}],
    });
    let model: twilight_model::guild::auto_moderation::AutoModerationRule =
        serde_json::from_value(raw.clone()).unwrap();
    let fp = tommybot_automod::discord_api::rule_fingerprint;
    assert_eq!(fp(&raw), fp(&serde_json::to_value(model).unwrap()));
}

/// The first-start approval skips a rule an audit entry shows was changed without a permit.
#[tokio::test]
async fn first_start_skips_a_rule_changed_without_a_permit() {
    let Some(t) = T::new(742).await else { return };
    let g = t.g(0).get();
    t.server_config(
        0,
        t.owner(),
        json!({"discord_automod": {"actions": {"77": ["timeout"], "78": ["timeout"]}}}),
    )
    .await;
    let mut other = native_rule(g, &["z"]);
    other["id"] = json!("78");
    t.set(|m| {
        m.rules.insert(g, vec![native_rule(g, &["x"]), other]);
    });
    crate::native::on_rule_audit(&t.app, &rule_edit(g, 1, t.base + 5))
        .await
        .unwrap();
    crate::native::on_guild_create(&t.app, t.g(0))
        .await
        .unwrap();
    crate::native::on_native_hit(&t.app, &native_hit(g, 78, t.base + 1))
        .await
        .unwrap();
    assert_eq!(t.calls("PATCH"), 1, "rule 78 was approved");
    assert_eq!(
        t.count("SELECT count(*) FROM automod_rule_approved WHERE guild_id = $1 AND rule_id = 77")
            .await,
        0,
        "rule 77 was not"
    );
    t.cleanup().await;
}

fn create(m: Message) -> MessageCreate {
    MessageCreate {
        channel_type: None,
        message: m,
    }
}

/// A scam link with its own Ban: an automod-permit holder is banned too (a hacked mod), the grace
/// window after an earlier punishment does not soften it, and the same message is punished once.
/// The permit still skips a blacklisted word.
#[tokio::test]
async fn a_scam_ban_skips_the_permit_and_the_grace_window() {
    let Some(t) = T::new(251).await else { return };
    let gid = t.g(0);
    let cfg = json!({"automod": {"enabled": true, "filters": {
        "scam_links": {"enabled": true, "action": "ban"},
        "words": {"filter": {"enabled": true}, "strict": ["darn"]}}}});
    t.server_config(0, t.owner(), cfg).await;
    t.exec("INSERT INTO permits (id, guild_id, name, perms, whitelist) VALUES ($1, $1, 'Staff', '{}', '{automod}')")
        .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $1, 556)")
        .await;
    let (staff, member) = (t.base + 5, t.base + 6);
    t.set(|m| {
        m.members.insert((gid.get(), staff), vec![556]);
        m.members.insert((gid.get(), member), vec![]);
    });
    let post = |id: &str, user: u64, content: &str| {
        create(msg(
            json!({"guild_id": gid.to_string(), "id": id, "content": content,
            "author": {"id": user.to_string(), "username": "x", "discriminator": "0", "avatar": "0123456789abcdef0123456789abcdef"}}),
        ))
    };
    let scam = "free nitro https://discord-nitro.ru/claim";
    on_message(&t.app, &post("100", staff, "darn it"))
        .await
        .unwrap();
    assert_eq!(t.calls("DELETE"), 0, "the permit skips the word filter");
    let _ = on_message(&t.app, &post("101", staff, scam)).await;
    assert_eq!(
        t.calls(&format!("PUT /guilds/{gid}/bans/{staff}")),
        1,
        "permit holder banned"
    );
    // The member was just punished for a heat burst.
    t.app
        .heat
        .lock()
        .unwrap()
        .entry((gid.get(), member))
        .or_default()
        .0
        .punished_ms = now_ms();
    let _ = on_message(&t.app, &post("102", member, scam)).await;
    let ban = format!("PUT /guilds/{gid}/bans/{member}");
    assert_eq!(t.calls(&ban), 1, "banned inside the grace window");
    // The same message again (the NSFW scan, an edit): deleted, not punished twice.
    let _ = on_edit(&t.app, &post("102", member, scam).message).await;
    assert_eq!(t.calls(&ban), 1, "one message, one ban");
    assert_eq!(t.calls("DELETE /channels/7/messages/102"), 2);
    t.exec("DELETE FROM automod_strikes WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// A webhook exempt from spam (listed, or its bot owner on the anti-nuke whitelist) still has an
/// invite deleted; its plain messages stay.
#[tokio::test]
async fn an_exempt_webhook_still_loses_an_invite() {
    let Some(t) = T::new(252).await else { return };
    let gid = t.g(0);
    let cfg = json!({"whitelist": {"webhooks": ["9"]},
        "automod": {"enabled": true, "filters": {"invites": {"enabled": true}}}});
    t.server_config(0, t.owner(), cfg).await;
    let hook = |id: &str, content: &str| {
        create(msg(
            json!({"guild_id": gid.to_string(), "id": id, "content": content, "webhook_id": "9",
            "author": {"id": "9", "username": "hook", "discriminator": "0000", "avatar": null, "bot": true}}),
        ))
    };
    on_message(&t.app, &hook("100", "hello")).await.unwrap();
    on_message(&t.app, &hook("101", "join discord.gg/abc"))
        .await
        .unwrap();
    assert_eq!(t.calls("DELETE /channels/7/messages/100"), 0);
    assert_eq!(t.calls("DELETE /channels/7/messages/101"), 1);
    t.cleanup().await;
}

/// Mentions by a mention-whitelisted member never count towards the mention lockdown.
#[tokio::test]
async fn whitelisted_pings_do_not_lock_the_server() {
    let Some(t) = T::new(253).await else { return };
    let gid = t.g(0);
    let (trusted, member) = (t.base + 5, t.base + 6);
    let cfg = json!({"whitelist": {"mentions": {"users": [trusted.to_string()]}},
        "automod": {"enabled": true, "anti_spam": false,
            "auto_lockdown": {"enabled": true, "mention_count": 2}}});
    t.server_config(0, t.owner(), cfg).await;
    let pings = |id: &str, user: u64| {
        let who = |u: u64| json!({"id": u.to_string(), "username": "y", "discriminator": "0", "avatar": null, "public_flags": 0});
        create(msg(
            json!({"guild_id": gid.to_string(), "id": id, "content": "hi",
            "mentions": [who(1), who(2), who(3)],
            "author": {"id": user.to_string(), "username": "x", "discriminator": "0", "avatar": "0123456789abcdef0123456789abcdef"}}),
        ))
    };
    let _ = on_message(&t.app, &pings("100", trusted)).await;
    assert_eq!(t.calls("/roles"), 0, "not locked");
    let _ = on_message(&t.app, &pings("101", member)).await;
    assert!(t.calls("/roles") >= 1, "a member's pings lock");
    t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// A permit holder writing in the catch channel is caught; only Owner, Extra Owners and Trusted
/// Admins are left alone.
#[tokio::test]
async fn the_catch_channel_catches_permit_holders() {
    let Some(t) = T::new(254).await else { return };
    let gid = t.g(0);
    t.server_config(
        0,
        t.owner(),
        json!({"catch": {"enabled": true, "channel": "7", "action": "Kick"}}),
    )
    .await;
    t.exec("INSERT INTO permits (id, guild_id, name, perms, whitelist) VALUES ($1, $1, 'Staff', '{}', '{automod}')")
        .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $1, 556)")
        .await;
    let staff = t.base + 5;
    t.set(|m| {
        m.members.insert((gid.get(), staff), vec![556]);
    });
    let post = |user: u64| {
        create(msg(json!({"guild_id": gid.to_string(),
            "author": {"id": user.to_string(), "username": "x", "discriminator": "0", "avatar": null}})))
    };
    assert!(crate::catch::on_message(&t.app, &post(staff))
        .await
        .unwrap());
    assert!(!crate::catch::on_message(&t.app, &post(t.owner()))
        .await
        .unwrap());
    t.cleanup().await;
}

#[test]
fn a_timeout_without_duration_uses_the_strike_length_with_strikes_off() {
    let mut am = AutomodCfg::default();
    let fc = FilterCfg {
        action: Some(FilterAction::Timeout),
        ..FilterCfg::default()
    };
    assert_eq!(own_action(&fc, &am), None, "the strikes decide");
    am.strike.enabled = false;
    let secs = am.strike.strike_timeout_ms / 1000;
    assert_eq!(
        own_action(&fc, &am),
        Some((FilterAction::Timeout, Some(secs)))
    );
    let own = FilterCfg {
        duration_secs: Some(30),
        ..fc
    };
    assert_eq!(
        own_action(&own, &am),
        Some((FilterAction::Timeout, Some(30)))
    );
}

/// Giving back a failed pass's message claim restores the old rank, but never erases a stronger
/// claim another pass made meanwhile.
#[test]
fn acted_unclaim_keeps_a_newer_claim() {
    let key = (9_990_001, 1);
    let prev = acted_claim(key, STRIKE_RANK).unwrap();
    acted_unclaim(key, STRIKE_RANK, Some(prev));
    assert!(!ACTED.lock().unwrap().contains_key(&key), "given back");
    let prev = acted_claim(key, STRIKE_RANK).unwrap();
    assert!(
        acted_claim(key, 4).is_some(),
        "a stronger pass claims meanwhile"
    );
    acted_unclaim(key, STRIKE_RANK, Some(prev));
    assert_eq!(
        ACTED.lock().unwrap().get(&key),
        Some(&4),
        "the newer claim stays"
    );
}

/// With "Use Discord AutoMod" on, a message that reached the bot was not blocked by Discord, so
/// the local filter still counts its words.
#[tokio::test]
async fn synced_words_still_count_locally() {
    let Some(t) = T::new(262).await else { return };
    let cfg = json!({"automod": {"enabled": true, "filters": {"words": {
        "filter": {"enabled": true, "action": "delete"}, "strict": ["darn"],
        "use_discord_automod": true}}}});
    t.server_config(0, t.owner(), cfg).await;
    t.set(|m| {
        m.members.insert((t.g(0).get(), t.base + 6), vec![]);
    });
    let _ = on_message(&t.app, &post(&t, "100", t.base + 6, "darn it")).await;
    assert_eq!(t.calls("DELETE /channels/7/messages/100"), 1);
    done(t).await;
}

/// With the auto timeouts off a heat strike is logged as a strike, never as a timeout.
#[test]
fn strikes_off_logs_no_timeout() {
    let act = tommybot_automod::strike::StrikeAction {
        strike: 2,
        timeout_ms: 60_000,
        purge: false,
        reset_heat: false,
    };
    let text = |timed| {
        serde_json::to_string(&heat_log(timed, "x", "Message spam", (5, 7), &act).build()).unwrap()
    };
    let off = text(false);
    assert!(off.contains("got a strike") && off.contains("`2`"), "{off}");
    assert!(
        !off.contains("timed out") && !off.contains("Duration"),
        "{off}"
    );
    assert!(text(true).contains("timed out"));
}

/// A guild with a scam-link filter of `action` (a strike when None), members and a permit
/// holder (`base + 5`, role 556) known to the mock; a post by `user` with message id `id`.
async fn scam_guild(n: u64, action: Option<&str>, extra: Value) -> Option<T> {
    let t = T::new(n).await?;
    let gid = t.g(0);
    let mut cfg = json!({"automod": {"enabled": true, "filters": {
        "scam_links": {"enabled": true, "action": action}}}});
    for (k, v) in extra.as_object().unwrap() {
        cfg["automod"][k] = v.clone();
    }
    t.server_config(0, t.owner(), cfg).await;
    t.exec("INSERT INTO permits (id, guild_id, name, perms, whitelist) VALUES ($1, $1, 'Staff', '{}', '{automod}')")
        .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $1, 556)")
        .await;
    t.set(|m| {
        m.members.insert((gid.get(), t.base + 5), vec![556]);
        for u in 6..=8 {
            m.members.insert((gid.get(), t.base + u), vec![]);
        }
    });
    Some(t)
}

fn post(t: &T, id: &str, user: u64, content: &str) -> MessageCreate {
    create(msg(
        json!({"guild_id": t.g(0).to_string(), "id": id, "content": content,
        "author": {"id": user.to_string(), "username": "x", "discriminator": "0",
            "avatar": "0123456789abcdef0123456789abcdef"}}),
    ))
}

/// Heat already over the max, as after a spam burst.
fn hot() -> tommybot_automod::heat::HeatState {
    tommybot_automod::heat::HeatState {
        value: 1_000.0,
        at_ms: now_ms(),
        ..Default::default()
    }
}

const SCAM: &str = "free nitro https://discord-nitro.ru/claim";

async fn done(t: T) {
    t.exec("DELETE FROM automod_strikes WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// A Trusted Admin's scam link is left alone: immune levels are exempt from every bot action.
#[tokio::test]
async fn a_trusted_admin_stays_exempt() {
    let Some(t) = scam_guild(257, Some("ban"), json!({})).await else {
        return;
    };
    t.exec("INSERT INTO trusted_admins (guild_id, user_id) VALUES ($1, $1 + 7)")
        .await;
    let _ = on_message(&t.app, &post(&t, "100", t.base + 7, SCAM)).await;
    assert_eq!(t.calls("/bans/"), 0, "no ban");
    assert_eq!(t.calls("DELETE"), 0, "kept");
    done(t).await;
}

/// A permit holder's scam link is acted on whatever the heat weight, and their exempt spam took
/// no claim that would soften it.
#[tokio::test]
async fn a_permit_holder_s_scam_is_always_acted_on() {
    let heat = json!({"heat": {"malicious": 10.0}});
    let Some(t) = scam_guild(258, None, heat).await else {
        return;
    };
    let (gid, staff) = (t.g(0), t.base + 5);
    // Their spam trips the heat, but they are exempt from it.
    t.app
        .heat
        .lock()
        .unwrap()
        .entry((gid.get(), staff))
        .or_default()
        .0 = hot();
    on_message(&t.app, &post(&t, "100", staff, "hello"))
        .await
        .unwrap();
    assert_eq!(t.calls("DELETE"), 0, "exempt spam is kept");
    let _ = on_message(&t.app, &post(&t, "101", staff, SCAM)).await;
    let timeout = format!("PATCH /guilds/{gid}/members/{staff}");
    assert_eq!(
        t.calls(&timeout),
        1,
        "struck, not softened by the exempt spam"
    );
    done(t).await;
}

/// A strike for a message's spam, then a scam link edited into the same message with a Ban
/// filter: the stronger punishment lands.
#[tokio::test]
async fn a_strike_then_a_ban_on_the_same_message_escalates() {
    let Some(t) = scam_guild(259, Some("ban"), json!({})).await else {
        return;
    };
    let (gid, member) = (t.g(0), t.base + 6);
    t.app
        .heat
        .lock()
        .unwrap()
        .entry((gid.get(), member))
        .or_default()
        .0 = hot();
    let _ = on_message(&t.app, &post(&t, "100", member, "hello")).await;
    assert_eq!(
        t.calls(&format!("PATCH /guilds/{gid}/members/{member}")),
        1,
        "strike"
    );
    let _ = on_edit(&t.app, &post(&t, "100", member, SCAM).message).await;
    assert_eq!(
        t.calls(&format!("PUT /guilds/{gid}/bans/{member}")),
        1,
        "escalated to the ban"
    );
    done(t).await;
}

/// Three scam messages in a burst with a Warn filter give one warn; the rest are deleted.
#[tokio::test]
async fn a_burst_of_warn_hits_warns_once() {
    let Some(t) = scam_guild(260, Some("warn"), json!({})).await else {
        return;
    };
    let member = t.base + 6;
    for id in ["100", "101", "102"] {
        let _ = on_message(&t.app, &post(&t, id, member, SCAM)).await;
    }
    let warns: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cases WHERE guild_id = $1 AND kind = 'warn'")
            .bind(t.g(0).get() as i64)
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(warns, 1);
    assert_eq!(t.calls("DELETE /channels/7/messages/"), 3);
    done(t).await;
}

/// Two passes over one message at once (a create and its image scan) ban once.
#[tokio::test]
async fn concurrent_passes_punish_once() {
    let Some(t) = scam_guild(261, Some("ban"), json!({})).await else {
        return;
    };
    let member = t.base + 8;
    let m = post(&t, "100", member, SCAM);
    let (a, b) = tokio::join!(on_message(&t.app, &m), on_edit(&t.app, &m.message));
    let _ = (a, b);
    assert_eq!(t.calls(&format!("PUT /guilds/{}/bans/{member}", t.g(0))), 1);
    done(t).await;
}

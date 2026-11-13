//! Tests of automod's message text and its punishment paths (own file to keep automod.rs short).
//! The DB tests run only when DATABASE_URL points at a scratch Postgres, with Ban Sync's mock Discord.
use super::{parts::app_text, *};
use crate::bansync::tests::T;
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
    let Some(t) = T::new(21).await else { return };
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
    let Some(t) = T::new(22).await else { return };
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
    let Some(t) = T::new(23).await else { return };
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
    let Some(t) = T::new(24).await else { return };
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

/// With "Use Discord AutoMod" on, the local filter counts only words Discord's rule could not
/// take, and every word while the sync fails.
#[test]
fn the_local_filter_covers_what_discord_does_not() {
    let mut f = FiltersCfg::default();
    f.words.use_discord_automod = true;
    let long = "x".repeat(61);
    f.words.strict = vec!["darn".into(), long.clone()];
    let text = format!("darn {long}");
    let guild = 4_242;
    assert_eq!(local_word_hits(guild, &f, &text), 1, "only the long word");
    SYNC_FAILED.lock().unwrap().insert(guild);
    assert_eq!(
        local_word_hits(guild, &f, &text),
        2,
        "all while the sync fails"
    );
    SYNC_FAILED.lock().unwrap().remove(&guild);
    f.words.use_discord_automod = false;
    assert_eq!(local_word_hits(guild, &f, &text), 2);
}

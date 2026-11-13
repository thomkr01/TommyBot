//! Behaviour tests of the quarantine state (D1, D2/N1, D6, D7, D12, D18, D55, N5) through entry
//! points that main had too, so each one also shows the bug on main. They run only when
//! DATABASE_URL points at a scratch Postgres; Discord is Ban Sync's mock.
use crate::bansync::tests::{ME, T};
use serde_json::{json, Value};
use twilight_model::{
    channel::Channel,
    gateway::payload::incoming::{MemberAdd, MemberUpdate},
    guild::audit_log::AuditLogEntry,
    id::Id,
};

const Q: u64 = 500;
const ROLE: u64 = 501;
const BOOST: u64 = 502;
const VERIFIED: u64 = 503;
const AUTOROLE: u64 = 504;
const BOT_TOP: u64 = 520;
/// Log channel, known to be private.
const LOGS: u64 = 88;

fn logging() -> Value {
    json!({"routes": {"anti_nuke": LOGS.to_string(), "verification": LOGS.to_string(),
                      "join_gate": LOGS.to_string(), "mod_command": LOGS.to_string(), "server": LOGS.to_string()}})
}

/// Server `i` with `cfg` (plus the log routes), its roles in Discord, and the bot holding the top role.
async fn server(t: &T, i: u64, mut cfg: Value) {
    cfg["logging"] = logging();
    t.server_config(i, t.owner(), cfg).await;
    // server_config seeds an empty guild cache; the role tests need the real one.
    t.app.invalidate_guild(t.g(i));
    let g = t.g(i).get();
    t.set(|m| {
        let all = vec![
            (g, false, 0, 0),
            (Q, false, 0, 1),
            (ROLE, false, 0, 2),
            (BOOST, true, 0, 3),
            (VERIFIED, false, 0, 4),
            (AUTOROLE, false, 0, 5),
            (BOT_TOP, true, 8, 10),
        ];
        m.roles.insert(g, all);
        m.members.insert((g, ME), vec![BOT_TOP]);
    });
    let private = (false, std::time::Instant::now());
    t.app
        .log_public
        .lock()
        .unwrap()
        .insert(Id::new(LOGS), private);
}

fn roles(t: &T, i: u64, u: u64) -> Option<Vec<u64>> {
    t.mock
        .lock()
        .unwrap()
        .members
        .get(&(t.g(i).get(), u))
        .cloned()
}

async fn saved(t: &T, i: u64, u: u64) -> Option<Vec<i64>> {
    sqlx::query_scalar("SELECT saved_roles FROM quarantines WHERE guild_id = $1 AND user_id = $2")
        .bind(t.g(i).get() as i64)
        .bind(u as i64)
        .fetch_optional(&t.app.pool)
        .await
        .unwrap()
}

async fn row(t: &T, i: u64, u: u64, saved: &[i64]) {
    sqlx::query("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, $2, $3)")
        .bind(t.g(i).get() as i64)
        .bind(u as i64)
        .bind(saved)
        .execute(&t.app.pool)
        .await
        .unwrap();
}

fn web_verification() -> Value {
    json!({"enabled": true, "mode": "Web", "target": "Everyone", "on_fail": "Nothing",
           "roles": [VERIFIED.to_string()], "channel": "700"})
}

/// D1: a quarantine row whose member got their roles back by hand is not "already contained": a
/// re-armed nuker is contained again (and today's roles saved), not only reverted.
#[tokio::test]
async fn a_stale_quarantine_row_does_not_count_as_contained() {
    let Some(t) = T::new(150).await else { return };
    let an = json!({"enabled": true, "quarantine_role": Q.to_string(),
                    "limits": {"Ban": {"per_minute": 1, "per_hour": 1}}});
    server(&t, 0, json!({ "antinuke": an })).await;
    let (stale, held) = (t.base * 10 + 1, t.base * 10 + 2);
    // Released by hand (a Quarantine Whitelist user): other roles than the saved ones, no Q.
    row(&t, 0, stale, &[ROLE as i64]).await;
    // Still held in Discord.
    row(&t, 0, held, &[ROLE as i64]).await;
    t.set(|m| {
        m.members.insert((t.g(0).get(), stale), vec![AUTOROLE]);
        m.members.insert((t.g(0).get(), held), vec![Q]);
    });
    let cfg = t.app.cfg(t.g(0).get()).await.unwrap();
    for u in [stale, held] {
        let r = crate::antinuke::count(
            &t.app,
            &cfg,
            t.g(0),
            Id::new(u),
            tommybot_antinuke::Action::Ban,
            Some(9),
            &[],
            false,
            5,
        )
        .await;
        tracing::info!(?r, u, "count");
    }
    assert_eq!(roles(&t, 0, stale), Some(vec![Q]), "contained again");
    assert_eq!(
        saved(&t, 0, stale).await,
        Some(vec![AUTOROLE as i64]),
        "the stale saved list is replaced by today's roles"
    );
    let patch = |u| format!("PATCH /guilds/{}/members/{u}", t.g(0));
    assert_eq!(
        t.calls(&patch(held)),
        0,
        "a held member is not contained twice"
    );
    assert_eq!(saved(&t, 0, held).await, Some(vec![ROLE as i64]));
    t.cleanup().await;
}

/// D6: with the Quarantine role deleted in Discord, a member is still stripped (to the managed
/// roles Discord will not take off) and staff are told the role is missing.
#[tokio::test]
async fn a_deleted_quarantine_role_still_strips_the_member() {
    let Some(t) = T::new(151).await else { return };
    // 599 is not a role of the server any more.
    let an = json!({"enabled": true, "quarantine_role": "599"});
    server(&t, 0, json!({ "antinuke": an })).await;
    let u = t.base * 10 + 1;
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![ROLE, BOOST]);
    });
    let r = crate::antinuke::quarantine_actor(&t.app, t.g(0), Id::new(u), "test").await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(roles(&t, 0, u), Some(vec![BOOST]));
    assert_eq!(saved(&t, 0, u).await, Some(vec![ROLE as i64, BOOST as i64]));
    assert!(t.calls("Quarantine role missing") >= 1, "staff are told");
    t.cleanup().await;
}

/// D2: a Raider List quarantine of a joiner with no roles is a real quarantine: passing web
/// verification does not let them out.
#[tokio::test]
async fn a_raider_list_quarantine_of_a_roleless_joiner_cannot_be_verified_out() {
    let Some(t) = T::new(152).await else { return };
    let cfg = json!({
        "antinuke": {"quarantine_role": Q.to_string()},
        "raider_list": {"enabled": true, "action": "quarantine"},
        "joingate": {"enabled": true},
        "verification": web_verification(),
    });
    server(&t, 0, cfg).await;
    let u = t.base * 10 + 1;
    sqlx::query(
        "INSERT INTO raiders (user_id, status, category, reviewed_at, verified)
         VALUES ($1, 'confirmed', 'raid', now(), true)",
    )
    .bind(u as i64)
    .execute(&t.app.pool)
    .await
    .unwrap();
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![]);
    });
    let m: MemberAdd = serde_json::from_value(json!({
        "guild_id": t.g(0).to_string(), "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
        "user": {"id": u.to_string(), "username": "raider", "discriminator": "0", "avatar": null},
    }))
    .unwrap();
    crate::joingate::on_join(&t.app, &m, false).await.unwrap();
    assert_eq!(roles(&t, 0, u), Some(vec![Q]), "the list quarantined them");
    let r = crate::verify::web_grant(&t.app, t.g(0), Id::new(u), &[]).await;
    assert!(r.is_err(), "verifying must be refused");
    assert_eq!(roles(&t, 0, u), Some(vec![Q]), "still quarantined");
    assert!(saved(&t, 0, u).await.is_some());
    sqlx::query("DELETE FROM raiders WHERE user_id = $1")
        .bind(u as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.cleanup().await;
}

/// D2: an autorole landing before verification's join quarantine does not turn it into a real one.
#[tokio::test]
async fn a_join_quarantine_with_an_autorole_still_lifts_on_verify() {
    let Some(t) = T::new(153).await else { return };
    let mut v = web_verification();
    v["quarantine_on_join"] = json!(true);
    let cfg = json!({"antinuke": {"quarantine_role": Q.to_string()}, "verification": v});
    server(&t, 0, cfg).await;
    let u = t.base * 10 + 1;
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![AUTOROLE]);
    });
    crate::verify::on_join(&t.app, t.g(0), Id::new(u), false, false, None).await;
    assert_eq!(roles(&t, 0, u), Some(vec![Q]), "join quarantine");
    let r = crate::verify::web_grant(&t.app, t.g(0), Id::new(u), &[]).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(roles(&t, 0, u), Some(vec![VERIFIED]));
    assert_eq!(saved(&t, 0, u).await, None, "the join quarantine is over");
    t.cleanup().await;
}

/// N1: turning "quarantine on join" off while someone waits does not strand them in quarantine.
#[tokio::test]
async fn turning_quarantine_on_join_off_does_not_strand_a_verifying_member() {
    let Some(t) = T::new(154).await else { return };
    let mut v = web_verification();
    v["quarantine_on_join"] = json!(true);
    let cfg = json!({"antinuke": {"quarantine_role": Q.to_string()}, "verification": v.clone()});
    server(&t, 0, cfg.clone()).await;
    let u = t.base * 10 + 1;
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![]);
    });
    crate::verify::on_join(&t.app, t.g(0), Id::new(u), false, false, None).await;
    assert_eq!(roles(&t, 0, u), Some(vec![Q]));
    let mut off = cfg;
    off["verification"]["quarantine_on_join"] = json!(false);
    off["logging"] = logging();
    sqlx::query("INSERT INTO guild_config (guild_id, version, config) VALUES ($1, 2, $2)")
        .bind(t.g(0).get() as i64)
        .bind(off)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.app.forget_cfg(t.g(0).get());
    let r = crate::verify::web_grant(&t.app, t.g(0), Id::new(u), &[]).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(roles(&t, 0, u), Some(vec![VERIFIED]), "out of quarantine");
    assert_eq!(saved(&t, 0, u).await, None);
    t.cleanup().await;
}

/// D7: a bot on the Raider List loses its managed role's permissions, which a quarantine alone
/// leaves it.
#[tokio::test]
async fn a_raider_list_quarantine_of_a_bot_clears_its_role() {
    let Some(t) = T::new(155).await else { return };
    let cfg = json!({
        "antinuke": {"quarantine_role": Q.to_string()},
        "raider_list": {"enabled": true, "action": "quarantine"},
        "joingate": {"enabled": true},
    });
    server(&t, 0, cfg).await;
    let (u, own) = (t.base * 10 + 1, 530);
    sqlx::query(
        "INSERT INTO raiders (user_id, status, category, reviewed_at, verified)
         VALUES ($1, 'confirmed', 'raid', now(), true)",
    )
    .bind(u as i64)
    .execute(&t.app.pool)
    .await
    .unwrap();
    t.set(|m| {
        m.bots.push(u);
        m.bot_roles.insert(own, u);
        m.roles
            .get_mut(&t.g(0).get())
            .unwrap()
            .push((own, true, 8, 6));
        m.members.insert((t.g(0).get(), u), vec![own]);
    });
    let m: MemberAdd = serde_json::from_value(json!({
        "guild_id": t.g(0).to_string(), "roles": [own.to_string()],
        "joined_at": "2026-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0,
        "user": {"id": u.to_string(), "username": "raidbot", "discriminator": "0", "avatar": null, "bot": true},
    }))
    .unwrap();
    crate::joingate::on_join(&t.app, &m, false).await.unwrap();
    let perms = t.mock.lock().unwrap().roles[&t.g(0).get()]
        .iter()
        .find(|r| r.0 == own)
        .unwrap()
        .2;
    assert_eq!(perms, 0, "the bot's own role lost its permissions");
    sqlx::query("DELETE FROM raiders WHERE user_id = $1")
        .bind(u as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.cleanup().await;
}

/// D12: a failed undo (the role is above the bot) does not spare the actor.
#[tokio::test]
async fn a_failed_undo_still_quarantines_the_actor() {
    let Some(t) = T::new(156).await else { return };
    let an = json!({"enabled": true, "strict_mode": true, "quarantine_role": Q.to_string()});
    server(&t, 0, json!({ "antinuke": an })).await;
    let actor = t.base * 10 + 1;
    t.set(|m| {
        m.fail_role_edit = true;
        m.members.insert((t.g(0).get(), actor), vec![ROLE]);
    });
    let e: AuditLogEntry = serde_json::from_value(json!({
        "id": "1", "action_type": 31, "guild_id": t.g(0).to_string(), "target_id": ROLE.to_string(),
        "user_id": actor.to_string(), "reason": null,
        "changes": [{"key": "permissions", "old_value": "0", "new_value": "8"}],
    }))
    .unwrap();
    let r = crate::antinuke::on_audit(&t.app, &e).await;
    tracing::info!(?r, "on_audit");
    assert!(t.calls(&format!("PATCH /guilds/{}/roles/{ROLE}", t.g(0))) >= 1);
    assert_eq!(roles(&t, 0, actor), Some(vec![Q]), "quarantined anyway");
    t.cleanup().await;
}

/// D18: a managed role Discord will not take off does not keep enforce_permits from the rest.
#[tokio::test]
async fn enforce_permits_skips_managed_roles() {
    let Some(t) = T::new(157).await else { return };
    server(&t, 0, json!({"misc": {"enforce_permits": true}})).await;
    let (u, managed, plain) = (t.base * 10 + 1, 510, 511);
    t.set(|m| {
        let all = m.roles.get_mut(&t.g(0).get()).unwrap();
        all.push((managed, true, 8, 6));
        all.push((plain, false, 8, 7));
        m.members.insert((t.g(0).get(), u), vec![managed, plain]);
    });
    let m: MemberUpdate = serde_json::from_value(json!({
        "guild_id": t.g(0).to_string(), "roles": [managed.to_string(), plain.to_string()],
        "joined_at": "2026-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false,
        "pending": false, "avatar": null, "communication_disabled_until": null, "nick": null,
        "premium_since": null,
        "user": {"id": u.to_string(), "username": "u", "discriminator": "0", "avatar": null},
    }))
    .unwrap();
    let r = crate::antinuke::enforce_permits(&t.app, &m).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(
        roles(&t, 0, u),
        Some(vec![managed]),
        "the plain role is gone"
    );
    t.cleanup().await;
}

/// D55: enforce_quarantine gives the verification channel the shape the join quarantine reads
/// (and repairs an older overwrite), also a channel the dashboard made and has not saved yet while
/// verification is on and the saved channel is gone; other channels still get the full deny.
#[tokio::test]
async fn enforce_quarantine_gives_the_verification_channel_its_shape() {
    use tommybot_core::sections::{QUARANTINE_DENY, VERIFY_ALLOW, VERIFY_DENY};
    let Some(t) = T::new(158).await else { return };
    let q = json!({"quarantine_role": Q.to_string()});
    let misc = json!({"enforce_quarantine_perms": true});
    let mut unsaved = web_verification();
    unsaved.as_object_mut().unwrap().remove("channel");
    let mut dead = web_verification();
    dead["channel"] = json!("799"); // deleted in Discord: GET answers 404
    let mut off = web_verification();
    off["enabled"] = json!(false);
    off.as_object_mut().unwrap().remove("channel");
    for (i, v) in [web_verification(), unsaved, dead, off]
        .into_iter()
        .enumerate()
    {
        server(
            &t,
            i as u64,
            json!({"antinuke": q, "misc": misc, "verification": v}),
        )
        .await;
    }
    let chan = |i: u64, id: u64, allow: u64, deny: u64| -> Channel {
        let ow = json!([{"id": Q.to_string(), "type": 0, "allow": allow.to_string(), "deny": deny.to_string()}]);
        serde_json::from_value(json!({"id": id.to_string(), "type": 0, "guild_id": t.g(i).to_string(),
                                      "permission_overwrites": if allow | deny == 0 { json!([]) } else { ow }}))
        .unwrap()
    };
    let shape = |i, id| chan(i, id, VERIFY_ALLOW, VERIFY_DENY);
    for (i, c) in [
        (0, chan(0, 700, VERIFY_ALLOW, 2048)), // the saved one, with D55's old overwrite
        (0, chan(0, 701, 0, 0)),               // another channel
        (1, shape(1, 702)),                    // made by the dashboard, not saved yet
        (1, chan(1, 703, VERIFY_ALLOW | 2048, 0)),
        (2, shape(2, 704)), // the saved channel was deleted
        (3, shape(3, 705)), // verification is off
    ] {
        let r = crate::setup::enforce_quarantine(&t.app, &c).await;
        assert!(r.is_ok(), "{i}: {r:?}");
    }
    let put = |c: u64, deny: Option<u64>| {
        let m = t.mock.lock().unwrap();
        let to = format!("PUT /channels/{c}/permissions/{Q} ");
        let body = deny.map(|d| format!("\"deny\":\"{d}\""));
        m.log
            .iter()
            .filter(|l| l.starts_with(&to) && body.as_ref().is_none_or(|b| l.contains(b)))
            .count()
    };
    assert_eq!(
        put(700, Some(VERIFY_DENY)),
        1,
        "the saved one is repaired to the shape"
    );
    assert_eq!(
        (put(702, None), put(704, None)),
        (0, 0),
        "a new verification channel keeps it"
    );
    for c in [701, 703, 705] {
        assert_eq!(put(c, Some(QUARANTINE_DENY)), 1, "{c} gets the deny");
    }
    t.cleanup().await;
}

/// Round 2, item 1: a failed member read counts as not contained, so the nuker gets the full run.
#[tokio::test]
async fn a_failed_member_read_does_not_count_as_contained() {
    let Some(t) = T::new(161).await else { return };
    let an = json!({"enabled": true, "quarantine_role": Q.to_string(),
                    "limits": {"Ban": {"per_minute": 1, "per_hour": 1}}});
    server(&t, 0, json!({ "antinuke": an })).await;
    let u = t.base * 10 + 1;
    row(&t, 0, u, &[ROLE as i64]).await;
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![Q]);
        m.fail_member = true;
    });
    let cfg = t.app.cfg(t.g(0).get()).await.unwrap();
    let ban = tommybot_antinuke::Action::Ban;
    let r = crate::antinuke::count(
        &t.app,
        &cfg,
        t.g(0),
        Id::new(u),
        ban,
        Some(9),
        &[],
        false,
        5,
    )
    .await;
    tracing::info!(?r, "count");
    let cases: i64 = sqlx::query_scalar("SELECT count(*) FROM cases WHERE guild_id = $1")
        .bind(t.g(0).get() as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(cases, 1, "the full run opened a case");
    t.cleanup().await;
}

/// Round 2, item 2: a saved list stays when the member holds no other role than quarantine
/// leaves (none at all, or a recreated Quarantine role they lack), and when the other roles come
/// from a membership that began after the quarantine (a rejoin with autoroles).
#[tokio::test]
async fn a_second_quarantine_keeps_a_valid_saved_list() {
    let Some(t) = T::new(82).await else { return };
    server(
        &t,
        0,
        json!({"antinuke": {"quarantine_role": Q.to_string()}}),
    )
    .await;
    let (none, boost, rejoined) = (t.base * 10 + 1, t.base * 10 + 2, t.base * 10 + 3);
    for u in [none, boost, rejoined] {
        row(&t, 0, u, &[ROLE as i64]).await;
    }
    // The mock's members joined in 2020: this quarantine is older than the membership.
    sqlx::query(
        "UPDATE quarantines SET created_at = '2019-01-01' WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(t.g(0).get() as i64)
    .bind(rejoined as i64)
    .execute(&t.app.pool)
    .await
    .unwrap();
    t.set(|m| {
        m.members.insert((t.g(0).get(), none), vec![]);
        m.members.insert((t.g(0).get(), boost), vec![BOOST]);
        m.members.insert((t.g(0).get(), rejoined), vec![AUTOROLE]);
    });
    for u in [none, boost, rejoined] {
        let r = crate::antinuke::quarantine_actor(&t.app, t.g(0), Id::new(u), "test").await;
        assert!(r.is_ok(), "{u}: {r:?}");
        assert_eq!(saved(&t, 0, u).await, Some(vec![ROLE as i64]), "{u}");
    }
    t.cleanup().await;
}

/// Round 2, item 5: with verification off, a rejoin ends a join quarantine instead of holding the
/// member where nobody can verify out.
#[tokio::test]
async fn a_join_quarantine_ends_on_rejoin_when_verification_is_off() {
    let Some(t) = T::new(83).await else { return };
    let mut v = web_verification();
    v["enabled"] = json!(false);
    server(
        &t,
        0,
        json!({"antinuke": {"quarantine_role": Q.to_string()}, "verification": v}),
    )
    .await;
    let u = t.base * 10 + 1;
    row(&t, 0, u, &[]).await;
    sqlx::query("UPDATE quarantines SET on_join = true WHERE guild_id = $1")
        .bind(t.g(0).get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![]);
    });
    let r = crate::antinuke::requarantine_rejoin(&t.app, t.g(0), Id::new(u)).await;
    assert_eq!(r.unwrap(), crate::antinuke::Rejoin::Free);
    assert_eq!(saved(&t, 0, u).await, None);
    assert_eq!(roles(&t, 0, u), Some(vec![]));
    t.cleanup().await;
}

/// N5: a report log the bot cannot post (the nuker hid the channel) still DMs the owner.
#[tokio::test]
async fn a_failed_report_log_still_alerts_the_owner() {
    let Some(t) = T::new(159).await else { return };
    let an = json!({"enabled": true, "quarantine_role": Q.to_string()});
    server(&t, 0, json!({ "antinuke": an })).await;
    let actor = t.base * 10 + 1;
    t.set(|m| {
        m.fail_channels.push(LOGS);
        m.members.insert((t.g(0).get(), actor), vec![ROLE]);
    });
    let since = crate::app::now_secs() - 60;
    let r = crate::antinuke::revert_and_report(&t.app, t.g(0), Id::new(actor), "test", since).await;
    tracing::info!(?r, "revert_and_report");
    assert!(
        t.calls(&format!("POST /channels/{LOGS}/messages")) >= 1,
        "the log was tried"
    );
    assert!(
        t.calls("POST /channels/77/messages") >= 1,
        "the owner got the DM"
    );
    t.cleanup().await;
}

/// Migration 3900: only rows with nothing saved (the old join quarantine) become join quarantines,
/// unless a quarantine case or Raider List quarantine since the row shows a real one.
#[tokio::test]
async fn migration_3900_marks_only_empty_saved_rows_as_join() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = tommybot_core::db::connect(&url).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    // The table as it was before 3900, in a schema of its own that the rollback drops.
    for sql in [
        "CREATE SCHEMA m3900",
        "SET LOCAL search_path = m3900",
        "CREATE TABLE quarantines (guild_id BIGINT, user_id BIGINT, saved_roles BIGINT[] NOT NULL,
                                   created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        "CREATE TABLE cases (guild_id BIGINT, user_id BIGINT, kind TEXT, created_at TIMESTAMPTZ)",
        "CREATE TABLE joingate_actions (guild_id BIGINT, user_id BIGINT, action TEXT, ok BOOLEAN,
                                        created_at TIMESTAMPTZ)",
        "INSERT INTO quarantines VALUES (1, 1, '{}'), (1, 2, '{5}'), (1, 3, '{5,6}'), (1, 5, '{}'),
                                        (1, 6, '{}'), (1, 7, '{}')",
        // 5: a staff quarantine case; 6: a Raider List quarantine; 7: only an old, released case.
        "INSERT INTO cases VALUES (1, 5, 'quarantine', now()), (1, 7, 'quarantine', now() - interval '1 day')",
        "INSERT INTO joingate_actions VALUES (1, 6, 'Quarantine', true, now())",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.unwrap();
    }
    let up = include_str!("../../core/migrations/3900_quarantine_join.sql");
    sqlx::raw_sql(up).execute(&mut *tx).await.unwrap();
    let rows: Vec<(i64, bool)> =
        sqlx::query_as("SELECT user_id, on_join FROM quarantines ORDER BY user_id")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            (1, true),
            (2, false),
            (3, false),
            (5, false),
            (6, false),
            (7, true)
        ]
    );
    // New rows default to a real quarantine.
    sqlx::query("INSERT INTO quarantines VALUES (1, 4, '{}')")
        .execute(&mut *tx)
        .await
        .unwrap();
    let new: bool = sqlx::query_scalar("SELECT on_join FROM quarantines WHERE user_id = 4")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(!new);
    tx.rollback().await.unwrap();
}

/// D61: a member who left during verification's join quarantine and rejoins is verifying, not
/// held: their sticky roles wait for verification instead of being dropped.
#[tokio::test]
async fn a_join_quarantine_rejoin_keeps_sticky_roles_for_verification() {
    let Some(t) = T::new(160).await else { return };
    let mut v = web_verification();
    v["quarantine_on_join"] = json!(true);
    let cfg = json!({"antinuke": {"quarantine_role": Q.to_string()}, "verification": v,
                     "misc": {"sticky_roles": true}});
    server(&t, 0, cfg).await;
    let (g, u) = (t.g(0).get() as i64, t.base * 10 + 1);
    // Left while in the join quarantine; their roles from before are saved.
    for sql in [
        "INSERT INTO quarantines (guild_id, user_id, saved_roles, on_join) VALUES ($1, $2, '{}', true)",
        "INSERT INTO member_roles (guild_id, user_id, roles, joined_at, left_at)
         VALUES ($1, $2, '{501}', '2025-01-01T00:00:00Z', now())",
    ] {
        sqlx::query(sql)
            .bind(g)
            .bind(u as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
    }
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![]);
    });
    let m: MemberAdd = serde_json::from_value(json!({
        "guild_id": t.g(0).to_string(), "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
        "user": {"id": u.to_string(), "username": "back", "discriminator": "0", "avatar": null},
    }))
    .unwrap();
    crate::joingate::on_join(&t.app, &m, false).await.unwrap();
    assert_eq!(
        roles(&t, 0, u),
        Some(vec![Q]),
        "back in the join quarantine"
    );
    let r = crate::verify::web_grant(&t.app, t.g(0), Id::new(u), &[]).await;
    assert!(r.is_ok(), "{r:?}");
    let mut got = roles(&t, 0, u).unwrap();
    got.sort();
    assert_eq!(
        got,
        vec![ROLE, VERIFIED],
        "sticky roles came back after verifying"
    );
    sqlx::query("DELETE FROM member_roles WHERE guild_id = $1")
        .bind(g)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.cleanup().await;
}

/// D61: a join raid hit that keeps the member drops their saved sticky roles, as on main, so a
/// raider cannot get them back by verifying or rejoining; nothing is given back.
#[tokio::test]
async fn a_logged_raider_loses_saved_sticky_roles() {
    let Some(t) = T::new(84).await else { return };
    let raid = json!({"enabled": true, "count": 1, "span_ms": 60000, "suspicious_only": false,
                      "active_ms": 60000, "string_patterns": [], "flag_younger_than_ms": null,
                      "flag_no_avatar": true, "flag_spammer": false, "action": "Log"});
    let cfg = json!({"antinuke": {"quarantine_role": Q.to_string()}, "joinraid": raid,
                     "misc": {"sticky_roles": true}});
    server(&t, 0, cfg).await;
    let (g, u) = (t.g(0).get() as i64, t.base * 10 + 1);
    sqlx::query(
        "INSERT INTO member_roles (guild_id, user_id, roles, joined_at, left_at)
         VALUES ($1, $2, '{501}', '2025-01-01T00:00:00Z', now())",
    )
    .bind(g)
    .bind(u as i64)
    .execute(&t.app.pool)
    .await
    .unwrap();
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![]);
    });
    let m: MemberAdd = serde_json::from_value(json!({
        "guild_id": t.g(0).to_string(), "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
        "user": {"id": u.to_string(), "username": "raider", "discriminator": "0", "avatar": null},
    }))
    .unwrap();
    crate::joingate::on_join(&t.app, &m, false).await.unwrap();
    let flagged: i64 = sqlx::query_scalar("SELECT count(*) FROM raid_log WHERE guild_id = $1")
        .bind(g)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(flagged, 1, "the join raid flagged them");
    assert_eq!(roles(&t, 0, u), Some(vec![]), "nothing given back");
    let kept: (Vec<i64>, bool) = sqlx::query_as(
        "SELECT roles, pending FROM member_roles WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(g)
    .bind(u as i64)
    .fetch_one(&t.app.pool)
    .await
    .unwrap();
    assert_eq!(kept, (vec![], false), "the saved list is dropped");
    for sql in [
        "DELETE FROM member_roles WHERE guild_id = $1",
        "DELETE FROM raid_log WHERE guild_id = $1",
    ] {
        sqlx::query(sql).bind(g).execute(&t.app.pool).await.unwrap();
    }
    t.cleanup().await;
}

/// Web verification and staff /verify work during a timeout: the pending sticky roles come back,
/// but the main roles wait for the timeout's end when the timeout takes them.
#[tokio::test]
async fn verifying_during_a_timeout_does_not_give_back_main_roles() {
    let Some(t) = T::new(85).await else { return };
    let cfg = json!({"misc": {"sticky_roles": true, "remove_main_roles_on_timeout": true},
                     "lockdown": {"main_roles": [ROLE.to_string()]}});
    server(&t, 0, cfg).await;
    let (g, u) = (t.g(0).get() as i64, t.base * 10 + 1);
    sqlx::query(
        "INSERT INTO member_roles (guild_id, user_id, roles, joined_at, pending)
         VALUES ($1, $2, '{501,504}', '2025-01-01T00:00:00Z', true)",
    )
    .bind(g)
    .bind(u as i64)
    .execute(&t.app.pool)
    .await
    .unwrap();
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![VERIFIED]);
        m.timeouts
            .insert((t.g(0).get(), u), "2099-01-01T00:00:00.000000+00:00".into());
    });
    crate::sticky::restore_pending(&t.app, t.g(0), Id::new(u))
        .await
        .unwrap();
    let mut got = roles(&t, 0, u).unwrap();
    got.sort();
    assert_eq!(
        got,
        vec![VERIFIED, AUTOROLE],
        "the main role waits for the timeout"
    );
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM scheduled_jobs WHERE guild_id = $1 AND kind = $2")
            .bind(g)
            .bind(crate::timeout_roles::KIND)
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(queued, 1, "queued for the timeout's end");
    sqlx::query("DELETE FROM member_roles WHERE guild_id = $1")
        .bind(g)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.cleanup().await;
}

/// D25: `verify` ends only verification's join quarantine (other quarantines need the quarantine
/// permit), and saved roles with dangerous permissions (Administrator) need an owner or TA.
#[tokio::test]
async fn verify_lifts_only_a_join_quarantine_and_admin_roles_need_staff() {
    let Some(t) = T::new(163).await else { return };
    let cfg = json!({"antinuke": {"quarantine_role": Q.to_string()}, "verification": web_verification()});
    server(&t, 0, cfg).await;
    const ADMIN: u64 = 505;
    let (real, admin) = (t.base * 10 + 1, t.base * 10 + 2);
    t.set(|m| {
        let g = t.g(0).get();
        m.roles.get_mut(&g).unwrap().push((ADMIN, false, 8, 6));
        m.members.insert((g, real), vec![Q]);
        m.members.insert((g, admin), vec![Q]);
    });
    row(&t, 0, real, &[ROLE as i64]).await;
    row(&t, 0, admin, &[ADMIN as i64]).await;
    let permit = |perms: &[&str]| tommybot_core::permits::Level::Custom {
        rank: 1,
        perms: perms.iter().map(|p| p.to_string()).collect(),
        whitelist: vec![],
        limits: Default::default(),
    };
    let run = |level: tommybot_core::permits::Level, cmd| {
        let t = &t;
        async move {
            let me = (Id::new(9), &level);
            let owner = Id::new(t.owner());
            let r = crate::modcmds::run(&t.app, t.g(0), owner, me, Id::new(LOGS), cmd, (None, None), false);
            r.await.unwrap().text
        }
    };
    use crate::modcmds::ModCmd::{Unquarantine, Verify};
    let text = run(permit(&["verify"]), Verify { user: real }).await;
    assert!(text.starts_with("Cannot verify"), "{text}");
    assert_eq!(roles(&t, 0, real), Some(vec![Q]), "still quarantined");
    let text = run(permit(&["quarantine"]), Unquarantine { user: admin }).await;
    assert!(text.starts_with("Cannot release"), "{text}");
    assert_eq!(saved(&t, 0, admin).await, Some(vec![ADMIN as i64]));
    // During a role lock the Administrator role reads 0 live; the lock's saved value counts.
    const LOCKED: u64 = 506;
    let stripped = t.base * 10 + 3;
    t.set(|m| {
        let g = t.g(0).get();
        m.roles.get_mut(&g).unwrap().push((LOCKED, false, 0, 7));
        m.members.insert((g, stripped), vec![Q]);
    });
    t.app.invalidate_guild(t.g(0));
    row(&t, 0, stripped, &[LOCKED as i64]).await;
    let lock = tommybot_automod::lockdown::Snapshot {
        kind: Some(tommybot_automod::lockdown::LockKind::Role),
        role_perms: vec![(LOCKED, 8)],
        ..Default::default()
    };
    tommybot_automod::store::save_lock(&t.app.pool, t.g(0).get(), &lock)
        .await
        .unwrap();
    let text = run(permit(&["quarantine"]), Unquarantine { user: stripped }).await;
    sqlx::query("DELETE FROM lock_snapshots WHERE guild_id = $1")
        .bind(t.g(0).get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    assert!(text.starts_with("Cannot release"), "role lock: {text}");
    // With the quarantine permit, verify may end it.
    let text = run(permit(&["verify", "quarantine"]), Verify { user: real }).await;
    assert_eq!(saved(&t, 0, real).await, None, "{text}");
    t.cleanup().await;
}

/// An audit entry: `actor` added `role` to `target`.
fn role_added(t: &T, actor: u64, target: u64, role: u64) -> AuditLogEntry {
    serde_json::from_value(json!({
        "id": "2", "action_type": 25, "guild_id": t.g(0).to_string(), "target_id": target.to_string(),
        "user_id": actor.to_string(), "reason": null,
        "changes": [{"key": "$add", "new_value": [{"id": role.to_string(), "name": "r"}]}],
    }))
    .unwrap()
}

fn joined(t: &T, u: u64) -> MemberAdd {
    serde_json::from_value(json!({
        "guild_id": t.g(0).to_string(), "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
        "user": {"id": u.to_string(), "username": "back", "discriminator": "0", "avatar": null},
    }))
    .unwrap()
}

/// D15: a quarantined member rejoins and an autorole bot gives them a role: the role goes, the
/// bot is not quarantined. A human doing the same still is.
#[tokio::test]
async fn an_autorole_bot_after_a_rejoin_quarantine_is_undone_not_punished() {
    let Some(t) = T::new(165).await else { return };
    let an = json!({"enabled": true, "quarantine_role": Q.to_string()});
    server(&t, 0, json!({ "antinuke": an })).await;
    let (u, bot, human) = (t.base * 10 + 1, t.base * 10 + 2, t.base * 10 + 3);
    row(&t, 0, u, &[ROLE as i64]).await;
    t.set(|m| {
        m.bots.push(bot);
        m.members.insert((t.g(0).get(), u), vec![]);
    });
    crate::joingate::on_join(&t.app, &joined(&t, u), false)
        .await
        .unwrap();
    assert_eq!(roles(&t, 0, u), Some(vec![Q]), "quarantined again");
    for actor in [bot, human] {
        t.set(|m| {
            m.members.insert((t.g(0).get(), u), vec![Q, AUTOROLE]);
            m.members.insert((t.g(0).get(), actor), vec![ROLE]);
        });
        let e = role_added(&t, actor, u, AUTOROLE);
        crate::antinuke::on_audit(&t.app, &e).await.unwrap();
        assert_eq!(roles(&t, 0, u), Some(vec![Q]), "the role is taken back");
    }
    assert_eq!(saved(&t, 0, bot).await, None, "the autorole bot was punished");
    assert!(saved(&t, 0, human).await.is_some(), "a human is still held to the rule");
    t.cleanup().await;
}

/// D15 review: the grace covers any number of bot changes in the window, but only plain roles.
/// An Administrator role or a permit role quarantines the bot as before.
#[tokio::test]
async fn the_rejoin_grace_covers_plain_roles_only() {
    let Some(t) = T::new(162).await else { return };
    let an = json!({"enabled": true, "quarantine_role": Q.to_string()});
    server(&t, 0, json!({ "antinuke": an })).await;
    const ADMIN: u64 = 507;
    const PERMIT: u64 = 511;
    let [u, v, w, admin_bot, auto_bot, level_bot, permit_bot, unknown_bot] =
        [1, 2, 3, 4, 5, 6, 7, 8].map(|n| t.base * 10 + n);
    let bots = [admin_bot, auto_bot, level_bot, permit_bot, unknown_bot];
    t.set(|m| {
        let g = t.g(0).get();
        m.roles.get_mut(&g).unwrap().extend([(ADMIN, false, 8, 6), (PERMIT, false, 0, 7)]);
        m.bots.extend(bots);
        for x in [u, v, w].into_iter().chain(bots) {
            m.members.insert((g, x), vec![]);
        }
    });
    t.exec("INSERT INTO permits (id, guild_id, name, perms) VALUES ($1 + 1, $1, 'Helpers', '{warn}')")
        .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1 + 1, $1, 511)")
        .await;
    for x in [u, v, w] {
        row(&t, 0, x, &[ROLE as i64]).await;
        crate::joingate::on_join(&t.app, &joined(&t, x), false)
            .await
            .unwrap();
    }
    let give = |actor: u64, target: u64, role: u64| {
        let t = &t;
        async move {
            t.set(|m| {
                m.members.insert((t.g(0).get(), target), vec![Q, role]);
            });
            let e = role_added(t, actor, target, role);
            crate::antinuke::on_audit(&t.app, &e).await.unwrap();
        }
    };
    give(admin_bot, u, ADMIN).await;
    assert!(saved(&t, 0, admin_bot).await.is_some(), "Administrator is no autorole");
    give(auto_bot, v, AUTOROLE).await;
    give(level_bot, v, AUTOROLE).await;
    give(auto_bot, v, AUTOROLE).await;
    for b in [auto_bot, level_bot] {
        assert_eq!(saved(&t, 0, b).await, None, "plain roles are graced, however many");
    }
    give(permit_bot, w, PERMIT).await;
    assert!(saved(&t, 0, permit_bot).await.is_some(), "a permit role is no autorole");
    // A role Discord does not list (created a moment ago, or the read failed) is not plain.
    give(unknown_bot, v, 599).await;
    assert!(saved(&t, 0, unknown_bot).await.is_some(), "an unknown role is no autorole");
    t.cleanup().await;
}

/// D12b: an undo Discord refuses on a join-quarantined member punishes nobody, is tried once
/// more and then shown to staff.
#[tokio::test]
async fn a_failed_undo_on_a_join_quarantine_is_retried_and_logged() {
    let Some(t) = T::new(166).await else { return };
    let an = json!({"enabled": true, "quarantine_role": Q.to_string()});
    server(&t, 0, json!({ "antinuke": an })).await;
    let (u, actor) = (t.base * 10 + 1, t.base * 10 + 2);
    sqlx::query("INSERT INTO quarantines (guild_id, user_id, saved_roles, on_join) VALUES ($1, $2, '{}', true)")
        .bind(t.g(0).get() as i64)
        .bind(u as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.set(|m| {
        m.fail_member_edit = true;
        m.members.insert((t.g(0).get(), u), vec![Q, AUTOROLE]);
        m.members.insert((t.g(0).get(), actor), vec![]);
    });
    let e = role_added(&t, actor, u, AUTOROLE);
    crate::antinuke::on_audit(&t.app, &e).await.unwrap();
    let patch = format!("PATCH /guilds/{}/members/{u}", t.g(0));
    assert_eq!(t.calls(&patch), 2, "tried twice");
    assert_eq!(t.calls("Could not take back roles"), 1, "staff were told");
    assert_eq!(saved(&t, 0, actor).await, None, "nobody is punished");
    t.cleanup().await;
}

/// D17: at startup a member with a quarantine row who joined after their last quarantine (a rejoin
/// while the bot was down) is quarantined again; one in the same membership is left as staff left
/// them, and one who is gone is skipped.
#[tokio::test]
async fn a_rejoin_while_the_bot_was_down_is_quarantined_at_startup() {
    let Some(t) = T::new(167).await else { return };
    server(&t, 0, json!({"antinuke": {"quarantine_role": Q.to_string()}})).await;
    let [back, freed, held_live, gone] = [1, 2, 3, 4].map(|n| t.base * 10 + n);
    for u in [back, freed, held_live, gone] {
        row(&t, 0, u, &[ROLE as i64]).await;
    }
    // The mock's members joined in 2020: before these rows, after a 2019 quarantine.
    let g = t.g(0).get() as i64;
    sqlx::query("UPDATE quarantines SET created_at = '2019-01-01' WHERE guild_id = $1 AND user_id <> $2")
        .bind(g)
        .bind(freed as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    // Quarantined again live on its rejoin: the bot's own record is newer than the join.
    sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id) VALUES ($1, $2, 'quarantine', $3)")
        .bind(g)
        .bind(ME as i64)
        .bind(held_live as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.set(|m| {
        for u in [back, freed, held_live] {
            m.members.insert((t.g(0).get(), u), vec![ROLE]);
        }
    });
    crate::joingate::requarantine_missed(&t.app, t.g(0), &[], false, None)
        .await
        .unwrap();
    assert_eq!(roles(&t, 0, back), Some(vec![Q]), "rejoined while down");
    assert_eq!(roles(&t, 0, freed), Some(vec![ROLE]), "released by hand");
    assert_eq!(roles(&t, 0, held_live), Some(vec![ROLE]), "same membership");
    assert_eq!(saved(&t, 0, back).await, Some(vec![ROLE as i64]), "saved list kept");
    // A second run (a restart's job next to GuildCreate) quarantines and logs nobody again.
    let logged = t.calls("Rejoined the server while quarantined");
    let edits = t.calls(&format!("PATCH /guilds/{}/members/{back}", t.g(0)));
    crate::joingate::requarantine_missed(&t.app, t.g(0), &[], false, None)
        .await
        .unwrap();
    assert_eq!(t.calls("Rejoined the server while quarantined"), logged);
    assert_eq!(t.calls(&format!("PATCH /guilds/{}/members/{back}", t.g(0))), edits);
    // During a raid the check waits in one job instead of reading members, at most 30 minutes.
    let raid = tommybot_automod::join::RaidDetector::restored(0, i64::MAX, String::new());
    t.app.raids.lock().unwrap().insert(t.g(0).get(), raid);
    let reads = t.calls(&format!("GET /guilds/{}/members", t.g(0)));
    for _ in 0..2 {
        crate::joingate::requarantine_missed(&t.app, t.g(0), &[], false, None)
            .await
            .unwrap();
    }
    assert_eq!(t.calls(&format!("GET /guilds/{}/members", t.g(0))), reads);
    // GuildCreate (no `first`) takes the first wait from the pending job, so the cap holds.
    sqlx::query("UPDATE scheduled_jobs SET payload = jsonb_build_object('first', $2::bigint) WHERE guild_id = $1 AND kind = 'requarantine_missed'")
        .bind(t.g(0).get() as i64)
        .bind(crate::app::now_secs() - 1801)
        .execute(&t.app.pool)
        .await
        .unwrap();
    crate::joingate::requarantine_missed(&t.app, t.g(0), &[], false, None)
        .await
        .unwrap();
    t.app.raids.lock().unwrap().clear();
    assert!(t.calls(&format!("GET /guilds/{}/members", t.g(0))) > reads, "ran anyway");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'requarantine_missed'")
        .bind(t.g(0).get() as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    t.cleanup().await;
}

/// D58: with TommyBot's verification on, a verification role another bot gives an unverified
/// member is taken back; nobody is punished, and a human giving it is left alone. With
/// "quarantine on join" the hold already did this (#170); this covers verification without it.
#[tokio::test]
async fn a_verification_role_from_another_bot_is_taken_back() {
    let Some(t) = T::new(168).await else { return };
    server(&t, 0, json!({"verification": web_verification()})).await;
    const MOD: u64 = 516;
    let [u, bot, human, staff] = [1, 2, 3, 4].map(|n| t.base * 10 + n);
    t.set(|m| {
        m.bots.push(bot);
        m.roles.get_mut(&t.g(0).get()).unwrap().push((MOD, false, 0, 6));
        m.members.insert((t.g(0).get(), human), vec![]);
        m.members.insert((t.g(0).get(), staff), vec![MOD]);
    });
    t.exec("INSERT INTO permits (id, guild_id, name, perms) VALUES ($1 + 1, $1, 'Mods', '{verify}')")
        .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1 + 1, $1, 516)")
        .await;
    // Another bot, and a human with only Discord's Manage Roles, lose it; a verify permit or the
    // owner keeps it (Thomas, 13:53).
    let owner = t.owner();
    for (actor, kept) in [(bot, vec![]), (human, vec![]), (staff, vec![VERIFIED]), (owner, vec![VERIFIED])] {
        t.set(|m| {
            m.members.insert((t.g(0).get(), u), vec![VERIFIED]);
        });
        let e = role_added(&t, actor, u, VERIFIED);
        crate::verify::given::on_audit(&t.app, &e).await.unwrap();
        assert_eq!(roles(&t, 0, u), Some(kept), "actor {actor}");
    }
    // The permit lookup fails (here the server read): the rule fails closed and staff are told.
    t.set(|m| {
        m.owners.remove(&t.g(0).get());
        m.members.insert((t.g(0).get(), u), vec![VERIFIED]);
    });
    t.app.invalidate_guild(t.g(0));
    crate::verify::given::on_audit(&t.app, &role_added(&t, staff, u, VERIFIED)).await.unwrap();
    assert_eq!(roles(&t, 0, u), Some(vec![]));
    assert_eq!(t.calls("could not be read"), 1);
    for x in [bot, human] {
        assert_eq!(saved(&t, 0, x).await, None, "nobody is punished");
    }
    assert_eq!(t.calls("given by another bot was taken back"), 1);
    assert_eq!(t.calls("given without a verify permit, was taken back"), 2);
    t.cleanup().await;
}

/// D27: an automatic quarantine queues one DM telling the member how to reach staff, while tickets
/// are on; none during a raid, none for a bot, and the job skips a join quarantine.
#[tokio::test]
async fn an_automatic_quarantine_tells_the_member_how_to_reach_staff() {
    let Some(t) = T::new(169).await else { return };
    let cfg = json!({"antinuke": {"quarantine_role": Q.to_string()}, "tickets": {"enabled": true}});
    server(&t, 0, cfg).await;
    let [member, bot, raider] = [1, 2, 3].map(|n| t.base * 10 + n);
    t.set(|m| {
        m.bots.push(bot);
        for u in [member, bot, raider] {
            m.members.insert((t.g(0).get(), u), vec![ROLE]);
        }
    });
    let jobs = || async {
        sqlx::query_as::<_, (Value,)>(
            "SELECT payload FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'quarantine_dm' ORDER BY id",
        )
        .bind(t.g(0).get() as i64)
        .fetch_all(&t.app.pool)
        .await
        .unwrap()
    };
    let why = "command rate limit exceeded";
    for u in [member, bot] {
        crate::antinuke::quarantine_actor(&t.app, t.g(0), Id::new(u), why)
            .await
            .unwrap();
    }
    // A raid wave queues no DMs.
    let raid = tommybot_automod::join::RaidDetector::restored(0, i64::MAX, String::new());
    t.app.raids.lock().unwrap().insert(t.g(0).get(), raid);
    crate::antinuke::quarantine_actor(&t.app, t.g(0), Id::new(raider), why)
        .await
        .unwrap();
    t.app.raids.lock().unwrap().clear();
    let queued = jobs().await;
    assert_eq!(queued.len(), 2, "{queued:?}");
    for (p,) in &queued {
        crate::reply::quarantine_dm_job(&t.app, t.g(0), p).await.unwrap();
    }
    assert_eq!(t.calls("You can DM TommyBot to reach this server"), 1, "the member, not the bot");
    // A second quarantine within 24 h queues no second DM.
    crate::antinuke::quarantine_actor(&t.app, t.g(0), Id::new(member), why)
        .await
        .unwrap();
    assert_eq!(jobs().await.len(), 2, "one DM a day");
    // A join quarantine gets the verification prompt instead.
    sqlx::query("UPDATE quarantines SET on_join = true WHERE guild_id = $1 AND user_id = $2")
        .bind(t.g(0).get() as i64)
        .bind(member as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    crate::reply::quarantine_dm_job(&t.app, t.g(0), &json!({"user": member}))
        .await
        .unwrap();
    assert_eq!(t.calls("You can DM TommyBot to reach this server"), 1);
    t.cleanup().await;
}

/// D58 review: a role opening the verified channels counts as a verification role; two grants in
/// parallel are both taken back; a removal Discord refuses is tried again and then told to staff;
/// and a Quarantine Whitelist bot's grant to a join-quarantined member is taken back too.
#[tokio::test]
async fn other_bot_grants_that_skip_verification_are_all_taken_back() {
    let Some(t) = T::new(164).await else { return };
    const EXTRA: u64 = 508;
    const VERIFIED2: u64 = 509;
    const STUCK: u64 = 510;
    let mut v = web_verification();
    v["roles"] = json!([VERIFIED.to_string(), VERIFIED2.to_string(), STUCK.to_string()]);
    let [u, w, x, y, bot] = [1, 2, 3, 4, 5].map(|n| t.base * 10 + n);
    let cfg = json!({"verification": v, "antinuke": {"enabled": true, "quarantine_role": Q.to_string(),
                     "quarantine_whitelist": [bot.to_string()]}});
    server(&t, 0, cfg).await;
    t.set(|m| {
        let g = t.g(0).get();
        let r = m.roles.get_mut(&g).unwrap();
        r.extend([(EXTRA, false, 0, 6), (VERIFIED2, false, 0, 7), (STUCK, true, 0, 8)]);
        m.bots.push(bot);
        let ow = |role: u64| json!({"id": role.to_string(), "type": 0, "allow": "1024", "deny": "0"});
        m.channels.insert(g, json!([{"id": "701", "type": 0, "guild_id": g.to_string(), "name": "general",
            "position": 0, "permission_overwrites": [ow(VERIFIED), ow(EXTRA)]}]));
        m.members.insert((g, u), vec![EXTRA]);
        m.members.insert((g, w), vec![VERIFIED, VERIFIED2]);
        m.members.insert((g, x), vec![STUCK]);
        m.members.insert((g, y), vec![Q, VERIFIED]);
    });
    let give = |target: u64, role: u64| role_added(&t, bot, target, role);
    // A plain role that opens the verified channel.
    crate::verify::given::on_audit(&t.app, &give(u, EXTRA)).await.unwrap();
    assert_eq!(roles(&t, 0, u), Some(vec![]));
    // Two verification roles in two calls at once: neither proves the other.
    let (a, b) = (give(w, VERIFIED), give(w, VERIFIED2));
    let (ra, rb) = tokio::join!(
        crate::verify::given::on_audit(&t.app, &a),
        crate::verify::given::on_audit(&t.app, &b)
    );
    ra.unwrap();
    rb.unwrap();
    assert_eq!(roles(&t, 0, w), Some(vec![]));
    // Discord refuses the removal (a managed role): tried twice, then staff hear of it.
    crate::verify::given::on_audit(&t.app, &give(x, STUCK)).await.unwrap();
    let del = format!("DELETE /guilds/{}/members/{x}/roles/{STUCK}", t.g(0));
    assert_eq!(t.calls(&del), 2);
    assert_eq!(t.calls("Could not take back a verification role"), 1);
    // The hold lets a Quarantine Whitelist bot through; this check does not.
    row(&t, 0, y, &[]).await;
    crate::verify::given::on_audit(&t.app, &give(y, VERIFIED)).await.unwrap();
    assert_eq!(roles(&t, 0, y), Some(vec![Q]));
    t.cleanup().await;
}

/// D58 round 3: a role opening the verified channels by base permission (the usual setup with
/// no overwrites) or Send Messages where @everyone only reads is taken back, and so is a bot's
/// member overwrite; a role opening nothing costs no member read. Grants made while TommyBot was
/// down are found at GuildCreate. With Quarantine Hold off, a held member is judged here.
#[tokio::test]
async fn bot_access_that_skips_verification_is_taken_back_by_base_overwrite_and_at_startup() {
    let Some(t) = T::new(174).await else { return };
    t.exec("INSERT INTO permits (id, guild_id, name, perms) VALUES ($1 + 3, $1 + 2, 'Mods', '{verify}')")
        .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1 + 3, $1 + 2, 516)")
        .await;
    const MEMBER: u64 = 512;
    const PINGS: u64 = 513;
    const TALK: u64 = 514;
    const SENDS: u64 = 515;
    let (view, send) = (1024u64, 2048u64);
    let [u, p, vfd, s1, s2, s3, a, b, c, z, bot, human, s4, d, e, nobody, s5] =
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17].map(|n| t.base * 100 + n);
    let an = json!({"enabled": true, "quarantine_role": Q.to_string(), "quarantine_hold": false});
    for i in 0..3 {
        server(&t, i, json!({"verification": web_verification(), "antinuke": an})).await;
    }
    let chan = |g: u64, id: &str, ows: Vec<Value>| {
        json!([{"id": id, "type": 0, "guild_id": g.to_string(), "name": "c", "position": 0,
                "permission_overwrites": ows}])
    };
    let ow = |id: u64, allow: u64, deny: u64| {
        json!({"id": id.to_string(), "type": 0, "allow": allow.to_string(), "deny": deny.to_string()})
    };
    t.set(|m| {
        m.bots.push(bot);
        // Server 0: @everyone has no View; the verified role has it in its base permissions.
        let g = t.g(0).get();
        let r = m.roles.get_mut(&g).unwrap();
        r.retain(|x| x.0 != VERIFIED);
        r.extend([(VERIFIED, false, view, 4), (MEMBER, false, view, 6), (PINGS, false, 0, 7)]);
        m.channels.insert(g, chan(g, "711", vec![]));
        for x in [u, p] {
            m.members.insert((g, x), vec![]);
        }
        m.members.insert((g, vfd), vec![VERIFIED]);
        // Server 1: @everyone reads but cannot write; verified members write.
        let g = t.g(1).get();
        let r = m.roles.get_mut(&g).unwrap();
        r.retain(|x| x.0 != g);
        r.extend([(g, false, view | send, 0), (TALK, false, 0, 6), (SENDS, false, send, 7)]);
        m.channels.insert(g, chan(g, "712", vec![ow(g, 0, send), ow(VERIFIED, send, 0), ow(TALK, send, 0)]));
        for x in [s1, s2, s3] {
            m.members.insert((g, x), vec![]);
        }
        // Server 2: grants made while TommyBot was down, newest first.
        let g = t.g(2).get();
        for x in [a, b, c] {
            m.members.insert((g, x), vec![VERIFIED]);
        }
        m.members.insert((g, z), vec![Q, VERIFIED]);
        let add = |id: u64, actor: u64, target: u64| {
            json!({"id": id.to_string(), "action_type": 25, "guild_id": g.to_string(),
                   "target_id": target.to_string(), "user_id": actor.to_string(), "reason": null,
                   "changes": [{"key": "$add", "new_value": [{"id": VERIFIED.to_string(), "name": "v"}]}]})
        };
        let mut taken = add(10, ME, d);
        taken["changes"][0]["key"] = json!("$remove");
        m.members.insert((g, d), vec![VERIFIED]);
        // `human` holds a verify permit, `nobody` only Discord's Manage Roles.
        m.roles.get_mut(&g).unwrap().push((516, false, 0, 6));
        m.members.insert((g, human), vec![516]);
        m.members.insert((g, nobody), vec![]);
        m.members.insert((g, e), vec![VERIFIED]);
        m.audit.insert(g, vec![taken, add(11, nobody, e), add(9, ME, c), add(8, bot, a), add(7, human, b), add(6, bot, c), add(5, bot, d)]);
    });
    let give = |i: u64, target: u64, role: u64| {
        let mut e = role_added(&t, bot, target, role);
        e.guild_id = Some(t.g(i));
        e
    };
    let given = |e: AuditLogEntry| {
        let t = &t;
        async move { crate::verify::given::on_audit(&t.app, &e).await.unwrap() }
    };
    let reads = |i: u64, x: u64| t.calls(&format!("GET /guilds/{}/members/{x} ", t.g(i)));
    // Base View: an ordinary "Member" autorole verifies nobody.
    t.set(|m| {
        m.members.insert((t.g(0).get(), u), vec![MEMBER]);
    });
    given(give(0, u, MEMBER)).await;
    assert_eq!(roles(&t, 0, u), Some(vec![]));
    // A role that opens nothing: no member read at all.
    t.set(|m| {
        m.members.insert((t.g(0).get(), p), vec![PINGS]);
    });
    given(give(0, p, PINGS)).await;
    assert_eq!((roles(&t, 0, p), reads(0, p)), (Some(vec![PINGS]), 0));
    // A member verified long ago is read once, then remembered.
    for _ in 0..2 {
        t.set(|m| {
        m.members.insert((t.g(0).get(), vfd), vec![VERIFIED, MEMBER]);
    });
        given(give(0, vfd, MEMBER)).await;
    }
    assert_eq!((roles(&t, 0, vfd), reads(0, vfd)), (Some(vec![VERIFIED, MEMBER]), 1));
    // Staff un-verify them: they are no longer remembered as verified.
    let mut unverify = role_added(&t, human, vfd, VERIFIED);
    unverify.changes = serde_json::from_value(json!([{"key": "$remove", "new_value": [{"id": VERIFIED.to_string(), "name": "v"}]}])).unwrap();
    given(unverify).await;
    t.set(|m| {
        m.members.insert((t.g(0).get(), vfd), vec![MEMBER]);
    });
    given(give(0, vfd, MEMBER)).await;
    assert_eq!(roles(&t, 0, vfd), Some(vec![]), "checked again after the un-verify");
    // A human without a permit: a role opening the channels goes from an unverified member and
    // stays with a verified one.
    t.set(|m| {
        m.members.insert((t.g(0).get(), human), vec![]);
        m.members.insert((t.g(0).get(), u), vec![MEMBER]);
        m.members.insert((t.g(0).get(), vfd), vec![VERIFIED, MEMBER]);
    });
    let by_human = |target: u64| {
        let mut e = role_added(&t, human, target, MEMBER);
        e.guild_id = Some(t.g(0));
        e
    };
    given(by_human(u)).await;
    given(by_human(vfd)).await;
    assert_eq!((roles(&t, 0, u), roles(&t, 0, vfd)), (Some(vec![]), Some(vec![VERIFIED, MEMBER])));
    // TommyBot's own slowmode keeps the gate; its own lock edits only shorten its life.
    let own = |kind: u64| -> AuditLogEntry {
        serde_json::from_value(json!({"id": "5", "action_type": kind, "guild_id": t.g(0).to_string(),
            "target_id": "711", "user_id": ME.to_string(), "reason": null, "changes": []})).unwrap()
    };
    let before = crate::verify::given::cached_for(t.g(0));
    given(own(11)).await;
    assert_eq!(crate::verify::given::cached_for(t.g(0)), before, "slowmode");
    given(own(14)).await;
    assert!(crate::verify::given::cached_for(t.g(0)).is_some_and(|d| d < std::time::Duration::from_secs(60)), "lock edit");
    // A role edited to open the verified channels counts at once: the edit drops the cached gate.
    t.set(|m| {
        let g = t.g(0).get();
        m.roles.get_mut(&g).unwrap().iter_mut().filter(|r| r.0 == PINGS).for_each(|r| r.2 = view);
    });
    let edit: AuditLogEntry = serde_json::from_value(json!({
        "id": "4", "action_type": 31, "guild_id": t.g(0).to_string(), "target_id": PINGS.to_string(),
        "user_id": human.to_string(), "reason": null, "changes": [],
    }))
    .unwrap();
    given(edit).await;
    given(give(0, p, PINGS)).await;
    assert_eq!(roles(&t, 0, p), Some(vec![]), "the edited role is taken back");
    // Send only: a role allowed to write there goes, one whose base Send @everyone's deny blocks stays.
    t.set(|m| {
        m.members.insert((t.g(1).get(), s1), vec![TALK]);
        m.members.insert((t.g(1).get(), s2), vec![SENDS]);
    });
    given(give(1, s1, TALK)).await;
    given(give(1, s2, SENDS)).await;
    assert_eq!((roles(&t, 1, s1), roles(&t, 1, s2)), (Some(vec![]), Some(vec![SENDS])));
    // A bot's member overwrite opening that channel is removed.
    let over: AuditLogEntry = serde_json::from_value(json!({
        "id": "3", "action_type": 13, "guild_id": t.g(1).to_string(), "target_id": "712",
        "user_id": bot.to_string(), "reason": null, "options": {"id": s3.to_string(), "type": "1"},
        "changes": [{"key": "allow", "new_value": send.to_string()}],
    }))
    .unwrap();
    given(over).await;
    assert_eq!(t.calls(&format!("DELETE /channels/712/permissions/{s3}")), 1);
    // A verification role and an overwrite at once: the bot's own role proves nothing, both go,
    // and the member is not remembered as verified.
    t.set(|m| {
        m.members.insert((t.g(1).get(), s4), vec![VERIFIED]);
    });
    let mut over4 = serde_json::to_value(role_added(&t, bot, s4, VERIFIED)).unwrap();
    over4["action_type"] = json!(13);
    over4["guild_id"] = json!(t.g(1).to_string());
    over4["target_id"] = json!("712");
    over4["options"] = json!({"id": s4.to_string(), "type": "1"});
    over4["changes"] = json!([{"key": "allow", "new_value": send.to_string()}]);
    let over4: AuditLogEntry = serde_json::from_value(over4).unwrap();
    // The overwrite handler goes first, while the bot's role is still on the member.
    tokio::join!(given(over4), async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        given(give(1, s4, VERIFIED)).await
    });
    assert_eq!(roles(&t, 1, s4), Some(vec![]));
    assert_eq!(t.calls(&format!("DELETE /channels/712/permissions/{s4}")), 1);
    t.set(|m| {
        m.members.insert((t.g(1).get(), s4), vec![VERIFIED]);
    });
    given(give(1, s4, VERIFIED)).await;
    assert_eq!(roles(&t, 1, s4), Some(vec![]), "a later grant is still checked");
    // A channel lock also denies Send to the verified role: the gate judges the saved overwrite,
    // so the Send-only channel stays gated while the lock lasts.
    let g1 = t.g(1).get();
    t.set(|m| {
        m.channels.insert(g1, chan(g1, "712", vec![ow(g1, 0, send), ow(VERIFIED, 0, send), ow(TALK, send, 0)]));
        m.members.insert((g1, s5), vec![TALK]);
    });
    let lock = tommybot_automod::lockdown::Snapshot {
        kind: Some(tommybot_automod::lockdown::LockKind::Channel(712)),
        channel_denies: vec![(712, VERIFIED)],
        prior: vec![(712, VERIFIED, send, 0)],
        ..Default::default()
    };
    tommybot_automod::store::save_lock(&t.app.pool, g1, &lock).await.unwrap();
    // The lock's gateway events: TommyBot marked them as its own, so they only soften.
    crate::verify::given::mark_own(t.g(1));
    t.app.invalidate_guild(t.g(1));
    assert!(crate::verify::given::cached_for(t.g(1)).is_some_and(|d| d < std::time::Duration::from_secs(60)));
    // Anyone else's gateway edit drops the gate at once (S R5-1).
    std::thread::sleep(std::time::Duration::from_millis(5100));
    t.app.invalidate_guild(t.g(1));
    assert_eq!(crate::verify::given::cached_for(t.g(1)), None);
    given(give(1, s5, TALK)).await;
    sqlx::query("DELETE FROM lock_snapshots WHERE guild_id = $1")
        .bind(g1 as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(roles(&t, 1, s5), Some(vec![]), "Send gate held during the lock");
    // At GuildCreate: only a role whose newest grant came from another bot goes.
    // During a raid the catch-up waits in a job and reads nothing.
    let raid = tommybot_automod::join::RaidDetector::restored(0, i64::MAX, String::new());
    t.app.raids.lock().unwrap().insert(t.g(2).get(), raid);
    crate::verify::given::catch_up(&t.app, t.g(2), None).await.unwrap();
    t.app.raids.lock().unwrap().clear();
    assert_eq!(t.calls(&format!("GET /guilds/{}/audit-logs", t.g(2))), 0);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'verify_catch_up'")
        .bind(t.g(2).get() as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    crate::verify::given::catch_up(&t.app, t.g(2), None).await.unwrap();
    assert_eq!(reads(2, d), 0, "TommyBot took it back last: no member read");
    // A member read that fails is retried by a job.
    t.exec("DELETE FROM scheduled_jobs WHERE guild_id = $1 + 2").await;
    t.set(|m| m.fail_member = true);
    crate::verify::given::catch_up(&t.app, t.g(2), None).await.unwrap();
    t.set(|m| m.fail_member = false);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'verify_catch_up'")
        .bind(t.g(2).get() as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "a skipped member is checked again");
    // A lasting failure stops after 5 tries and tells staff.
    t.exec("UPDATE scheduled_jobs SET payload = '{\"tries\": 4}' WHERE guild_id = $1 + 2").await;
    t.set(|m| m.fail_member = true);
    crate::verify::given::catch_up(&t.app, t.g(2), None).await.unwrap();
    t.set(|m| m.fail_member = false);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'verify_catch_up'")
        .bind(t.g(2).get() as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "gave up");
    assert_eq!(t.calls("Gave up after 5 failed tries"), 1);
    assert_eq!(roles(&t, 2, a), Some(vec![]), "a bot verified them while TommyBot was down");
    assert_eq!(roles(&t, 2, b), Some(vec![VERIFIED]), "staff with a verify permit gave it");
    assert_eq!(roles(&t, 2, e), Some(vec![]), "a human without a permit gave it");
    assert_eq!(roles(&t, 2, c), Some(vec![VERIFIED]), "TommyBot gave it last");
    // Quarantine Hold off: the hold leaves a held member's role, so this check takes it.
    row(&t, 2, z, &[]).await;
    given(give(2, z, VERIFIED)).await;
    assert_eq!(roles(&t, 2, z), Some(vec![Q]));
    assert_eq!(saved(&t, 2, bot).await, None, "nobody is punished");
    t.cleanup().await;
}

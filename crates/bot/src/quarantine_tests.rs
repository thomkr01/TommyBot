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
                      "join_gate": LOGS.to_string(), "mod_command": LOGS.to_string()}})
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
    let Some(t) = T::new(70).await else { return };
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
    let Some(t) = T::new(71).await else { return };
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
    let Some(t) = T::new(72).await else { return };
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
    let Some(t) = T::new(73).await else { return };
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
    let Some(t) = T::new(74).await else { return };
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
    let Some(t) = T::new(75).await else { return };
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
    let Some(t) = T::new(76).await else { return };
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
    let Some(t) = T::new(77).await else { return };
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
    let Some(t) = T::new(78).await else { return };
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
    let Some(t) = T::new(81).await else { return };
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
    let Some(t) = T::new(79).await else { return };
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
    let Some(t) = T::new(80).await else { return };
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
    let Some(t) = T::new(42).await else { return };
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
    let Some(t) = T::new(43).await else { return };
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

/// D12b: an undo Discord refuses on a join-quarantined member punishes nobody, is tried once
/// more and then shown to staff.
#[tokio::test]
async fn a_failed_undo_on_a_join_quarantine_is_retried_and_logged() {
    let Some(t) = T::new(44).await else { return };
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
    let Some(t) = T::new(45).await else { return };
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
    crate::joingate::requarantine_missed(&t.app, t.g(0), &[], false)
        .await
        .unwrap();
    assert_eq!(roles(&t, 0, back), Some(vec![Q]), "rejoined while down");
    assert_eq!(roles(&t, 0, freed), Some(vec![ROLE]), "released by hand");
    assert_eq!(roles(&t, 0, held_live), Some(vec![ROLE]), "same membership");
    assert_eq!(saved(&t, 0, back).await, Some(vec![ROLE as i64]), "saved list kept");
    t.cleanup().await;
}

/// D58: with TommyBot's verification on, a verification role another bot gives an unverified
/// member is taken back; nobody is punished, and a human giving it is left alone. With
/// "quarantine on join" the hold already did this (#170); this covers verification without it.
#[tokio::test]
async fn a_verification_role_from_another_bot_is_taken_back() {
    let Some(t) = T::new(46).await else { return };
    server(&t, 0, json!({"verification": web_verification()})).await;
    let (u, bot, human) = (t.base * 10 + 1, t.base * 10 + 2, t.base * 10 + 3);
    t.set(|m| m.bots.push(bot));
    for (actor, kept) in [(bot, vec![]), (human, vec![VERIFIED])] {
        t.set(|m| {
            m.members.insert((t.g(0).get(), u), vec![VERIFIED]);
        });
        let e = role_added(&t, actor, u, VERIFIED);
        crate::verify::given::on_audit(&t.app, &e).await.unwrap();
        assert_eq!(roles(&t, 0, u), Some(kept), "actor {actor}");
    }
    assert_eq!(saved(&t, 0, bot).await, None, "nobody is punished");
    assert_eq!(t.calls("given by another bot was taken back"), 1);
    t.cleanup().await;
}

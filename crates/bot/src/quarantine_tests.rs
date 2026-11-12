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

/// D55: enforce_quarantine leaves the verification channel open to the join quarantine, also one
/// the dashboard made and has not saved yet (its exact overwrite marks it); other channels still
/// get the deny.
#[tokio::test]
async fn enforce_quarantine_leaves_the_verification_channel_alone() {
    let Some(t) = T::new(78).await else { return };
    let q = json!({"quarantine_role": Q.to_string()});
    let misc = json!({"enforce_quarantine_perms": true});
    let saved = json!({"antinuke": q, "misc": misc, "verification": web_verification()});
    server(&t, 0, saved).await;
    server(&t, 1, json!({"antinuke": q, "misc": misc})).await;
    let chan = |i: u64, id: u64, allow: u64, deny: u64| -> Channel {
        let ow = json!([{"id": Q.to_string(), "type": 0, "allow": allow.to_string(), "deny": deny.to_string()}]);
        serde_json::from_value(json!({"id": id.to_string(), "type": 0, "guild_id": t.g(i).to_string(),
                                      "permission_overwrites": if allow | deny == 0 { json!([]) } else { ow }}))
        .unwrap()
    };
    // Read (View + History), not Send: the overwrite verification writes.
    let (read, send) = (1024 | 65536, 2048);
    let put = |c: u64| t.calls(&format!("PUT /channels/{c}/permissions/{Q}"));
    for (i, c) in [
        (0, chan(0, 700, read, send)), // the saved verification channel
        (0, chan(0, 701, 0, 0)),       // another channel
        (1, chan(1, 702, read, send)), // made by the dashboard, not saved yet
        (1, chan(1, 703, read | send, 0)),
    ] {
        let r = crate::setup::enforce_quarantine(&t.app, &c).await;
        assert!(r.is_ok(), "{i}: {r:?}");
    }
    assert_eq!(put(700), 0, "the verification channel keeps its overwrite");
    assert_eq!(put(702), 0, "a new verification channel too");
    assert_eq!((put(701), put(703)), (1, 1), "other channels get the deny");
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

/// Migration 3900: only rows with nothing saved (the old join quarantine) become join quarantines.
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
        "CREATE TABLE quarantines (guild_id BIGINT, user_id BIGINT, saved_roles BIGINT[] NOT NULL)",
        "INSERT INTO quarantines VALUES (1, 1, '{}'), (1, 2, '{5}'), (1, 3, '{5,6}')",
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
    assert_eq!(rows, vec![(1, true), (2, false), (3, false)]);
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

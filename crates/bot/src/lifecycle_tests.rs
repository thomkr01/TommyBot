//! Tests of what ends or undoes a moderation action (deep dive g2a): unban and renewal jobs, lock
//! timers, panic's end, reverts and raid punishments. They run only when DATABASE_URL points at
//! a scratch Postgres; Discord is Ban Sync's mock.
use std::sync::Arc;

use serde_json::{json, Value};
use tommybot_antinuke::Inverse;
use tommybot_automod::{
    lockdown::{LockKind, Snapshot},
    store,
};
use tommybot_core::{jobs, permits::Level};
use twilight_model::{gateway::payload::incoming::MemberAdd, id::Id, util::Timestamp};

use crate::{
    app::now_secs,
    bansync::{self, tests::T},
    commands::{self, ModKind},
};

/// (payload, seconds until it runs) of the pending `kind` jobs of server `i`, oldest first.
async fn pending(t: &T, i: u64, kind: &str) -> Vec<(Value, i64)> {
    sqlx::query_as(
        "SELECT payload, extract(epoch FROM run_at - now())::bigint FROM scheduled_jobs
         WHERE guild_id = $1 AND kind = $2 ORDER BY id",
    )
    .bind(t.g(i).get() as i64)
    .bind(kind)
    .fetch_all(&t.app.pool)
    .await
    .unwrap()
}

async fn enqueue(t: &T, i: u64, kind: &str, payload: Value, in_secs: i64) {
    let g = t.g(i).get() as i64;
    jobs::enqueue(&t.app.pool, g, kind, payload, now_secs() + in_secs)
        .await
        .unwrap();
}

/// The bot ranks above every mock member (they have no roles), so moderation is not refused.
fn bot_on_top(t: &T, i: u64) {
    let info = crate::hierarchy::GuildInfo {
        owner: Id::new(t.owner()),
        roles: Default::default(),
        role_perms: Default::default(),
        managed: Default::default(),
        parents: Default::default(),
        channels: Default::default(),
        nsfw: Default::default(),
        bot_top: 10,
        fetched: std::time::Instant::now(),
    };
    t.app.guilds.lock().unwrap().insert(t.g(i), Arc::new(info));
}

fn iso(secs: i64) -> String {
    Timestamp::from_secs(secs).unwrap().iso_8601().to_string()
}

async fn ban(t: &T, i: u64, user: u64, duration: Option<i64>) {
    let o = Id::new(t.owner());
    let flags = crate::prefix::Flags::default();
    commands::apply_dm(
        &t.app,
        t.g(i),
        o,
        (o, &Level::Owner),
        ModKind::Ban,
        Id::new(user),
        Some("test"),
        duration,
        0,
        flags,
        None,
    )
    .await
    .unwrap();
}

/// D8: panic's end lifts only a role lock it made; a lifted lock takes its own timer with it.
#[tokio::test]
async fn panic_end_and_unlock_leave_locks_they_did_not_make() {
    let Some(t) = T::new(70).await else { return };
    t.server_config(0, t.owner(), json!({})).await;
    let (g, gid) = (t.g(0).get(), t.g(0));
    let role = Snapshot {
        kind: Some(LockKind::Role),
        ..Default::default()
    };
    store::save_lock(&t.app.pool, g, &role).await.unwrap();
    store::lock_done(&t.app.pool, g, &role).await.unwrap();
    // A manual lock was there first, so panic's own lock() returned false.
    crate::panic::end(&t.app, gid, &json!({"unlock": true, "locked": false}))
        .await
        .unwrap();
    let active = crate::lockdown::active(&t.app, gid).await.unwrap();
    assert_eq!(active, [LockKind::Role], "panic's end lifted a manual lock");
    // The lock's own timer and another lock's timer; a manual unlock drops only the first.
    enqueue(&t, 0, "unlock", json!({"lock": LockKind::Role}), 600).await;
    enqueue(&t, 0, "unlock", json!({"lock": LockKind::Channel(5)}), 600).await;
    let lifted = crate::lockdown::unlock(&t.app, gid, Some(&LockKind::Role))
        .await
        .unwrap();
    assert_eq!(lifted, 1);
    let timers: Vec<Value> = pending(&t, 0, "unlock")
        .await
        .into_iter()
        .map(|j| j.0)
        .collect();
    assert_eq!(timers, [json!({"lock": LockKind::Channel(5)})]);
    // A panic_end job from before `locked` existed still unlocks.
    store::save_lock(&t.app.pool, g, &role).await.unwrap();
    crate::panic::end(&t.app, gid, &json!({"unlock": true}))
        .await
        .unwrap();
    assert!(crate::lockdown::active(&t.app, gid)
        .await
        .unwrap()
        .is_empty());
    t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// D9: a revert step Discord refused stays unreverted, so the next run or /revert tries it again.
#[tokio::test]
async fn a_failed_revert_step_stays_for_the_next_run() {
    let Some(t) = T::new(71).await else { return };
    t.server_config(0, t.owner(), json!({})).await;
    t.ban(0, 700, "nuked");
    t.set(|m| m.fail_delete_channel = true);
    for (action, inv) in [
        ("BanAdd", Inverse::Unban { user: 700 }),
        ("ChannelCreate", Inverse::DeleteChannel { id: 9 }),
    ] {
        sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse) VALUES ($1, 42, $2, 9, $3)")
            .bind(t.g(0).get() as i64)
            .bind(action)
            .bind(json!(inv))
            .execute(&t.app.pool)
            .await
            .unwrap();
    }
    let sum = crate::antinuke::revert_actor(&t.app, t.g(0), Id::new(42), now_secs() - 60, None)
        .await
        .unwrap();
    assert_eq!((sum.ops, sum.failed), (2, 1));
    assert_eq!(t.banned(0, 700), None, "the unban ran");
    let left: Vec<(String,)> = sqlx::query_as(
        "SELECT action FROM action_log WHERE guild_id = $1 AND NOT reverted ORDER BY id",
    )
    .bind(t.g(0).get() as i64)
    .fetch_all(&t.app.pool)
    .await
    .unwrap();
    assert_eq!(left, [("ChannelCreate".to_string(),)]);
    t.cleanup().await;
}

/// D13: heat panic is on even when its log cannot be sent, and the caller is not told it failed.
#[tokio::test]
async fn a_failed_heat_panic_log_does_not_fail_the_count() {
    let Some(t) = T::new(72).await else { return };
    let panic = json!({"heat_algorithm": true, "heat_executors": 1});
    t.server_config(
        0,
        t.owner(),
        json!({"panic": panic, "logging": {"fallback": "42"}}),
    )
    .await;
    t.set(|m| m.fail_message = true);
    let r = crate::panic::note(&t.app, t.g(0), Id::new(5), true).await;
    assert!(r.is_ok(), "{r:?}");
    assert!(t
        .app
        .heat_raiders
        .lock()
        .unwrap()
        .contains_key(&t.g(0).get()));
    t.cleanup().await;
}

/// D23: a ban drops the user's older unban job (a temp ban's), keeping only its own.
#[tokio::test]
async fn a_later_ban_drops_an_older_temp_bans_unban() {
    let Some(t) = T::new(73).await else { return };
    t.server_config(0, t.owner(), json!({})).await;
    enqueue(&t, 0, "unban", json!({"user": 710}), 600).await;
    enqueue(&t, 0, "unban", json!({"user": 711}), 600).await;
    ban(&t, 0, 710, None).await;
    assert!(t.banned(0, 710).is_some());
    let left = pending(&t, 0, "unban").await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].0, json!({"user": 711}), "another user's job stays");
    // A new temp ban keeps only its own end.
    ban(&t, 0, 711, Some(7200)).await;
    let left = pending(&t, 0, "unban").await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].1 > 7000, "the new temp ban's job: {left:?}");
    t.cleanup().await;
}

/// D23: Ban Sync copies a permanent ban over a local temp ban, and the temp ban's unban goes.
#[tokio::test]
async fn a_synced_ban_replaces_a_shorter_local_temp_ban() {
    let Some(t) = T::new(74).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.ban(1, 720, "temp ban");
    enqueue(&t, 1, "unban", json!({"user": 720}), 3600).await;
    t.ban(0, 720, "raiding");
    bansync::share_job(&t.app, t.g(0), &t.share(720, Some(77)))
        .await
        .unwrap();
    t.run_copies().await;
    assert_eq!(t.rows(720).await, [T::row(1, "banned", None)]);
    assert!(pending(&t, 1, "unban").await.is_empty());
    // Discord kept the first reason, yet the copy is ours: the source's lift gives the temp ban back.
    assert_eq!(t.banned(1, 720).as_deref(), Some("temp ban"));
    t.set(|m| {
        m.bans.remove(&(t.g(0).get(), 720));
    });
    bansync::lift_job(&t.app, t.g(0), &t.share(720, Some(77)))
        .await
        .unwrap();
    let why = Some("the local temp ban runs on");
    assert_eq!(t.rows(720).await, [T::row(1, "lifted", why)]);
    assert_eq!(t.banned(1, 720).as_deref(), Some("temp ban"));
    let left = pending(&t, 1, "unban").await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert!((3590..=3600).contains(&left[0].1), "the old end: {left:?}");
    t.cleanup().await;
}

/// Live 11-13: a copy laid over a local temp ban that has ended by the source's lift is unbanned,
/// and a retry after the copy's ban (row still pending) sees that ban as its own.
#[tokio::test]
async fn a_copy_over_a_temp_ban_is_ours_on_retry_and_lifted_after_its_end() {
    let Some(t) = T::new(87).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.ban(1, 721, "temp ban");
    enqueue(&t, 1, "unban", json!({"user": 721}), 3600).await;
    t.ban(0, 721, "raiding");
    bansync::share_job(&t.app, t.g(0), &t.share(721, Some(77)))
        .await
        .unwrap();
    t.run_copies().await;
    assert_eq!(t.rows(721).await, [T::row(1, "banned", None)]);
    t.exec("UPDATE ban_sync_links SET temp_end = now() - interval '1 minute' WHERE guild_id = $1 + 1 AND user_id = 721")
        .await;
    t.set(|m| {
        m.bans.remove(&(t.g(0).get(), 721));
    });
    bansync::lift_job(&t.app, t.g(0), &t.share(721, Some(77)))
        .await
        .unwrap();
    assert_eq!(t.rows(721).await, [T::row(1, "lifted", None)]);
    assert_eq!(t.banned(1, 721), None);
    assert!(pending(&t, 1, "unban").await.is_empty());
    // A try banned over the temp ban (its unban job is gone) and stopped before the row moved on.
    t.ban(1, 722, "temp ban");
    t.ban(0, 722, "raiding");
    t.link(1, 722, 0, "pending").await;
    t.exec("UPDATE ban_sync_links SET over_reason = 'temp ban', temp_end = now() + interval '1 hour' WHERE guild_id = $1 + 1 AND user_id = 722")
        .await;
    let copy = json!({"user": 722, "source": t.g(0).get()});
    bansync::copy_job(&t.app, t.g(1), &copy).await.unwrap();
    assert_eq!(t.rows(722).await, [T::row(1, "banned", None)]);
    let cases = t
        .count(
            "SELECT count(*) FROM cases WHERE guild_id = $1 + 1 AND user_id = 722 AND kind = 'ban'",
        )
        .await;
    assert_eq!(cases, 1, "the retry finished the copy");
    t.cleanup().await;
}

/// R-g4: a staff ban over a done copy is theirs (Discord keeps the copy's reason, or the temp
/// ban's), so the source's lift leaves it and its unban job; a leased temp unban job still counts.
#[tokio::test]
async fn a_staff_ban_over_a_copy_is_not_lifted_and_a_leased_temp_job_counts() {
    let Some(t) = T::new(62).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    bot_on_top(&t, 1);
    let unban_source = |u: u64| {
        t.set(|m| {
            m.bans.remove(&(t.g(0).get(), u));
        })
    };
    let hand = Some("banned again by hand");
    // A copy laid over a local temp ban, then a week's tempban by staff.
    t.ban(1, 723, "temp ban");
    enqueue(&t, 1, "unban", json!({"user": 723}), 3600).await;
    // A plain copy, then a permanent ban by staff.
    for u in [723, 724] {
        t.ban(0, u, "raiding");
        bansync::share_job(&t.app, t.g(0), &t.share(u, Some(77)))
            .await
            .unwrap();
    }
    t.run_copies().await;
    assert_eq!(t.rows(723).await, [T::row(1, "banned", None)]);
    assert_eq!(t.rows(724).await, [T::row(1, "banned", None)]);
    ban(&t, 1, 723, Some(7 * 86_400)).await;
    ban(&t, 1, 724, None).await;
    for u in [723, 724] {
        unban_source(u);
        bansync::lift_job(&t.app, t.g(0), &t.share(u, Some(77)))
            .await
            .unwrap();
        assert_eq!(t.rows(u).await, [T::row(1, "skipped", hand)], "{u}");
        assert!(t.banned(1, u).is_some(), "{u}");
    }
    let left = pending(&t, 1, "unban").await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].1 > 7 * 86_400 - 10, "staff's own end: {left:?}");
    // A retry while the temp ban's unban job is leased did not ban yet: not ours, it waits.
    t.ban(1, 725, "temp ban");
    t.ban(0, 725, "raiding");
    enqueue(&t, 1, "unban", json!({"user": 725}), 3600).await;
    t.exec("UPDATE scheduled_jobs SET locked_until = now() + interval '60 seconds' WHERE guild_id = $1 + 1 AND payload->>'user' = '725'")
        .await;
    t.link(1, 725, 0, "pending").await;
    t.exec("UPDATE ban_sync_links SET over_reason = 'temp ban', temp_end = (SELECT run_at FROM scheduled_jobs WHERE guild_id = $1 + 1 AND payload->>'user' = '725') WHERE guild_id = $1 + 1 AND user_id = 725")
        .await;
    let copy = json!({"user": 725, "source": t.g(0).get()});
    assert!(bansync::copy_job(&t.app, t.g(1), &copy).await.is_err());
    assert_eq!(
        t.rows(725).await,
        [T::row(1, "pending", None)],
        "no phantom ban case"
    );
    t.cleanup().await;
}

/// R-g4 round 2: a copy is not laid over a temp ban that is ending (its unban is due or running);
/// the copy job fails and retries, and lays the copy once the temp unban is no longer running.
#[tokio::test]
async fn a_copy_waits_for_a_temp_ban_that_is_ending() {
    let Some(t) = T::new(63).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.ban(1, 726, "temp ban");
    t.ban(0, 726, "raiding");
    enqueue(&t, 1, "unban", json!({"user": 726}), 3600).await;
    t.exec("UPDATE scheduled_jobs SET locked_until = now() + interval '60 seconds' WHERE guild_id = $1 + 1 AND payload->>'user' = '726'")
        .await;
    t.link(1, 726, 0, "pending").await;
    let copy = json!({"user": 726, "source": t.g(0).get()});
    assert!(bansync::copy_job(&t.app, t.g(1), &copy).await.is_err());
    assert_eq!(t.rows(726).await, [T::row(1, "pending", None)]);
    assert_eq!(pending(&t, 1, "unban").await.len(), 1, "the temp job stays");
    // The try failed and freed its lease: the copy goes over the temp ban.
    t.exec("UPDATE scheduled_jobs SET locked_until = NULL WHERE guild_id = $1 + 1")
        .await;
    bansync::copy_job(&t.app, t.g(1), &copy).await.unwrap();
    assert_eq!(t.rows(726).await, [T::row(1, "banned", None)]);
    assert!(pending(&t, 1, "unban").await.is_empty());
    t.cleanup().await;
}

/// FINAL review: the ban's own drop of the temp ban's unban job is best effort; when it fails,
/// the copy does not count as done (a left job would disown and later lift it), and the retry
/// finishes it.
#[tokio::test]
async fn a_copy_retries_when_the_temp_ban_job_cannot_be_dropped() {
    let Some(t) = T::new(64).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.ban(1, 727, "temp ban");
    t.ban(0, 727, "raiding");
    enqueue(&t, 1, "unban", json!({"user": 727}), 3600).await;
    t.link(1, 727, 0, "pending").await;
    let g = t.g(1).get();
    let ddl = format!(
        "CREATE FUNCTION tb_keep_727_{g}() RETURNS trigger LANGUAGE plpgsql AS
           $$ BEGIN RAISE EXCEPTION 'delete refused'; END $$;
         CREATE TRIGGER tb_keep_727_{g} BEFORE DELETE ON scheduled_jobs FOR EACH ROW
           WHEN (OLD.guild_id = {g} AND OLD.kind = 'unban' AND OLD.payload->>'user' = '727')
           EXECUTE FUNCTION tb_keep_727_{g}();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(&t.app.pool)
        .await
        .unwrap();
    let copy = json!({"user": 727, "source": t.g(0).get()});
    let first = bansync::copy_job(&t.app, t.g(1), &copy).await;
    let undo =
        format!("DROP TRIGGER tb_keep_727_{g} ON scheduled_jobs; DROP FUNCTION tb_keep_727_{g}();");
    sqlx::raw_sql(sqlx::AssertSqlSafe(undo))
        .execute(&t.app.pool)
        .await
        .unwrap();
    assert!(first.is_err());
    assert_eq!(t.rows(727).await, [T::row(1, "pending", None)]);
    bansync::copy_job(&t.app, t.g(1), &copy).await.unwrap();
    assert_eq!(t.rows(727).await, [T::row(1, "banned", None)]);
    assert!(pending(&t, 1, "unban").await.is_empty());
    t.cleanup().await;
}

/// D26: a new timeout drops an older renewal; a renewal for a member who left retries hourly.
#[tokio::test]
async fn timeout_renewals_end_with_the_timeout() {
    let Some(t) = T::new(75).await else { return };
    t.server_config(0, t.owner(), json!({})).await;
    bot_on_top(&t, 0);
    t.set(|m| {
        m.members.insert((t.g(0).get(), 730), vec![]);
    });
    let later = now_secs() + 40 * 86_400;
    enqueue(
        &t,
        0,
        "timeout_renew",
        json!({"user": 730, "until_secs": later}),
        600,
    )
    .await;
    let o = Id::new(t.owner());
    let flags = crate::prefix::Flags::default();
    commands::apply_dm(
        &t.app,
        t.g(0),
        o,
        (o, &Level::Owner),
        ModKind::Timeout,
        Id::new(730),
        None,
        Some(600),
        0,
        flags,
        None,
    )
    .await
    .unwrap();
    assert!(t
        .mock
        .lock()
        .unwrap()
        .timeouts
        .contains_key(&(t.g(0).get(), 730)));
    assert!(
        pending(&t, 0, "timeout_renew").await.is_empty(),
        "old renewal kept"
    );
    // 731 left: Discord keeps the timeout for a rejoin, so the renewal tries again in an hour.
    let job = jobs::Job {
        id: 0,
        guild_id: t.g(0).get() as i64,
        kind: "timeout_renew".into(),
        payload: json!({"user": 731, "until_secs": later}),
        attempts: 0,
    };
    super::dispatch(&t.app, &job).await.unwrap();
    let left = pending(&t, 0, "timeout_renew").await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert!((3590..=3600).contains(&left[0].1), "{left:?}");
    t.cleanup().await;
}

/// D28: an automatic timeout never shortens a longer one.
#[tokio::test]
async fn an_automatic_timeout_keeps_a_longer_one() {
    let Some(t) = T::new(76).await else { return };
    t.server_config(0, t.owner(), json!({})).await;
    let (g, week) = (t.g(0).get(), iso(now_secs() + 7 * 86_400));
    t.set(|m| {
        m.members.insert((g, 740), vec![]);
        m.timeouts.insert((g, 740), week.clone());
    });
    commands::timeout(&t.app.http, t.g(0), Id::new(740), 300, "Automod")
        .await
        .unwrap();
    assert_eq!(t.mock.lock().unwrap().timeouts[&(g, 740)], week);
    // A shorter one is extended.
    t.set(|m| {
        m.timeouts.insert((g, 740), iso(now_secs() + 60));
    });
    commands::timeout(&t.app.http, t.g(0), Id::new(740), 300, "Automod")
        .await
        .unwrap();
    let until = t.mock.lock().unwrap().timeouts[&(g, 740)].clone();
    let until = Timestamp::parse(&until).unwrap().as_secs();
    assert!(until >= now_secs() + 290, "{until}");
    t.cleanup().await;
}

fn raid_cfg() -> Value {
    json!({"joinraid": {
        "enabled": true, "count": 3, "span_ms": 60_000, "suspicious_only": false,
        "active_ms": 60_000, "string_patterns": [], "flag_younger_than_ms": null,
        "flag_no_avatar": true, "flag_spammer": false, "id_flag_min": null, "action": "Ban",
    }})
}

async fn join(t: &T, i: u64, u: u64) {
    t.set(|m| {
        m.members.insert((t.g(i).get(), u), vec![]);
    });
    let m: MemberAdd = serde_json::from_value(json!({
        "guild_id": t.g(i).to_string(), "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
        "user": {"id": u.to_string(), "username": "raider", "discriminator": "0", "avatar": null},
    }))
    .unwrap();
    crate::joingate::on_join(&t.app, &m, false).await.unwrap();
}

/// D60: a join raid does not punish an earlier flagged joiner who is staff now.
#[tokio::test]
async fn a_join_raid_skips_a_joiner_made_staff_since() {
    let Some(t) = T::new(77).await else { return };
    t.server_config(0, t.owner(), raid_cfg()).await;
    join(&t, 0, 751).await;
    join(&t, 0, 752).await;
    t.exec("INSERT INTO trusted_admins (guild_id, user_id) VALUES ($1, 751)")
        .await;
    join(&t, 0, 753).await;
    assert_eq!(t.banned(0, 751), None, "staff was banned");
    assert!(t.banned(0, 752).is_some() && t.banned(0, 753).is_some());
    t.cleanup().await;
}

/// D64: a raid ban that fails with a 5xx is retried from a job; a refused one is not.
#[tokio::test]
async fn a_raid_ban_discord_did_not_answer_is_retried() {
    let Some(t) = T::new(78).await else { return };
    t.server_config(0, t.owner(), raid_cfg()).await;
    t.set(|m| m.fail_put_5xx = true);
    for u in [761, 762, 763] {
        join(&t, 0, u).await;
    }
    assert_eq!(t.banned(0, 761), None);
    let retries = pending(&t, 0, "raid_punish").await;
    assert_eq!(retries.len(), 3, "{retries:?}");
    t.set(|m| m.fail_put_5xx = false);
    for (payload, _) in retries {
        let job = jobs::Job {
            id: 0,
            guild_id: t.g(0).get() as i64,
            kind: "raid_punish".into(),
            payload,
            attempts: 0,
        };
        super::dispatch(&t.app, &job).await.unwrap();
    }
    assert!([761, 762, 763].iter().all(|u| t.banned(0, *u).is_some()));
    // Refused (403): final, no retry.
    t.exec("DELETE FROM scheduled_jobs WHERE guild_id = $1")
        .await;
    t.set(|m| m.fail_put = true);
    for u in [764, 765, 766] {
        join(&t, 0, u).await;
    }
    assert!(pending(&t, 0, "raid_punish").await.is_empty());
    t.cleanup().await;
}

fn raid_job(t: &T, payload: Value) -> jobs::Job {
    jobs::Job {
        id: 0,
        guild_id: t.g(0).get() as i64,
        kind: "raid_punish".into(),
        payload,
        attempts: 0,
    }
}

/// D64 review: a punishment that worked is not retried because its log failed.
#[tokio::test]
async fn a_failed_join_gate_log_does_not_retry_a_done_punishment() {
    let Some(t) = T::new(79).await else { return };
    let cfg = json!({"logging": {"fallback": "42"}});
    t.server_config(0, t.owner(), cfg).await;
    // Not a refusal: before the fix this failed `punish` and queued a second ban.
    t.set(|m| m.fail_message_5xx = true);
    let c = t.app.cfg(t.g(0).get()).await.unwrap();
    let ban = tommybot_automod::join::Action::Ban;
    crate::joingate::punish_or_retry(&t.app, &c, t.g(0), 790, ban, "join raid").await;
    assert!(t.banned(0, 790).is_some());
    assert!(pending(&t, 0, "raid_punish").await.is_empty());
    t.cleanup().await;
}

/// D64 review: a retry leaves alone whoever was made staff or let back in since, and staff's
/// Allow (also every unban and ban appeal, through `raiders::allow`) drops a pending retry.
#[tokio::test]
async fn a_raid_retry_skips_staff_and_allowed_users() {
    let Some(t) = T::new(80).await else { return };
    t.server_config(0, t.owner(), json!({})).await;
    t.exec("INSERT INTO trusted_admins (guild_id, user_id) VALUES ($1, 801)")
        .await;
    t.exec("INSERT INTO raider_allow (guild_id, user_id, by) VALUES ($1, 802, 1)")
        .await;
    for u in [801, 802, 803] {
        let job = raid_job(&t, json!({"user": u, "action": "Ban", "why": "join raid"}));
        super::dispatch(&t.app, &job).await.unwrap();
    }
    assert_eq!((t.banned(0, 801), t.banned(0, 802)), (None, None));
    assert!(t.banned(0, 803).is_some());
    let payload = json!({"user": 804, "action": "Ban", "why": "join raid"});
    enqueue(&t, 0, "raid_punish", payload, 30).await;
    let g = t.g(0).get() as i64;
    tommybot_core::raiders::allow(&t.app.pool, g, 804, 1)
        .await
        .unwrap();
    assert!(pending(&t, 0, "raid_punish").await.is_empty());
    t.cleanup().await;
}

/// D60b: when the staff lookup for an earlier flagged joiner fails, the raid retries later
/// instead of punishing now, and the retry checks immunity again.
#[tokio::test]
async fn a_failed_staff_lookup_in_a_raid_retries_instead_of_punishing() {
    let Some(t) = T::new(170).await else { return };
    t.server_config(0, t.owner(), raid_cfg()).await;
    join(&t, 0, 401).await;
    join(&t, 0, 402).await;
    // Discord fails every member lookup: neither earlier joiner can be checked.
    t.set(|m| m.fail_member = true);
    join(&t, 0, 403).await;
    assert_eq!((t.banned(0, 401), t.banned(0, 402)), (None, None));
    assert!(t.banned(0, 403).is_some(), "the joiner itself is punished");
    // D57: the raid punished the joiner before this join's invite lookup.
    let at = |what: &str| {
        t.mock
            .lock()
            .unwrap()
            .log
            .iter()
            .rposition(|l| l.contains(what))
    };
    let g = t.g(0);
    assert!(at(&format!("PUT /guilds/{g}/bans/403")) < at(&format!("GET /guilds/{g}/invites")));
    let retries = pending(&t, 0, "raid_punish").await;
    assert_eq!(retries.len(), 2, "{retries:?}");
    // Made a Trusted Admin meanwhile: the retry spares them.
    t.exec("INSERT INTO trusted_admins (guild_id, user_id) VALUES ($1, 401)")
        .await;
    t.set(|m| m.fail_member = false);
    for (payload, _) in retries {
        super::dispatch(&t.app, &raid_job(&t, payload))
            .await
            .unwrap();
    }
    assert_eq!(t.banned(0, 401), None, "staff was banned");
    assert!(t.banned(0, 402).is_some());
    t.cleanup().await;
}

/// D60b in voice: a voice raid checks earlier flagged members for staff again, and a failed
/// lookup retries later instead of punishing.
#[tokio::test]
async fn a_voice_raid_rechecks_earlier_members_for_staff() {
    let Some(t) = T::new(172).await else { return };
    let cfg = json!({"joinraid": {"voice": {
        "enabled": true, "count": 3, "span_ms": 60_000, "active_ms": 60_000, "action": "Ban"}}});
    t.server_config(0, t.owner(), cfg).await;
    let voice = |u: u64, member: bool| -> twilight_model::voice::VoiceState {
        let m = json!({"user": {"id": u.to_string(), "username": "v", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0});
        serde_json::from_value(json!({"guild_id": t.g(0).to_string(), "channel_id": "620",
            "user_id": u.to_string(), "session_id": "s", "deaf": false, "mute": false,
            "self_deaf": false, "self_mute": false, "self_stream": false, "self_video": false,
            "suppress": false, "request_to_speak_timestamp": null,
            "member": if member { m } else { Value::Null }}))
        .unwrap()
    };
    for u in [481, 482] {
        t.set(|m| {
            m.members.insert((t.g(0).get(), u), vec![]);
        });
        crate::voice_raid::on_voice(&t.app, &voice(u, false), None)
            .await
            .unwrap();
    }
    // 481 is made staff; then Discord fails every member read, so neither can be checked now.
    t.exec("INSERT INTO trusted_admins (guild_id, user_id) VALUES ($1, 481)")
        .await;
    t.set(|m| m.fail_member = true);
    crate::voice_raid::on_voice(&t.app, &voice(483, true), None)
        .await
        .unwrap();
    assert_eq!((t.banned(0, 481), t.banned(0, 482)), (None, None));
    assert!(t.banned(0, 483).is_some(), "the member who joined now");
    // Neither could be checked, but both are out of voice already; only the punishment waits.
    let out = |u: u64| {
        t.calls(&format!(
            "PATCH /guilds/{}/members/{u} {{\"channel_id\":null}}",
            t.g(0)
        ))
    };
    assert_eq!(
        (out(481), out(482)),
        (1, 1),
        "a failed check still disconnects"
    );
    let retries = pending(&t, 0, "raid_punish").await;
    assert_eq!(retries.len(), 2, "{retries:?}");
    t.set(|m| m.fail_member = false);
    for (payload, _) in retries {
        super::dispatch(&t.app, &raid_job(&t, payload))
            .await
            .unwrap();
    }
    assert_eq!(t.banned(0, 481), None, "staff was banned");
    assert!(t.banned(0, 482).is_some());
    t.cleanup().await;
}

/// R1: a raid that only kicks does not cancel a Join Gate ban of the same joiner.
#[tokio::test]
async fn a_raid_kick_does_not_cancel_a_join_gate_ban() {
    let Some(t) = T::new(183).await else { return };
    let mut cfg = raid_cfg();
    cfg["joinraid"]["action"] = json!("Kick");
    cfg["joingate"] = json!({"enabled": true, "no_avatar": {"enabled": true, "action": "Ban"}});
    t.server_config(0, t.owner(), cfg).await;
    for u in [831, 832, 833] {
        join(&t, 0, u).await;
    }
    assert!(t.banned(0, 833).is_some(), "the gate's ban still applies");
    t.cleanup().await;
}

/// D57 review: a raid kick or ban that failed leaves the joiner in, so they still get the
/// verification quarantine.
#[tokio::test]
async fn a_failed_raid_ban_still_quarantines_for_verification() {
    let Some(t) = T::new(184).await else { return };
    let mut cfg = raid_cfg();
    cfg["verification"] = json!({"enabled": true, "mode": "Web", "target": "Everyone",
        "on_fail": "Nothing", "roles": ["503"], "channel": "700", "quarantine_on_join": true});
    t.server_config(0, t.owner(), cfg).await;
    join(&t, 0, 841).await;
    join(&t, 0, 842).await;
    t.set(|m| m.fail_put = true);
    join(&t, 0, 843).await;
    assert_eq!(t.banned(0, 843), None);
    let kind = crate::antinuke::quarantine_kind(&t.app, t.g(0), Id::new(843))
        .await
        .unwrap();
    assert_eq!(
        kind,
        Some(true),
        "the joiner waits in verification's quarantine"
    );
    t.cleanup().await;
}

/// D56: a Raider List hit counts towards a Join Raid but is not punished a second time.
#[tokio::test]
async fn a_raider_list_hit_counts_in_a_raid_but_is_punished_once() {
    let Some(t) = T::new(171).await else { return };
    let mut cfg = raid_cfg();
    cfg["raider_list"] = json!({"enabled": true, "action": "kick"});
    t.server_config(0, t.owner(), cfg).await;
    let listed = t.base * 10 + 1;
    let add = "INSERT INTO raiders (user_id, status, category, reviewed_at, verified)
               VALUES ($1, 'confirmed', 'raid', now(), true)";
    sqlx::query(add)
        .bind(listed as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    join(&t, 0, listed).await;
    join(&t, 0, 412).await;
    // The third join triggers only because the listed one counted.
    join(&t, 0, 413).await;
    assert!(t.banned(0, 412).is_some() && t.banned(0, 413).is_some());
    assert_eq!(
        t.banned(0, listed),
        None,
        "kicked by the list, not banned again"
    );
    assert_eq!(
        t.calls(&format!("DELETE /guilds/{}/members/{listed}", t.g(0))),
        1
    );
    sqlx::query("DELETE FROM raiders WHERE user_id = $1")
        .bind(listed as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.cleanup().await;
}

/// D8 review: panic's end lifts only a lock its own lock call saved. A lock that failed before
/// saving (here: the bot's member cannot be read) is not panic's, so a later staff lock stays.
#[tokio::test]
async fn panic_owns_only_a_lock_it_saved() {
    let Some(t) = T::new(81).await else { return };
    let panic = json!({"lock_on_trigger": true, "duration_secs": 600});
    for i in [0, 1] {
        t.server_config(i, t.owner(), json!({ "panic": panic }))
            .await;
    }
    // Server 1 can be locked: the bot's own member is there.
    t.set(|m| {
        m.members.insert((t.g(1).get(), bansync::tests::ME), vec![]);
    });
    for i in [0, 1] {
        let cfg = t.app.cfg(t.g(i).get()).await.unwrap().panic.clone();
        crate::panic::start(&t.app, t.g(i), &cfg).await.unwrap();
    }
    let locked = |p: Vec<(Value, i64)>| p[0].0["locked"].clone();
    assert_eq!(
        locked(pending(&t, 0, "panic_end").await),
        false,
        "a failed lock is not panic's"
    );
    assert!(crate::lockdown::active(&t.app, t.g(0))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(locked(pending(&t, 1, "panic_end").await), true);
    assert_eq!(
        crate::lockdown::active(&t.app, t.g(1)).await.unwrap(),
        [LockKind::Role, LockKind::Server]
    );
    for i in [0, 1] {
        t.app.panic_runs.lock().unwrap().remove(&t.g(i).get());
    }
    t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// Live 13-11: panic's role lock left @everyone writing. Panic now also takes a server lock from
/// the main roles, gives a whitelisted category back only what that lock took, and its end lifts
/// both. A server lock staff made first stays after panic.
#[tokio::test]
async fn panic_stops_writing_and_puts_it_back() {
    let Some(t) = T::new(82).await else { return };
    const SEND: u64 = 1 << 11;
    const VIEW: u64 = 1 << 10;
    const MENTION: u64 = 1 << 17;
    const CAT: u64 = 50;
    let everyone = VIEW | SEND | MENTION | (1 << 47);
    for i in [0, 1] {
        let g = t.g(i).get();
        let panic = json!({"lock_on_trigger": true, "duration_secs": 600, "unlock_when_ending": true,
                           "whitelisted_categories": [(CAT + i).to_string()]});
        t.server_config(i, t.owner(), json!({ "panic": panic }))
            .await;
        t.set(|m| {
            m.members.insert((g, bansync::tests::ME), vec![7 + i]);
            m.roles
                .insert(g, vec![(g, false, everyone, 0), (7 + i, false, 0, 5)]);
            // The category already denies @everyone reactions: the exemption must not open them.
            let deny = (1u64 << 6).to_string();
            m.overwrites.insert(
                CAT + i,
                vec![json!({"id": g.to_string(), "type": 0, "allow": "0", "deny": deny})],
            );
            m.channel_guild.insert(CAT + i, g);
        });
    }
    let perms = |i: u64| {
        let g = t.g(i).get();
        t.mock.lock().unwrap().roles[&g]
            .iter()
            .find(|r| r.0 == g)
            .unwrap()
            .2
    };
    // Server 1: staff locked the server before panic.
    crate::lockdown::lock(&t.app, t.g(1), LockKind::Server)
        .await
        .unwrap();
    assert_eq!(perms(1) & SEND, 0);
    for i in [0, 1] {
        let cfg = t.app.cfg(t.g(i).get()).await.unwrap().panic.clone();
        crate::panic::start(&t.app, t.g(i), &cfg).await.unwrap();
    }
    assert_eq!(
        crate::lockdown::active(&t.app, t.g(0)).await.unwrap(),
        [LockKind::Role, LockKind::Server]
    );
    assert_eq!(
        perms(0) & (SEND | MENTION),
        0,
        "panic left @everyone writing"
    );
    assert_ne!(perms(0) & (1 << 47), 0, "unknown bits are kept");
    let end0 = pending(&t, 0, "panic_end").await[0].0.clone();
    assert_eq!(
        (end0["locked"].clone(), end0["locked_server"].clone()),
        (json!(true), json!(true))
    );
    let ex = end0["exempt"].as_array().unwrap();
    assert_eq!(ex.len(), 1, "{end0}");
    assert_eq!(ex[0]["role"], json!(t.g(0).get()));
    assert_eq!(
        (ex[0]["had"].clone(), ex[0]["deny"].clone()),
        (json!(true), json!(1u64 << 6))
    );
    let put = {
        let m = t.mock.lock().unwrap();
        m.log
            .iter()
            .filter(|l| {
                l.starts_with("PUT ") && l.contains(&format!("channels/{CAT}/permissions/"))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(put.len(), 1, "{put:?}");
    let body: Value = serde_json::from_str(put[0].splitn(3, ' ').nth(2).unwrap()).unwrap();
    let allow: u64 = body["allow"].as_str().unwrap().parse().unwrap();
    let deny: u64 = body["deny"].as_str().unwrap().parse().unwrap();
    assert_eq!(allow & SEND, SEND, "{body}");
    assert_eq!((allow & (1 << 6), deny), (0, 1 << 6), "{body}");
    assert_eq!(
        allow & MENTION,
        0,
        "the role lock's Mention @everyone came back: {body}"
    );
    // Server 1: the staff lock already took writing, so panic adds only the role lock and
    // the category, which the staff lock silenced too, is left as it is.
    assert_eq!(
        crate::lockdown::active(&t.app, t.g(1)).await.unwrap(),
        [LockKind::Server, LockKind::Role]
    );
    let end1 = pending(&t, 1, "panic_end").await[0].0.clone();
    assert_eq!(end1["locked_server"], json!(false));
    assert_eq!(end1["exempt"], json!([]));
    for i in [0, 1] {
        let p = pending(&t, i, "panic_end").await[0].0.clone();
        crate::panic::end(&t.app, t.g(i), &p).await.unwrap();
    }
    assert!(crate::lockdown::active(&t.app, t.g(0))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(perms(0), everyone);
    assert_eq!(
        t.calls(&format!("channels/{CAT}/permissions/")),
        2,
        "the category overwrite is put back"
    );
    assert_eq!(
        crate::lockdown::active(&t.app, t.g(1)).await.unwrap(),
        [LockKind::Server],
        "panic's end lifted the staff lock"
    );
    assert_eq!(perms(1) & SEND, 0);
    t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// Thomas 13-11: a lock from before panic that is lifted during panic (its timer ran) is taken
/// again by panic, which then owns it; a role a restore made again under a lock is locked too;
/// an end that ran or is running while panic started is seen by the start.
#[tokio::test]
async fn panic_takes_back_what_an_older_lock_gave_up() {
    let Some(t) = T::new(83).await else { return };
    const SEND: u64 = 1 << 11;
    let (g, gid) = (t.g(0).get(), t.g(0));
    let panic = json!({"lock_on_trigger": true, "duration_secs": 600, "unlock_when_ending": true});
    t.server_config(0, t.owner(), json!({ "panic": panic }))
        .await;
    t.set(|m| {
        m.members.insert((g, bansync::tests::ME), vec![7]);
        m.roles
            .insert(g, vec![(g, false, SEND | (1 << 10), 0), (7, false, 0, 5)]);
    });
    let perms = |id: u64| {
        t.mock.lock().unwrap().roles[&g]
            .iter()
            .find(|r| r.0 == id)
            .unwrap()
            .2
    };
    crate::lockdown::lock(&t.app, gid, LockKind::Server)
        .await
        .unwrap();
    let cfg = t.app.cfg(g).await.unwrap().panic.clone();
    crate::panic::start(&t.app, gid, &cfg).await.unwrap();
    let flags = || async {
        let p = pending(&t, 0, "panic_end").await[0].0.clone();
        (p["locked"].clone(), p["locked_server"].clone())
    };
    assert_eq!(flags().await, (json!(true), json!(false)));
    assert_eq!(
        pending(&t, 0, "panic_end").await[0].0["waits_server"],
        json!(true)
    );
    // The staff lock's timer runs during panic.
    crate::lockdown::unlock(&t.app, gid, Some(&LockKind::Server))
        .await
        .unwrap();
    assert_eq!(perms(g) & SEND, 0, "everyone writes again during panic");
    assert_eq!(flags().await, (json!(true), json!(true)));
    // Staff lifting panic's own lock is not undone, also not by a later unlock of another part.
    crate::lockdown::unlock(&t.app, gid, Some(&LockKind::Server))
        .await
        .unwrap();
    assert_ne!(perms(g) & SEND, 0);
    assert_eq!(flags().await, (json!(true), json!(false)));
    crate::lockdown::unlock(&t.app, gid, Some(&LockKind::Role))
        .await
        .unwrap();
    assert_ne!(
        perms(g) & SEND,
        0,
        "an unlock of another part muted everyone again"
    );
    let p = pending(&t, 0, "panic_end").await[0].0.clone();
    assert_eq!(
        (p["waits_role"].clone(), p["waits_server"].clone()),
        (json!(null), json!(false))
    );
    // A restore made the locked role 7 again as 70, with the backup's Send.
    crate::lockdown::lock(&t.app, gid, LockKind::Server)
        .await
        .unwrap();
    t.set(|m| m.roles.get_mut(&g).unwrap().push((70, false, SEND, 1)));
    t.exec("UPDATE lock_snapshots SET snapshot = jsonb_set(snapshot, '{role_perms}', '[[7, 2048]]') WHERE guild_id = $1 AND kind = 'Some(Server)'").await;
    crate::lockdown::forget_locks(g);
    crate::lockdown::relink_roles(&t.app, gid, &[(7, 70)].into_iter().collect()).await;
    assert_eq!(
        perms(70) & SEND,
        0,
        "the recreated role writes during the lock"
    );
    crate::lockdown::unlock(&t.app, gid, Some(&LockKind::Server))
        .await
        .unwrap();
    assert_ne!(
        perms(70) & SEND,
        0,
        "the unlock gave the recreated role nothing back"
    );
    // Server 1: the staff lock ran out while panic was still starting (relock waited then).
    let (g1, gid1) = (t.g(1).get(), t.g(1));
    t.server_config(1, t.owner(), json!({ "panic": panic }))
        .await;
    t.set(|m| {
        m.members.insert((g1, bansync::tests::ME), vec![8]);
        m.roles
            .insert(g1, vec![(g1, false, SEND | (1 << 10), 0), (8, false, 0, 5)]);
    });
    crate::lockdown::lock(&t.app, gid1, LockKind::Server)
        .await
        .unwrap();
    crate::panic::start(&t.app, gid1, &cfg).await.unwrap();
    crate::lockdown::unlock_now(&t.app, gid1, Some(&LockKind::Server))
        .await
        .unwrap();
    crate::panic::catch_up_waits(&t.app, gid1).await;
    let p1 = pending(&t, 1, "panic_end").await[0].0.clone();
    assert_eq!(
        (p1["locked_server"].clone(), p1["waits_server"].clone()),
        (json!(true), json!(false))
    );
    assert_eq!(
        crate::lockdown::active(&t.app, gid1).await.unwrap(),
        [LockKind::Role, LockKind::Server]
    );
    t.app.panic_runs.lock().unwrap().remove(&g1);
    // An end the scheduler took (or that ran) is no longer "later".
    assert!(crate::panic::ends_later(&t.app, gid).await);
    t.exec("UPDATE scheduled_jobs SET locked_until = now() + interval '1 minute' WHERE guild_id = $1 AND kind = 'panic_end'").await;
    assert!(!crate::panic::ends_later(&t.app, gid).await);
    t.exec("DELETE FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'panic_end'")
        .await;
    assert!(!crate::panic::ends_later(&t.app, gid).await);
    t.app.panic_runs.lock().unwrap().remove(&g);
    t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

/// Live 11-13: a role lock and unlock keep permission bits twilight does not know (1<<48, Set
/// Voice Channel Status), on the role and on a channel overwrite.
#[tokio::test]
async fn a_lock_and_unlock_keep_unknown_permission_bits() {
    let Some(t) = T::new(86).await else { return };
    t.server_config(0, t.owner(), json!({})).await;
    let (g, gid) = (t.g(0).get(), t.g(0));
    let (voice_status, ban) = (1u64 << 48, 1u64 << 2);
    t.set(|m| {
        m.roles.insert(
            g,
            vec![(g, false, voice_status | ban, 0), (500, true, 8, 10)],
        );
        m.members.insert((g, bansync::tests::ME), vec![500]);
    });
    assert!(crate::lockdown::lock(&t.app, gid, LockKind::Role)
        .await
        .unwrap());
    let perms = |t: &T| t.mock.lock().unwrap().roles[&g][0].2;
    assert_eq!(
        perms(&t),
        voice_status,
        "locked: Ban Members goes, bit 48 stays"
    );
    crate::lockdown::unlock(&t.app, gid, Some(&LockKind::Role))
        .await
        .unwrap();
    assert_eq!(perms(&t), voice_status | ban);
    // Overwrites are read through the same twilight type.
    let o: twilight_model::channel::permission_overwrite::PermissionOverwrite =
        serde_json::from_value(
            json!({"id": "5", "type": 0, "allow": voice_status.to_string(), "deny": "0"}),
        )
        .unwrap();
    assert_eq!(o.allow.bits(), voice_status);
    t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
        .await;
    t.cleanup().await;
}

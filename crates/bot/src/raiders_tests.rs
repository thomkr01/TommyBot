//! Tests of `raiders`. The DB tests run only when DATABASE_URL points at a scratch Postgres, with
//! Ban Sync's mock Discord.
use super::*;
use crate::bansync::tests::{BOT_OWNER, ME, T};
use twilight_model::gateway::payload::incoming::MemberAdd;

#[test]
fn reports_need_a_case_a_big_old_server_and_stay_under_the_daily_cap() {
    assert!(report_refusal(false, 500, 30, 0).unwrap().contains("case"));
    assert!(report_refusal(true, 99, 30, 0)
        .unwrap()
        .contains("100 members"));
    assert!(report_refusal(true, 500, 6, 0).unwrap().contains("7 days"));
    assert!(report_refusal(true, 500, 30, 10)
        .unwrap()
        .contains("10 reports"));
    assert_eq!(report_refusal(true, 100, 7, 9), None);
}

#[test]
fn each_button_needs_its_permit() {
    let perm = |p: &[&str]| Level::Custom {
        rank: 1,
        perms: p.iter().map(|s| s.to_string()).collect(),
        whitelist: vec![],
        limits: Default::default(),
    };
    assert_eq!(click_permit(&perm(&["kick"]), "kick"), Some(true));
    assert_eq!(click_permit(&perm(&["kick"]), "ban"), Some(false));
    assert_eq!(click_permit(&perm(&["kick"]), "allow"), Some(true));
    assert_eq!(click_permit(&perm(&["warn"]), "allow"), Some(false));
    assert_eq!(click_permit(&Level::None, "quarantine"), Some(false));
    assert_eq!(click_permit(&Level::Owner, "nuke"), None);
}

#[test]
fn evidence_detail_is_counts_and_ids_only() {
    let d = Detail {
        count: Some(3),
        case: Some(12),
        filter: Some("scam link"),
    };
    assert_eq!(
        json!(d),
        json!({"count": 3, "case": 12, "filter": "scam link"})
    );
    assert_eq!(json!(Detail::default()), json!({}));
    assert_eq!(parse_cat("RAID"), Some(RaiderCategory::Raid));
    assert_eq!(parse_cat("other"), None);
}

#[test]
fn the_listing_dm_names_everything_the_owner_checks() {
    let row = (
        5,
        "auto_nuke".into(),
        "n7*".into(),
        json!({"case": 4, "count": 2}),
        99,
    );
    let t = listed_text(
        42,
        Some(("bad_guy", true)),
        3,
        &[("Main".into(), Some(120), row)],
    );
    for part in [
        "<@42>",
        "`42`",
        "bad\\_guy",
        "bot: yes",
        "**3**",
        "**Main** (`5`, 120 members)",
        "`auto_nuke`",
        "ref `n7\\*`",
        "case 4",
        "count 2",
        "Verify",
    ] {
        assert!(t.contains(part), "{part}: {t}");
    }
}

impl T {
    /// A user id only this test uses.
    fn u(&self, k: u64) -> Id<UserMarker> {
        Id::new(self.base * 10 + k)
    }

    async fn raider_list(&self, i: u64, owner: u64, cfg: Value) {
        let cfg = json!({"raider_list": cfg, "joingate": {"enabled": true, "no_avatar": {"enabled": true, "action": "Ban"}}});
        self.server_config(i, owner, cfg).await
    }

    /// Takes the queued `kind` jobs for user `u`, as (server, payload).
    async fn jobs_of(&self, kind: &str, u: Id<UserMarker>) -> Vec<(i64, Value)> {
        sqlx::query_as(
            "DELETE FROM scheduled_jobs WHERE kind = $1 AND payload->>'user' = $2 RETURNING guild_id, payload",
        )
        .bind(kind)
        .bind(u.get().to_string())
        .fetch_all(&self.app.pool)
        .await
        .unwrap()
    }

    async fn raider(&self, u: Id<UserMarker>) -> Option<(String, String, bool)> {
        sqlx::query_as("SELECT status, category, verified FROM raiders WHERE user_id = $1")
            .bind(u.get() as i64)
            .fetch_optional(&self.app.pool)
            .await
            .unwrap()
    }

    async fn list(&self, u: Id<UserMarker>, status: &str, cat: &str, expires: Option<&str>) {
        sqlx::query(
            "INSERT INTO raiders (user_id, status, category, expires_at, reviewed_at)
             VALUES ($1, $2, $3, now() + $4::interval, now())",
        )
        .bind(u.get() as i64)
        .bind(status)
        .bind(cat)
        .bind(expires)
        .execute(&self.app.pool)
        .await
        .unwrap();
    }

    async fn acted(&self, i: u64, u: Id<UserMarker>) -> Option<String> {
        sqlx::query_scalar("SELECT action FROM raider_actions WHERE guild_id = $1 AND user_id = $2")
            .bind(self.g(i).get() as i64)
            .bind(u.get() as i64)
            .fetch_optional(&self.app.pool)
            .await
            .unwrap()
    }

    async fn gate_reasons(&self, i: u64, u: Id<UserMarker>) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT reason FROM joingate_actions WHERE guild_id = $1 AND user_id = $2 ORDER BY id",
        )
        .bind(self.g(i).get() as i64)
        .bind(u.get() as i64)
        .fetch_all(&self.app.pool)
        .await
        .unwrap()
    }

    async fn join(&self, i: u64, u: Id<UserMarker>) {
        self.set(|m| {
            m.members.insert((self.g(i).get(), u.get()), vec![]);
        });
        let m: MemberAdd = serde_json::from_value(json!({
            "guild_id": self.g(i).to_string(), "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": u.to_string(), "username": "raider", "discriminator": "0", "avatar": null},
        }))
        .unwrap();
        crate::joingate::on_join(&self.app, &m).await.unwrap();
    }

    async fn forget(&self) {
        for sql in [
            "DELETE FROM raider_evidence WHERE user_id BETWEEN $1 * 10 AND $1 * 10 + 9",
            "DELETE FROM raider_reviews WHERE user_id BETWEEN $1 * 10 AND $1 * 10 + 9",
            "DELETE FROM raiders WHERE user_id BETWEEN $1 * 10 AND $1 * 10 + 9",
            "DELETE FROM scheduled_jobs WHERE guild_id = 0 AND (payload->>'user')::bigint BETWEEN $1 * 10 AND $1 * 10 + 9",
        ] {
            self.exec(sql).await;
        }
        self.cleanup().await;
    }
}

#[tokio::test]
async fn three_owners_list_by_themselves_and_one_owner_counts_once() {
    let Some(t) = T::new(50).await else { return };
    let (a, b, c) = (t.base + 7, t.base + 8, t.owner());
    let off = json!({});
    t.raider_list(0, a, off.clone()).await;
    t.raider_list(1, a, off.clone()).await;
    t.raider_list(2, b, off.clone()).await;
    t.raider_list(3, c, off.clone()).await;
    let u = t.u(1);
    let d = Detail::default();
    // One owner with two servers is one source.
    assert!(collect(&t.app, t.g(0), u, Kind::Nuke, "n1", d)
        .await
        .unwrap());
    assert!(collect(&t.app, t.g(1), u, Kind::Raid, "r1", d)
        .await
        .unwrap());
    assert!(
        !collect(&t.app, t.g(1), u, Kind::Raid, "r1", d)
            .await
            .unwrap(),
        "same row twice"
    );
    assert!(collect(&t.app, t.g(2), u, Kind::Raid, "r2", d)
        .await
        .unwrap());
    assert_eq!(t.raider(u).await.unwrap().0, "pending", "two owners");
    assert!(t.jobs_of("raider_check", u).await.is_empty());
    collect(&t.app, t.g(3), u, Kind::Spam, "s1", d)
        .await
        .unwrap();
    let (status, cat, verified) = t.raider(u).await.unwrap();
    assert_eq!(
        (status.as_str(), cat.as_str(), verified),
        ("confirmed", "raid", false)
    );
    let jobs = t.jobs_of("raider_check", u).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (jobs[0].0, &jobs[0].1["user"]),
        (0, &json!(u.get().to_string()))
    );
    assert!(
        t.calls("listed by itself") >= 1,
        "the bot owner got the evidence"
    );
    // Scam deletions and reports from three owners never list anyone by themselves.
    let (v, w) = (t.u(2), t.u(3));
    for i in [0, 2, 3] {
        collect(&t.app, t.g(i), v, Kind::Scam, "m", d)
            .await
            .unwrap();
        let owner = live_owner(&t.app, t.g(i)).await.unwrap().unwrap();
        let ev = ("report", "raid", "1", json!({}), Some(1));
        record(&t.app, (t.g(i), owner, w), ev).await.unwrap();
    }
    assert_eq!(t.raider(v).await.unwrap().0, "pending");
    assert_eq!(t.raider(w).await.unwrap().0, "pending");
    let expires: bool = sqlx::query_scalar(
        "SELECT expires_at > now() + interval '13 days' FROM raiders WHERE user_id = $1",
    )
    .bind(v.get() as i64)
    .fetch_one(&t.app.pool)
    .await
    .unwrap();
    assert!(expires, "a scam entry ends 14 days after its evidence");
    t.forget().await;
}

#[tokio::test]
async fn staff_the_bot_and_its_owner_are_never_collected() {
    let Some(t) = T::new(51).await else { return };
    let o = t.owner();
    t.raider_list(0, o, json!({})).await;
    t.raider_list(1, o, json!({"no_collect": true})).await;
    let (mod_, eo, plain) = (t.u(1), t.u(2), t.u(3));
    // A moderator that anti-nuke quarantined: their permit role is only in the saved roles.
    t.exec("INSERT INTO permits (id, guild_id, name, perms) VALUES ($1, $1, 'Mods', '{kick}')")
        .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $1, 555)")
        .await;
    sqlx::query(
        "INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, $2, '{555}')",
    )
    .bind(t.g(0).get() as i64)
    .bind(mod_.get() as i64)
    .execute(&t.app.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO extra_owners (guild_id, user_id) VALUES ($1, $2)")
        .bind(t.g(0).get() as i64)
        .bind(eo.get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    let d = Detail::default();
    for who in [mod_, eo, Id::new(ME), Id::new(BOT_OWNER), Id::new(o)] {
        assert!(
            !collect(&t.app, t.g(0), who, Kind::Nuke, "n", d)
                .await
                .unwrap(),
            "{who}"
        );
    }
    assert!(
        !collect(&t.app, t.g(1), plain, Kind::Nuke, "n", d)
            .await
            .unwrap(),
        "no_collect"
    );
    assert!(collect(&t.app, t.g(0), plain, Kind::Nuke, "n", d)
        .await
        .unwrap());
    assert!(t.raider(mod_).await.is_none() && t.raider(eo).await.is_none());
    // A failed member lookup is no answer: nothing is written and the caller sees the error.
    t.set(|m| m.fail_member = true);
    assert!(collect(&t.app, t.g(0), t.u(4), Kind::Nuke, "n", d)
        .await
        .is_err());
    t.set(|m| m.fail_member = false);
    t.forget().await;
}

#[tokio::test]
async fn only_confirmed_unexpired_entries_are_enforced_on_join_before_the_rest() {
    let Some(t) = T::new(52).await else { return };
    let o = t.owner();
    t.raider_list(0, o, json!({"enabled": true})).await;
    let (listed, pending, rejected, allowed, expired) = (t.u(1), t.u(2), t.u(3), t.u(4), t.u(5));
    t.list(listed, "confirmed", "raid", None).await;
    t.list(pending, "pending", "raid", None).await;
    t.list(rejected, "rejected", "raid", None).await;
    t.list(allowed, "confirmed", "raid", None).await;
    t.list(expired, "confirmed", "scam", Some("-1 hour")).await;
    sqlx::query("INSERT INTO raider_allow (guild_id, user_id, by) VALUES ($1, $2, 9)")
        .bind(t.g(0).get() as i64)
        .bind(allowed.get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    // The list bans first; the Join Gate's no-avatar filter does not punish a second time.
    t.join(0, listed).await;
    assert_eq!(t.gate_reasons(0, listed).await, ["Raider List: raid"]);
    assert_eq!(t.calls(&format!("PUT /guilds/{}/bans/{listed}", t.g(0))), 1);
    assert!(t.banned(0, listed.get()).unwrap().starts_with(PREFIX));
    assert_eq!(t.acted(0, listed).await.as_deref(), Some("ban"));
    // Everyone else goes on to the Join Gate as before.
    for u in [pending, rejected, allowed, expired] {
        t.join(0, u).await;
        assert_eq!(t.gate_reasons(0, u).await, ["join gate"], "{u}");
        assert_eq!(t.acted(0, u).await, None);
    }
    t.forget().await;
}

#[tokio::test]
async fn alerts_and_checks_happen_once_and_a_5xx_retries() {
    let Some(t) = T::new(53).await else { return };
    let o = t.owner();
    t.raider_list(0, o, json!({"enabled": true, "action": "alert"}))
        .await;
    t.raider_list(1, o, json!({"enabled": true, "categories": ["nuke"]}))
        .await;
    let u = t.u(1);
    t.list(u, "confirmed", "raid", None).await;
    t.join(0, u).await;
    assert_eq!(t.acted(0, u).await.as_deref(), Some("alert"));
    assert_eq!(
        t.gate_reasons(0, u).await,
        ["join gate"],
        "an alert lets the rest run"
    );
    // The check job: a 5xx retries, two runs alert once, other categories are left alone.
    t.exec("DELETE FROM raider_actions WHERE guild_id = $1")
        .await;
    let p = json!({"user": u.get().to_string()});
    t.set(|m| m.fail_member = true);
    assert!(check_job(&t.app, t.g(0), &p).await.is_err());
    t.set(|m| m.fail_member = false);
    check_job(&t.app, t.g(0), &p).await.unwrap();
    check_job(&t.app, t.g(0), &p).await.unwrap();
    assert_eq!(t.acted(0, u).await.as_deref(), Some("alert"));
    t.set(|m| {
        m.members.insert((t.g(1).get(), u.get()), vec![]);
    });
    check_job(&t.app, t.g(1), &p).await.unwrap();
    assert_eq!(t.acted(1, u).await, None, "raid is not in its categories");
    // The scan pages the members and alerts only for listed ones it has not alerted yet.
    t.exec("DELETE FROM raider_actions WHERE guild_id = $1")
        .await;
    t.set(|m| {
        for k in 2..6 {
            m.members.insert((t.g(0).get(), t.u(k).get()), vec![]);
        }
    });
    t.list(t.u(4), "confirmed", "spam", None).await;
    scan_job(&t.app, t.g(0)).await.unwrap();
    scan_job(&t.app, t.g(0)).await.unwrap();
    let rows = t
        .count("SELECT count(*) FROM raider_actions WHERE guild_id = $1")
        .await;
    assert_eq!(rows, 2);
    assert_eq!(t.acted(0, t.u(4)).await.as_deref(), Some("alert"));
    assert!(t.calls("/members?") >= 3, "paged");
    t.forget().await;
}

#[tokio::test]
async fn expiry_and_removal_lift_only_what_the_list_did() {
    let Some(t) = T::new(54).await else { return };
    let o = t.owner();
    t.raider_list(0, o, json!({"enabled": true})).await;
    t.raider_list(1, o, json!({"enabled": true})).await;
    t.raider_list(2, o, json!({"enabled": true, "lift_on_removal": false}))
        .await;
    let u = t.u(1);
    t.list(u, "confirmed", "scam", Some("-1 minute")).await;
    for i in 0..3 {
        sqlx::query(
            "INSERT INTO raider_actions (guild_id, user_id, action) VALUES ($1, $2, 'ban')",
        )
        .bind(t.g(i).get() as i64)
        .bind(u.get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    }
    t.ban(0, u.get(), "Raider List: scam | Moderator: 1");
    t.ban(1, u.get(), "spam, banned again by hand");
    t.ban(2, u.get(), "Raider List: scam");
    expire(&t.app).await.unwrap();
    assert_eq!(t.raider(u).await.unwrap().0, "removed");
    let fan = t.jobs_of("raider_lift", u).await;
    assert_eq!(fan.len(), 1);
    let job = |g: i64, p: &Value| jobs::Job {
        id: 0,
        guild_id: g,
        kind: "raider_lift".into(),
        payload: p.clone(),
        attempts: 1,
    };
    fan_out(&t.app, &job(0, &fan[0].1)).await.unwrap();
    let mut lifts = t.jobs_of("raider_lift", u).await;
    lifts.sort_by_key(|j| j.0);
    assert_eq!(lifts.len(), 3);
    // A Discord outage retries and keeps the row.
    t.set(|m| m.fail_get = true);
    assert!(lift_job(&t.app, t.g(0), &lifts[0].1).await.is_err());
    assert_eq!(t.acted(0, u).await.as_deref(), Some("ban"));
    t.set(|m| m.fail_get = false);
    for (g, p) in lifts.iter().chain(lifts.iter()) {
        lift_job(&t.app, Id::new(*g as u64), p).await.unwrap();
    }
    assert_eq!(t.banned(0, u.get()), None, "the list's ban is lifted");
    assert!(t.banned(1, u.get()).is_some(), "a ban made by hand stays");
    assert!(t.banned(2, u.get()).is_some(), "the server keeps it");
    let unbans = t
        .count("SELECT count(*) FROM cases WHERE guild_id = $1 AND kind = 'unban'")
        .await;
    assert_eq!(unbans, 1, "two runs, one case");
    for i in 0..3 {
        assert_eq!(t.acted(i, u).await, None);
    }
    // A confirmed entry is never lifted.
    let v = t.u(2);
    t.list(v, "confirmed", "raid", None).await;
    sqlx::query("INSERT INTO raider_actions (guild_id, user_id, action) VALUES ($1, $2, 'ban')")
        .bind(t.g(0).get() as i64)
        .bind(v.get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.ban(0, v.get(), "Raider List: raid");
    lift_job(&t.app, t.g(0), &json!({"user": v.get().to_string()}))
        .await
        .unwrap();
    assert!(t.banned(0, v.get()).is_some());
    t.forget().await;
}

#[tokio::test]
async fn old_rejected_and_unreviewed_entries_go_with_their_evidence_but_reviews_stay() {
    let Some(t) = T::new(55).await else { return };
    let (old_rej, old_pend, new_rej) = (t.u(1), t.u(2), t.u(3));
    t.list(old_rej, "rejected", "raid", None).await;
    t.list(new_rej, "rejected", "raid", None).await;
    sqlx::query("INSERT INTO raiders (user_id, status, category, updated_at) VALUES ($1, 'pending', 'spam', now() - interval '91 days')")
        .bind(old_pend.get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE raiders SET reviewed_at = now() - interval '31 days' WHERE user_id = $1")
        .bind(old_rej.get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    for u in [old_rej, old_pend, new_rej] {
        sqlx::query("INSERT INTO raider_evidence (user_id, source_guild, source_owner, kind, ref) VALUES ($1, 1, 1, 'auto_raid', 'x')")
            .bind(u.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO raider_reviews (actor, user_id, to_status) VALUES (1, $1, 'rejected')",
        )
        .bind(u.get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    }
    expire(&t.app).await.unwrap();
    assert!(t.raider(old_rej).await.is_none() && t.raider(old_pend).await.is_none());
    assert!(t.raider(new_rej).await.is_some());
    let left = t
        .count("SELECT count(*) FROM raider_evidence WHERE user_id BETWEEN $1 * 10 AND $1 * 10 + 9")
        .await;
    let reviews = t
        .count("SELECT count(*) FROM raider_reviews WHERE user_id BETWEEN $1 * 10 AND $1 * 10 + 9")
        .await;
    assert_eq!((left, reviews), (1, 3));
    t.forget().await;
}

#[tokio::test]
async fn staff_lifting_a_list_action_allows_the_user_and_purge_keeps_the_list() {
    let Some(t) = T::new(56).await else { return };
    let o = t.owner();
    t.raider_list(0, o, json!({"enabled": true})).await;
    let (u, v) = (t.u(1), t.u(2));
    t.list(u, "confirmed", "raid", None).await;
    t.join(0, u).await;
    // The bot's own unban (a lift) is no staff choice; Discord's unban button by staff is.
    let entry = |actor: u64| -> AuditLogEntry {
        serde_json::from_value(json!({
            "id": "1300000000000000000", "action_type": 23, "changes": [], "guild_id": t.g(0).to_string(),
            "target_id": u.to_string(), "user_id": actor.to_string(),
        }))
        .unwrap()
    };
    on_unban(&t.app, &entry(ME)).await.unwrap();
    assert!(!exempt(&t.app, t.g(0), u, Some(&[][..])).await.unwrap());
    on_unban(&t.app, &entry(77)).await.unwrap();
    assert!(exempt(&t.app, t.g(0), u, Some(&[][..])).await.unwrap());
    // A user the list never touched here gets no allow row.
    lifted(&t.app, t.g(0), v, Id::new(77)).await;
    let allows = t
        .count("SELECT count(*) FROM raider_allow WHERE guild_id = $1")
        .await;
    assert_eq!(allows, 1);
    tommybot_core::db::purge_guild(&t.app.pool, t.g(0).get() as i64)
        .await
        .unwrap();
    let here = t
        .count("SELECT (SELECT count(*) FROM raider_actions WHERE guild_id = $1) + (SELECT count(*) FROM raider_allow WHERE guild_id = $1)")
        .await;
    assert_eq!(here, 0);
    assert!(t.raider(u).await.is_some(), "the global list stays");
    t.forget().await;
}

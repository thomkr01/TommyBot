//! Raider List writes shared by the bot and the dashboard, so a report or a piece of evidence gets
//! the same checks whichever process takes it.
use anyhow::Result;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};

use crate::{
    limits::{RAIDER_MIN_DAYS, RAIDER_MIN_MEMBERS, RAIDER_REPORTS_DAY, RAIDER_SCAM_DAYS},
    permits::{self, Level},
};

/// Case kinds a report can stand on.
pub const REPORTABLE: [&str; 5] = ["ban", "tempban", "softban", "kick", "quarantine"];
/// Kinds that make no entry until a second owner flagged the user: one owner's raid detector or
/// spam crowd alone is too common to put in the review queue.
const NEEDS_TWO: [&str; 2] = ["auto_raid", "auto_spam"];
/// The longest report reason; it is kept for the bot owner only.
pub const NOTE_MAX: usize = 400;

/// Evidence about `user` may not come from `guild`: the server opted out, or the user is its owner,
/// an extra owner, a trusted admin or holds a permit. A contained staff member is most likely a
/// hacked account, and quarantine took their roles, so the roles it saved count too.
pub async fn not_collectable(
    pool: &PgPool,
    guild: i64,
    owner: i64,
    user: i64,
    roles: &[i64],
) -> Result<bool> {
    let opted_out: Option<bool> = sqlx::query_scalar(
        "SELECT (config->'raider_list'->>'no_collect')::boolean FROM guild_config
         WHERE guild_id = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(guild)
    .fetch_optional(pool)
    .await?
    .flatten();
    if opted_out == Some(true) {
        return Ok(true);
    }
    let saved: Option<Vec<i64>> = sqlx::query_scalar(
        "SELECT saved_roles FROM quarantines WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(guild)
    .bind(user)
    .fetch_optional(pool)
    .await?;
    let all: Vec<i64> = roles
        .iter()
        .copied()
        .chain(saved.unwrap_or_default())
        .collect();
    Ok(permits::resolve(pool, guild, owner, user, &all).await? != Level::None)
}

/// One evidence row.
pub struct Evidence<'a> {
    pub user: i64,
    pub guild: i64,
    pub owner: i64,
    pub kind: &'a str,
    pub category: &'a str,
    pub r#ref: &'a str,
    pub detail: Value,
    pub reporter: Option<i64>,
    /// A reporter's own words, for the bot owner only.
    pub note: Option<&'a str>,
}

/// Takes the user's lock until the transaction ends, so one user's evidence and listing go one at a time.
pub async fn lock_user(tx: &mut PgConnection, user: i64) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('raider_user:' || $1, 0))")
        .bind(user)
        .execute(tx)
        .await?;
    Ok(())
}

/// Writes the evidence and its entry. New evidence makes the entry pending again when it was
/// rejected or removed, and a scam entry lasts 14 days after its last scam evidence. Raid and spam
/// detections make no entry until a second owner flagged the user. True when the evidence is new.
pub async fn add(tx: &mut PgConnection, e: &Evidence<'_>) -> Result<bool> {
    // One user's evidence is written one at a time, so two owners' evidence in parallel both see
    // each other in the owner count.
    lock_user(tx, e.user).await?;
    let fresh = sqlx::query(
        "INSERT INTO raider_evidence (user_id, source_guild, source_owner, kind, ref, detail, reporter_id, note)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
    )
    .bind(e.user)
    .bind(e.guild)
    .bind(e.owner)
    .bind(e.kind)
    .bind(e.r#ref)
    .bind(&e.detail)
    .bind(e.reporter)
    .bind(e.note)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !fresh {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO raiders (user_id, status, category, expires_at)
         SELECT $1, 'pending', $2, CASE WHEN $2 = 'scam' THEN now() + make_interval(days => $3) END
         WHERE NOT ($4 = ANY($5))
            OR (SELECT count(DISTINCT source_owner) FROM raider_evidence WHERE user_id = $1) >= 2
         ON CONFLICT (user_id) DO UPDATE SET
             verified = raiders.verified AND raiders.status NOT IN ('rejected', 'removed'),
             status = CASE WHEN raiders.status IN ('rejected', 'removed') THEN 'pending' ELSE raiders.status END,
             expires_at = CASE WHEN raiders.category = 'scam' AND $2 = 'scam'
                               THEN now() + make_interval(days => $3) ELSE raiders.expires_at END,
             updated_at = now()",
    )
    .bind(e.user)
    .bind(e.category)
    .bind(RAIDER_SCAM_DAYS as i32)
    .bind(e.kind)
    .bind(NEEDS_TWO.as_slice())
    .execute(&mut *tx)
    .await?;
    Ok(true)
}

/// Why a report from this server is refused, if it is.
pub fn report_refusal(case: bool, members: u64, days: i64, today: i64) -> Option<String> {
    if !case {
        return Some(
            "Only users with a ban, kick or quarantine case in this server can be reported.".into(),
        );
    }
    if members < RAIDER_MIN_MEMBERS || days < RAIDER_MIN_DAYS {
        return Some(format!("A server needs {RAIDER_MIN_MEMBERS} members and {RAIDER_MIN_DAYS} days with TommyBot to report to the Raider List."));
    }
    if today >= RAIDER_REPORTS_DAY {
        return Some(format!("The owner of this server already sent {RAIDER_REPORTS_DAY} reports in the last 24 hours, across all their servers. Try again tomorrow."));
    }
    None
}

/// A report as both the `t!raider report` command and the dashboard's case button send it. The
/// caller looked up what only Discord knows: the owner, the member count, when the bot joined,
/// and the member's roles (empty when they are not in the server).
pub struct Report<'a> {
    pub guild: i64,
    pub owner: i64,
    pub user: i64,
    pub reporter: i64,
    /// (case number, case kind) of the user's case here.
    pub case: Option<(i32, &'a str)>,
    pub category: &'a str,
    pub reason: Option<&'a str>,
    pub members: u64,
    pub days_with_bot: i64,
    pub roles: &'a [i64],
    /// The bot and the bot owner, never reported.
    pub protected: [i64; 2],
}

/// Every check of a report, then the evidence, under a per-owner lock so parallel reports cannot
/// pass the daily cap. `Err` text is for the reporter. A report never lists anyone by itself.
pub async fn report(pool: &PgPool, r: &Report<'_>) -> Result<Result<(), String>> {
    let case = r.case.filter(|c| REPORTABLE.contains(&c.1));
    if let Some(no) = report_refusal(case.is_some(), r.members, r.days_with_bot, 0) {
        return Ok(Err(no));
    }
    if r.protected.contains(&r.user)
        || not_collectable(pool, r.guild, r.owner, r.user, r.roles).await?
    {
        return Ok(Err("The Raider List does not take reports about this account from this server (its staff, an opted-out server, TommyBot or its owner).".into()));
    }
    let reason = r.reason.map(str::trim).filter(|t| !t.is_empty());
    if reason.is_some_and(|t| t.chars().count() > NOTE_MAX) {
        return Ok(Err(format!(
            "The reason is longer than {NOTE_MAX} characters."
        )));
    }
    let (no, kind) = case.unwrap_or_default();
    let mut tx = pool.begin().await?;
    // Its own key space: a raw id would share locks with the user lock in `add`, so two owners
    // reporting each other at once would deadlock.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('raider_owner:' || $1, 0))")
        .bind(r.owner)
        .execute(&mut *tx)
        .await?;
    let today: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM raider_evidence WHERE source_owner = $1 AND kind = 'report'
           AND created_at > now() - interval '1 day'",
    )
    .bind(r.owner)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(no) = report_refusal(true, r.members, r.days_with_bot, today) {
        return Ok(Err(no));
    }
    let e = Evidence {
        user: r.user,
        guild: r.guild,
        owner: r.owner,
        kind: "report",
        category: r.category,
        r#ref: &no.to_string(),
        detail: serde_json::json!({"case_no": no, "case_kind": kind}),
        reporter: Some(r.reporter),
        note: reason,
    };
    if !add(&mut tx, &e).await? {
        return Ok(Err(format!("Case {no} was already reported.")));
    }
    tx.commit().await?;
    Ok(Ok(()))
}

/// Staff lifted what the list did in this server (unban, unquarantine, Allow): the list never
/// acts on the user here again, and its action row goes, so a later removal lifts nothing staff
/// redid. False when the list had done nothing here.
pub async fn allow(db: impl sqlx::PgExecutor<'_>, guild: i64, user: i64, by: i64) -> Result<bool> {
    Ok(sqlx::query(
        "WITH a AS (DELETE FROM raider_actions WHERE guild_id = $1 AND user_id = $2 RETURNING 1)
         INSERT INTO raider_allow (guild_id, user_id, by) SELECT $1, $2, $3 FROM a
         ON CONFLICT (guild_id, user_id) DO UPDATE SET by = excluded.by, at = now()",
    )
    .bind(guild)
    .bind(user)
    .bind(by)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn reports_check_staff_opt_out_cap_and_bring_rejected_entries_back() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = crate::db::connect(&url).await.unwrap();
        let g = 3_311_000_000_000 + std::process::id() as i64 * 10;
        let (owner, user) = (g + 1, g * 10);
        let users = (user, user + 9);
        let clean = |pool: PgPool| async move {
            for sql in [
                "DELETE FROM raider_evidence WHERE user_id BETWEEN $1 AND $2 OR source_owner = $3",
                "DELETE FROM raiders WHERE user_id BETWEEN $1 AND $2",
                "DELETE FROM permits WHERE guild_id = $3 - 1",
                "DELETE FROM quarantines WHERE guild_id = $3 - 1",
                "DELETE FROM guild_config WHERE guild_id BETWEEN $3 - 1 AND $3",
            ] {
                sqlx::query(sql)
                    .bind(users.0)
                    .bind(users.1)
                    .bind(owner)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        clean(pool.clone()).await;
        let base = |u: i64, guild: i64| Report {
            guild,
            owner,
            user: u,
            reporter: owner,
            case: Some((7, "ban")),
            category: "raid",
            reason: Some("raided <@1> **twice**"),
            members: 500,
            days_with_bot: 30,
            roles: &[],
            protected: [1, 2],
        };
        // A moderator contained by anti-nuke: the permit role is only in the saved roles.
        sqlx::query(
            "INSERT INTO permits (id, guild_id, name, perms) VALUES ($1, $1, 'Mods', '{kick}')",
        )
        .bind(g)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $1, 555)")
            .bind(g)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, $2, '{555}')",
        )
        .bind(g)
        .bind(user + 1)
        .execute(&pool)
        .await
        .unwrap();
        assert!(report(&pool, &base(user + 1, g)).await.unwrap().is_err());
        assert!(
            report(&pool, &base(1, g)).await.unwrap().is_err(),
            "the bot"
        );
        sqlx::query("INSERT INTO guild_config (guild_id, version, config) VALUES ($1, 1, '{\"raider_list\": {\"no_collect\": true}}')")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            report(&pool, &base(user, owner)).await.unwrap().is_err(),
            "opted out"
        );
        let mut no_case = base(user, g);
        no_case.case = Some((7, "warn"));
        assert!(report(&pool, &no_case).await.unwrap().is_err());
        // A rejected entry comes back to the queue; the reason is kept raw in its own column.
        sqlx::query("INSERT INTO raiders (user_id, status, category, verified) VALUES ($1, 'rejected', 'raid', true)")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        report(&pool, &base(user, g)).await.unwrap().unwrap();
        let (status, verified, note): (String, bool, Option<String>) = sqlx::query_as(
            "SELECT r.status, r.verified, e.note FROM raiders r JOIN raider_evidence e USING (user_id) WHERE r.user_id = $1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (status.as_str(), verified, note.as_deref()),
            ("pending", false, Some("raided <@1> **twice**"))
        );
        assert!(
            report(&pool, &base(user, g)).await.unwrap().is_err(),
            "same case twice"
        );
        // The daily cap counts this owner's reports from every server.
        for k in 2..=9 {
            let mut r = base(user + k, g);
            r.case = Some((k as i32, "kick"));
            report(&pool, &r).await.unwrap().unwrap();
        }
        let mut r = base(user + 9, g);
        r.case = Some((99, "kick"));
        report(&pool, &r).await.unwrap().unwrap();
        r.case = Some((100, "kick"));
        assert!(report(&pool, &r)
            .await
            .unwrap()
            .unwrap_err()
            .contains("10 reports"));
        // One owner's raid detection alone makes no entry; a second owner's does.
        let lone = user + 3;
        sqlx::query("DELETE FROM raiders WHERE user_id = $1")
            .bind(lone)
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let raid = |owner| Evidence {
            user: lone,
            guild: g,
            owner,
            kind: "auto_raid",
            category: "raid",
            r#ref: "x",
            detail: Value::Null,
            reporter: None,
            note: None,
        };
        add(&mut tx, &raid(owner)).await.unwrap();
        tx.commit().await.unwrap();
        let entry = |u| {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM raiders WHERE user_id = $1").bind(u)
        };
        assert_eq!(entry(lone).fetch_one(&pool).await.unwrap(), 0);
        let mut tx = pool.begin().await.unwrap();
        let mut second = raid(owner + 1);
        second.guild = g + 1;
        add(&mut tx, &second).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(entry(lone).fetch_one(&pool).await.unwrap(), 1);
        // Allow drops what the list did here, so a later removal lifts nothing staff redid.
        sqlx::query(
            "INSERT INTO raider_actions (guild_id, user_id, action) VALUES ($1, $2, 'ban')",
        )
        .bind(g)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
        assert!(allow(&pool, g, user, owner).await.unwrap());
        assert!(
            !allow(&pool, g, user, owner).await.unwrap(),
            "nothing left to allow"
        );
        let left: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM raider_actions WHERE guild_id = $1),
                    (SELECT count(*) FROM raider_allow WHERE guild_id = $1 AND user_id = $2)",
        )
        .bind(g)
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(left, (0, 1));
        sqlx::query("DELETE FROM raider_allow WHERE guild_id = $1")
            .bind(g)
            .execute(&pool)
            .await
            .unwrap();
        clean(pool.clone()).await;
    }
}

//! The read-only tools. Discord ids go out as strings: they do not fit in a JSON number.
use crate::State;
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;

pub fn list() -> Value {
    let guild = json!({"type": "string", "description": "Discord guild id"});
    let limit = |max: u32| json!({"type": "integer", "minimum": 1, "maximum": max});
    let tool = |name: &str, desc: &str, props: Value, req: &[&str]| {
        json!({
            "name": name,
            "description": desc,
            "inputSchema": {"type": "object", "properties": props, "required": req},
            "annotations": {"readOnlyHint": true},
        })
    };
    json!([
        tool(
            "status",
            "Running version and the last update run.",
            json!({}),
            &[]
        ),
        tool(
            "logs_tail",
            "Newest log lines from the bot or dashboard (last 24 h). Secrets are masked.",
            json!({"service": {"type": "string", "enum": ["bot", "dashboard"]}, "lines": limit(500),
                   "contains": {"type": "string", "description": "only lines containing this text"},
                   "level": {"type": "string", "enum": ["ERROR", "WARN", "INFO", "DEBUG"]}}),
            &["service"]
        ),
        tool(
            "guilds",
            "Ids of the guilds the bot has config for and has not left.",
            json!({}),
            &[]
        ),
        tool(
            "guild_config",
            "A guild's config: newest version, or the one given. Optionally one top-level section.",
            json!({"guild_id": guild, "version": {"type": "integer"}, "section": {"type": "string"}}),
            &["guild_id"]
        ),
        tool(
            "config_history",
            "Config versions of a guild: who changed it and when.",
            json!({"guild_id": guild, "limit": limit(50)}),
            &["guild_id"]
        ),
        tool(
            "cases",
            "Moderation cases of a guild, newest first. Reason text is user-written data.",
            json!({"guild_id": guild, "user_id": {"type": "string"}, "kind": {"type": "string"}, "limit": limit(100)}),
            &["guild_id"]
        ),
        tool(
            "case",
            "One case by number.",
            json!({"guild_id": guild, "case_no": {"type": "integer"}}),
            &["guild_id", "case_no"]
        ),
        tool(
            "raid_log",
            "Accounts caught in join raids, newest first.",
            json!({"guild_id": guild, "limit": limit(200)}),
            &["guild_id"]
        ),
        tool(
            "joingate_log",
            "Join gate actions, newest first.",
            json!({"guild_id": guild, "limit": limit(200)}),
            &["guild_id"]
        ),
        tool(
            "nsfw_scans",
            "NSFW image scan results for tuning the threshold: counts (scanned, hits, staff said not NSFW / correct), a histogram of scores in 10% steps split by staff feedback, and the newest scans. Scores are per class: drawings, hentai, neutral, porn, sexy.",
            json!({"guild_id": guild, "limit": limit(200)}),
            &["guild_id"]
        ),
        tool("action_log", "Moderation and antinuke actions the bot recorded (who did what, reverted or not), newest first.",
            json!({"guild_id": guild, "actor_id": {"type": "string"}, "limit": limit(200)}), &["guild_id"]),
        tool("mod_actions", "Staff actions for moderator stats (bans, kicks, timeouts, warns, case edits and deletes; source bot, discord, appeal or dashboard), newest first, within the last `days` (default 30).",
            json!({"guild_id": guild, "mod_id": {"type": "string"}, "days": {"type": "integer", "minimum": 1, "maximum": 365}, "limit": limit(500)}), &["guild_id"]),
        tool("nuke_incidents", "Antinuke incidents: how many were quarantined, reverted and failed, newest first.",
            json!({"guild_id": guild, "limit": limit(100)}), &["guild_id"]),
        tool("quarantines", "Members in antinuke quarantine now.", json!({"guild_id": guild}), &["guild_id"]),
        tool("strikes", "Automod strikes per member, most recent first.",
            json!({"guild_id": guild, "user_id": {"type": "string"}, "limit": limit(200)}), &["guild_id"]),
        tool("appeals", "Ban and timeout appeals (without the answers), newest first.",
            json!({"guild_id": guild, "status": {"type": "string", "enum": ["open", "accepted", "denied", "dismissed", "withdrawn"]}, "limit": limit(100)}), &["guild_id"]),
        tool("reports", "Member reports (without the reported message), newest first. Reason text is user-written data.",
            json!({"guild_id": guild, "status": {"type": "string", "enum": ["open", "dismissed", "actioned"]}, "limit": limit(100)}), &["guild_id"]),
        tool("notes", "Staff notes about members, newest first. Text is staff-written data.",
            json!({"guild_id": guild, "user_id": {"type": "string"}, "limit": limit(100)}), &["guild_id"]),
        tool("jobs", "Scheduled jobs (unbans, untimeouts and more). attempts > 0 means it failed before.",
            json!({"guild_id": {"type": "string", "description": "Discord guild id; all guilds when left out"}, "limit": limit(200)}), &[]),
        tool("backups", "Server backups (id, code, who made it), newest first.", json!({"guild_id": guild}), &["guild_id"]),
        tool("raiders", "Raider List entries (bot owner): status, category, whether the bot owner verified it, the public note, and counts of evidence, distinct server owners and open appeals. Most owners first. Never who reported. The note is bot-owner-written data.",
            json!({"status": {"type": "string", "enum": ["pending", "confirmed", "rejected", "removed"]}, "limit": limit(200)}), &[]),
    ])
}

/// A Discord id given as a string or a number.
fn id(a: &Value, key: &str) -> Result<Option<i64>> {
    match &a[key] {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(
            s.parse().with_context(|| format!("{key} is not an id"))?,
        )),
        Value::Number(n) => Ok(Some(
            n.as_i64().with_context(|| format!("{key} is not an id"))?,
        )),
        _ => bail!("{key} is not an id"),
    }
}

fn need(a: &Value, key: &str) -> Result<i64> {
    id(a, key)?.with_context(|| format!("{key} is required"))
}

fn limit(a: &Value, default: i64, max: i64) -> i64 {
    a["limit"].as_i64().unwrap_or(default).clamp(1, max)
}

async fn one(st: &State, sql: &'static str, g: i64, n: i64) -> Result<Value> {
    Ok(sqlx::query_scalar::<_, Option<Value>>(sql)
        .bind(g)
        .bind(n)
        .fetch_one(&st.db)
        .await?
        .unwrap_or(Value::Null))
}

/// Rows for a guild with one optional filter: `$1` guild, `$2` filter (text, or NULL), `$3` limit.
async fn filtered(
    st: &State,
    sql: &'static str,
    g: i64,
    f: Option<String>,
    n: i64,
) -> Result<Value> {
    Ok(sqlx::query_scalar::<_, Option<Value>>(sql)
        .bind(g)
        .bind(f)
        .bind(n)
        .fetch_one(&st.db)
        .await?
        .unwrap_or(json!([])))
}

fn text(a: &Value, key: &str) -> Option<String> {
    a[key].as_str().map(String::from)
}

pub async fn call(st: &State, name: &str, a: &Value) -> Result<Value> {
    Ok(match name {
        "status" => {
            let run = sqlx::query_scalar::<_, Option<Value>>(
                "SELECT to_jsonb(r) FROM mcp_update_runs r ORDER BY id DESC LIMIT 1",
            )
            .fetch_one(&st.db)
            .await
            .ok()
            .flatten();
            json!({"version": std::env::var("TOMMYBOT_SHA").ok(), "last_update": run})
        }
        "logs_tail" => {
            let svc = a["service"].as_str().unwrap_or_default();
            if !matches!(svc, "bot" | "dashboard") {
                bail!("service must be bot or dashboard");
            }
            let lines = a["lines"].as_u64().unwrap_or(100).clamp(1, 500) as usize;
            json!(tail(&st.log_dir, svc, lines, a["contains"].as_str(), a["level"].as_str())?)
        }
        "guilds" => sqlx::query_scalar::<_, Option<Value>>(
            "SELECT jsonb_agg(guild_id::text ORDER BY guild_id) FROM mcp_guilds",
        )
        .fetch_one(&st.db)
        .await?
        .unwrap_or(json!([])),
        "guild_config" => {
            let g = need(a, "guild_id")?;
            let v = a["version"].as_i64().unwrap_or(-1);
            let row = one(st,
                "SELECT jsonb_build_object('version', version, 'changed_by', changed_by::text, 'created_at', created_at, 'config', config)
                 FROM mcp_guild_config WHERE guild_id = $1 AND ($2 < 0 OR version = $2) ORDER BY version DESC LIMIT 1",
                g, v).await?;
            match a["section"].as_str() {
                Some(s) if !row.is_null() => json!({"version": row["version"], "section": s, "config": row["config"][s]}),
                _ => row,
            }
        }
        "config_history" => one(st,
            "SELECT jsonb_agg(jsonb_build_object('version', version, 'changed_by', changed_by::text, 'created_at', created_at) ORDER BY version DESC)
             FROM (SELECT * FROM mcp_guild_config WHERE guild_id = $1 ORDER BY version DESC LIMIT $2) h",
            need(a, "guild_id")?, limit(a, 20, 50)).await?,
        "cases" => sqlx::query_scalar::<_, Option<Value>>(
            "SELECT jsonb_agg(c ORDER BY case_no DESC) FROM (
                SELECT case_no, kind, user_id::text, mod_id::text, reason, proof, points, duration_s, expires_at, active, created_at
                FROM mcp_cases WHERE guild_id = $1 AND ($2::bigint IS NULL OR user_id = $2) AND ($3::text IS NULL OR kind = $3)
                ORDER BY case_no DESC LIMIT $4) c",
        )
        .bind(need(a, "guild_id")?)
        .bind(id(a, "user_id")?)
        .bind(a["kind"].as_str())
        .bind(limit(a, 25, 100))
        .fetch_one(&st.db)
        .await?
        .unwrap_or(json!([])),
        "case" => one(st,
            "SELECT jsonb_build_object('case_no', case_no, 'kind', kind, 'user_id', user_id::text, 'mod_id', mod_id::text, 'reason', reason,
                'proof', proof, 'points', points, 'duration_s', duration_s, 'expires_at', expires_at, 'active', active, 'created_at', created_at)
             FROM mcp_cases WHERE guild_id = $1 AND case_no = $2",
            need(a, "guild_id")?, a["case_no"].as_i64().context("case_no is required")?).await?,
        "raid_log" => one(st,
            "SELECT jsonb_agg(r ORDER BY created_at DESC) FROM (
                SELECT raid_ms, user_id::text, name, code, created_at FROM mcp_raid_log WHERE guild_id = $1 ORDER BY created_at DESC LIMIT $2) r",
            need(a, "guild_id")?, limit(a, 50, 200)).await?,
        "nsfw_scans" => one(st,
            "SELECT jsonb_build_object(
                'scanned', count(*), 'hits', count(*) FILTER (WHERE hit),
                'not_nsfw', count(*) FILTER (WHERE feedback = 'fp'), 'correct', count(*) FILTER (WHERE feedback = 'ok'),
                'histogram', (SELECT jsonb_agg(h ORDER BY bucket) FROM (
                    SELECT least(floor(score * 10)::int, 9) * 10 AS bucket, count(*) AS scans,
                        count(*) FILTER (WHERE feedback = 'fp') AS not_nsfw, count(*) FILTER (WHERE feedback = 'ok') AS correct
                    FROM mcp_nsfw_scans WHERE guild_id = $1 GROUP BY 1) h),
                'recent', (SELECT jsonb_agg(r ORDER BY at DESC) FROM (
                    SELECT id, channel_id::text, message_id::text, user_id::text, scores, score, threshold, hit, feedback, at
                    FROM mcp_nsfw_scans WHERE guild_id = $1 ORDER BY at DESC LIMIT $2) r))
             FROM mcp_nsfw_scans WHERE guild_id = $1",
            need(a, "guild_id")?, limit(a, 50, 200)).await?,
        "joingate_log" => one(st,
            "SELECT jsonb_agg(j ORDER BY created_at DESC) FROM (
                SELECT user_id::text, is_bot, adder_id::text, filters, reason, action, ok, error, created_at
                FROM mcp_joingate_log WHERE guild_id = $1 ORDER BY created_at DESC LIMIT $2) j",
            need(a, "guild_id")?, limit(a, 50, 200)).await?,
        "action_log" => filtered(st,
            "SELECT jsonb_agg(r ORDER BY id DESC) FROM (
                SELECT id, actor_id::text, action, target_id::text, case_no, reverted, created_at
                FROM mcp_action_log WHERE guild_id = $1 AND ($2::text IS NULL OR actor_id = $2::bigint) ORDER BY id DESC LIMIT $3) r",
            need(a, "guild_id")?, id(a, "actor_id")?.map(|i| i.to_string()), limit(a, 50, 200)).await?,
        "mod_actions" => sqlx::query_scalar::<_, Option<Value>>(
            "SELECT jsonb_agg(r ORDER BY id DESC) FROM (
                SELECT id, mod_id::text, kind, source, target_id::text, case_no, created_at FROM mcp_mod_actions
                WHERE guild_id = $1 AND ($2::bigint IS NULL OR mod_id = $2) AND created_at >= now() - make_interval(days => $3)
                ORDER BY id DESC LIMIT $4) r",
        )
        .bind(need(a, "guild_id")?)
        .bind(id(a, "mod_id")?)
        .bind(a["days"].as_i64().unwrap_or(30).clamp(1, 365) as i32)
        .bind(limit(a, 100, 500))
        .fetch_one(&st.db)
        .await?
        .unwrap_or(json!([])),
        "nuke_incidents" => one(st,
            "SELECT jsonb_agg(r ORDER BY created_at DESC) FROM (
                SELECT case_id, quarantined, reverted, failed, created_at FROM mcp_nuke_incidents WHERE guild_id = $1 ORDER BY created_at DESC LIMIT $2) r",
            need(a, "guild_id")?, limit(a, 20, 100)).await?,
        "quarantines" => one(st,
            "SELECT jsonb_agg(r ORDER BY created_at DESC) FROM (
                SELECT user_id::text, saved_roles, created_at, on_join FROM mcp_quarantines WHERE guild_id = $1 ORDER BY created_at DESC LIMIT $2) r",
            need(a, "guild_id")?, 500).await?,
        "strikes" => filtered(st,
            "SELECT jsonb_agg(r ORDER BY last_strike DESC) FROM (
                SELECT user_id::text, strikes, to_timestamp(last_strike_ms / 1000.0) AS last_strike, to_timestamp(cap_end_ms / 1000.0) AS cap_end
                FROM mcp_strikes WHERE guild_id = $1 AND ($2::text IS NULL OR user_id = $2::bigint) ORDER BY last_strike_ms DESC LIMIT $3) r",
            need(a, "guild_id")?, id(a, "user_id")?.map(|i| i.to_string()), limit(a, 50, 200)).await?,
        "appeals" => filtered(st,
            "SELECT jsonb_agg(r ORDER BY id DESC) FROM (
                SELECT id, user_id::text, kind, status, decided_by::text, created_at
                FROM mcp_appeals WHERE guild_id = $1 AND ($2::text IS NULL OR status = $2) ORDER BY id DESC LIMIT $3) r",
            need(a, "guild_id")?, text(a, "status"), limit(a, 25, 100)).await?,
        "reports" => filtered(st,
            "SELECT jsonb_agg(r ORDER BY id DESC) FROM (
                SELECT id, reporter_id::text, target_id::text, channel_id::text, reason, status, action, handled_by::text, handled_at, created_at
                FROM mcp_reports WHERE guild_id = $1 AND ($2::text IS NULL OR status = $2) ORDER BY id DESC LIMIT $3) r",
            need(a, "guild_id")?, text(a, "status"), limit(a, 25, 100)).await?,
        "notes" => filtered(st,
            "SELECT jsonb_agg(r ORDER BY id DESC) FROM (
                SELECT id, short_id, user_id::text, author_id::text, text, created_at
                FROM mcp_notes WHERE guild_id = $1 AND ($2::text IS NULL OR user_id = $2::bigint) ORDER BY id DESC LIMIT $3) r",
            need(a, "guild_id")?, id(a, "user_id")?.map(|i| i.to_string()), limit(a, 25, 100)).await?,
        "jobs" => sqlx::query_scalar::<_, Option<Value>>(
            "SELECT jsonb_agg(r ORDER BY run_at) FROM (
                SELECT id, guild_id::text, kind, run_at, attempts, locked_until
                FROM mcp_jobs WHERE ($1::bigint IS NULL OR guild_id = $1) ORDER BY run_at LIMIT $2) r",
        )
        .bind(id(a, "guild_id")?)
        .bind(limit(a, 50, 200))
        .fetch_one(&st.db)
        .await?
        .unwrap_or(json!([])),
        "backups" => one(st,
            "SELECT jsonb_agg(r ORDER BY created_at DESC) FROM (
                SELECT id, code, created_by::text, created_at FROM mcp_backups WHERE guild_id = $1 ORDER BY created_at DESC LIMIT $2) r",
            need(a, "guild_id")?, 100).await?,
        "raiders" => sqlx::query_scalar::<_, Option<Value>>(
            "SELECT jsonb_agg(r ORDER BY owners DESC, updated_at DESC) FROM (
                SELECT user_id::text, status, category, verified, public_note, expires_at, appeal_after, created_at,
                    updated_at, evidence, owners, open_appeals
                FROM mcp_raiders WHERE ($1::text IS NULL OR status = $1) ORDER BY owners DESC, updated_at DESC LIMIT $2) r",
        )
        .bind(text(a, "status"))
        .bind(limit(a, 50, 200))
        .fetch_one(&st.db)
        .await?
        .unwrap_or(json!([])),
        _ => bail!("unknown tool: {name}"),
    })
}

/// The newest `n` matching lines of `<dir>/<svc>.*.log`, oldest first. Hourly files sort by name.
pub fn tail(
    dir: &str,
    svc: &str,
    n: usize,
    contains: Option<&str>,
    level: Option<&str>,
) -> Result<Vec<String>> {
    let prefix = format!("{svc}.");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("no logs in {dir}: is the logs volume mounted?"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with(&prefix) && f.ends_with(".log"))
        })
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files.iter().rev() {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        for line in text.lines().rev() {
            if contains.is_some_and(|c| !line.contains(c))
                || level.is_some_and(|l| !line.contains(&format!(" {l} ")))
            {
                continue;
            }
            out.push(redact(line));
            if out.len() == n {
                out.reverse();
                return Ok(out);
            }
        }
    }
    out.reverse();
    Ok(out)
}

static SECRETS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"[A-Za-z0-9_-]{23,28}\.[A-Za-z0-9_-]{6,7}\.[A-Za-z0-9_-]{27,}", // Discord bot token
        r"|(?i:bearer)\s+\S+",
        r"|discord(?:app)?\.com/api/webhooks/\S+",
        r"|postgres(?:ql)?://\S+",
    ))
    .unwrap()
});

pub fn redact(line: &str) -> String {
    SECRETS.replace_all(line, "[masked]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secrets() {
        let tok = format!("{}.{}.{}", "M".repeat(26), "G".repeat(6), "x".repeat(38));
        let line = format!("token {tok} url https://discord.com/api/webhooks/1/abc db postgres://u:p@h/db Authorization: Bearer abc123");
        let out = redact(&line);
        for s in [tok.as_str(), "webhooks/1/abc", "u:p@h", "abc123"] {
            assert!(!out.contains(s), "{s} leaked: {out}");
        }
        assert_eq!(redact("user 123 joined"), "user 123 joined");
    }

    #[test]
    fn tail_reads_newest_files_last_lines() {
        let dir = std::env::temp_dir().join(format!("mcp-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bot.2026-11-10-18.log"), "a INFO 1\nb WARN 2\n").unwrap();
        std::fs::write(dir.join("bot.2026-11-10-19.log"), "c INFO 3\nd ERROR 4\n").unwrap();
        std::fs::write(dir.join("dashboard.2026-11-10-19.log"), "x INFO 9\n").unwrap();
        let d = dir.to_str().unwrap();
        assert_eq!(
            tail(d, "bot", 3, None, None).unwrap(),
            ["b WARN 2", "c INFO 3", "d ERROR 4"]
        );
        assert_eq!(
            tail(d, "bot", 10, None, Some("WARN")).unwrap(),
            ["b WARN 2"]
        );
        assert_eq!(tail(d, "bot", 10, Some("3"), None).unwrap(), ["c INFO 3"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}

//! Permit engine (spec section 2): who may run which mod action against whom. Pure, no I/O.
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::models::UserId;

/// Higher variants outrank lower ones; `Custom` ranks by its permit rank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Level {
    None,
    Custom {
        rank: i32,
        perms: Vec<String>,
        /// Systems this permit is exempt from: "antinuke", "automod", "words", "quarantine" or "all".
        whitelist: Vec<String>,
        /// Per-action command rate limits, keyed by action name.
        limits: HashMap<String, Limit>,
    },
    TrustedAdmin,
    ExtraOwner,
    Owner,
}

impl Level {
    fn order(&self) -> (u8, i32) {
        match self {
            Level::None => (0, 0),
            Level::Custom { rank, .. } => (1, *rank),
            Level::TrustedAdmin => (2, 0),
            Level::ExtraOwner => (3, 0),
            Level::Owner => (4, 0),
        }
    }

    /// Owner, EO and TA are immune to every bot action and bypass all systems.
    pub fn immune(&self) -> bool {
        self.order().0 >= 2
    }

    pub fn has(&self, perm: &str) -> bool {
        match self {
            Level::None => false,
            Level::Custom { perms, .. } => perms
                .iter()
                .any(|p| p == perm || implied_by(perm).contains(&p.as_str())),
            _ => true,
        }
    }

    pub fn whitelisted(&self, flag: &str) -> bool {
        match self {
            Level::None => false,
            Level::Custom { whitelist, .. } => whitelist.iter().any(|w| {
                // The per-action `wl_` flags are narrower forms of the older blanket `antinuke` flag.
                w == flag || w == "all" || (flag.starts_with("wl_") && w == "antinuke")
            }),
            _ => true,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    NoPermit,
    TargetProtected,
    TargetAboveBot,
    SelfTarget,
}

/// Permits that predate a command keep working: holders of these older permits also get the new one.
fn implied_by(perm: &str) -> &'static [&'static str] {
    match perm {
        "lockdown" => &["lock_channels", "lock_staff_roles", "lock_joins"],
        "quarantine" => &["kick", "ban"],
        "purge" | "slowmode" => &["timeout", "kick", "ban"],
        "cases" => &["warn", "timeout", "kick", "ban"],
        "notes_view" => &["notes_manage"],
        _ => &[],
    }
}

/// The default staff permits (Admins, Moderators) that exist but no role holds, by name.
pub async fn roleless_staff(pool: &sqlx::PgPool, guild_id: i64) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT name FROM permits p WHERE guild_id = $1 AND name IN ('Admins', 'Moderators')
         AND NOT EXISTS (SELECT 1 FROM permit_roles r WHERE r.permit_id = p.id) ORDER BY name",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
}

/// Role ids of the default staff permits (Admins, Moderators): who Channel Setup lets into limited channels.
pub async fn staff_roles(pool: &sqlx::PgPool, guild_id: i64) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar(
        "SELECT DISTINCT r.role_id FROM permits p JOIN permit_roles r ON r.permit_id = p.id
         WHERE p.guild_id = $1 AND p.name IN ('Admins', 'Moderators') ORDER BY 1",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
}

/// Role ids on the Moderators permit and not on Admins: the security score holds them to member rules.
pub async fn mod_roles(pool: &sqlx::PgPool, guild_id: i64) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar(
        "SELECT DISTINCT r.role_id FROM permits p JOIN permit_roles r ON r.permit_id = p.id
         WHERE p.guild_id = $1 AND p.name = 'Moderators' AND NOT EXISTS (
           SELECT 1 FROM permits a JOIN permit_roles ar ON ar.permit_id = a.id
           WHERE a.guild_id = $1 AND a.name = 'Admins' AND ar.role_id = r.role_id)
         ORDER BY 1",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
}

/// One toggle in the dashboard's permit editor. `flag` entries live in a permit's whitelist, the
/// rest in its perms. `locked` ones are shown but never stored.
pub struct PermDef {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub flag: bool,
    pub locked: bool,
}

pub struct PermSection {
    pub id: &'static str,
    pub title: &'static str,
    pub items: &'static [PermDef],
}

const fn perm(key: &'static str, label: &'static str, help: &'static str) -> PermDef {
    PermDef {
        key,
        label,
        help,
        flag: false,
        locked: false,
    }
}

const fn flag(key: &'static str, label: &'static str, help: &'static str) -> PermDef {
    PermDef {
        key,
        label,
        help,
        flag: true,
        locked: false,
    }
}

/// Everything a permit can hold, in the order the editor shows it (five-section layout).
pub const CATALOG: &[PermSection] = &[
    PermSection {
        id: "immunity",
        title: "Immunity",
        items: &[
            PermDef {
                key: "owner",
                label: "Owner",
                help: "Anyone with this can do ANYTHING and is IMMUNE from anything",
                flag: false,
                locked: true,
            },
            flag(
                "all",
                "Full Immunity",
                "Entirely immune, can't change settings, total whitelist",
            ),
            flag(
                "automod",
                "Automod Immunity",
                "Safe from the automod, but not other things",
            ),
            perm(
                "dashboard_view",
                "Dashboard Access",
                "See this server's dashboard",
            ),
            perm(
                "dashboard_edit",
                "Dashboard Edit",
                "Change settings on this server's dashboard",
            ),
            flag(
                "reports",
                "Report Immunity",
                "Safe from being reported by server members",
            ),
        ],
    },
    PermSection {
        id: "moderation",
        title: "Moderation",
        items: &[
            perm("cases", "Manage Cases", "View and edit cases"),
            perm(
                "mod_stats",
                "View Mod Stats",
                "See how many actions each staff member took. Only owners can change who has this",
            ),
            perm("ban", "Ban Command", "Ban, unban and hackban"),
            perm(
                "kick",
                "Kick Command",
                "Anyone with this can kick members without limitations",
            ),
            perm("timeout", "Timeout Command", "Timeout and untimeout"),
            perm(
                "quarantine",
                "Quarantine Command",
                "Quarantine and unquarantine",
            ),
            perm("warn", "Warn Command", "Warn and unwarn"),
            perm(
                "slowmode",
                "Slowmode Command",
                "Anyone with this can set slowmode without limitations",
            ),
            perm(
                "purge",
                "Purge Command",
                "Anyone with this can purge messages without limitations",
            ),
            perm("verify", "Verify Command", "Manually verify members"),
            perm("sanitize", "Sanitize Command", "Sanitize/Dehoist"),
            perm("reports", "Manage Reports", "View and act on reports"),
            perm("backups", "Backups", "Take/Load/Delete backups"),
            perm(
                "notes_view",
                "View Notes",
                "Anyone with this can view member notes",
            ),
            perm(
                "notes_manage",
                "Manage Notes",
                "View/edit/remove/sweep notes",
            ),
            perm(
                "appeals",
                "Review Appeals",
                "Anyone with this can accept or deny appeals",
            ),
            perm("revert", "Revert Center", "Undo anti-nuke actions"),
            perm("tickets", "Manage Tickets", "See and close every ticket"),
        ],
    },
    PermSection {
        id: "lockdown",
        title: "Lockdown",
        items: &[
            perm(
                "lock_channels",
                "Lock Server Channels",
                "Anyone with this can lock and unlock server channels",
            ),
            perm(
                "lock_staff_roles",
                "Lock Staff Roles",
                "Lock/Unlock all dangerous roles",
            ),
            perm(
                "lock_joins",
                "Lock Server Joins",
                "Kick/Ban new members joining",
            ),
            perm(
                "lock_updates",
                "Make Lockdown Updates",
                "Make updates in the lock-updates channel",
            ),
        ],
    },
    PermSection {
        id: "whitelist",
        title: "Whitelist",
        items: &[
            flag(
                "wl_kick_ban",
                "Kick/Ban Whitelist",
                "Anyone with this can kick and ban without limitations",
            ),
            flag(
                "wl_channel_create",
                "Channel Creations Whitelist",
                "Anyone with this can create channels without limitations",
            ),
            flag(
                "wl_channel_delete",
                "Channel Deletions Whitelist",
                "Anyone with this can delete channels without limitations",
            ),
            flag(
                "wl_role_create",
                "Role Creations Whitelist",
                "Anyone with this can create roles without limitations",
            ),
            flag(
                "wl_role_delete",
                "Role Deletions Whitelist",
                "Anyone with this can delete roles without limitations",
            ),
            flag(
                "wl_webhook_create",
                "Webhook Creations Whitelist",
                "Anyone with this can create webhooks without limitations",
            ),
            flag(
                "quarantine",
                "Quarantine Whitelist",
                "Never quarantined by the quarantine hold",
            ),
            flag("words", "Profanity Whitelist", "Bypass the words blacklist"),
            flag("links", "Link Whitelist", "Bypass the links blacklist"),
        ],
    },
];

/// Keys older setups may already hold; saving them stays allowed.
const LEGACY: &[&str] = &["lockdown", "antinuke"];

/// Whether `key` may be stored as a perm (`flag` false) or a whitelist flag (`flag` true).
pub fn known(key: &str, flag: bool) -> bool {
    LEGACY.contains(&key)
        || CATALOG
            .iter()
            .flat_map(|s| s.items)
            .any(|d| d.key == key && d.flag == flag && d.key != "owner")
}

/// `target_above_bot`: target's top role is not below the bot's top role (Discord hierarchy).
pub fn check(
    actor: &Level,
    perm: &str,
    actor_id: UserId,
    target: &Level,
    target_id: UserId,
    target_above_bot: bool,
) -> Result<(), Denied> {
    if !actor.has(perm) {
        return Err(Denied::NoPermit);
    }
    if actor_id == target_id {
        return Err(Denied::SelfTarget);
    }
    // Any permit holder is protected, except from quarantine (permit holders can be quarantined).
    if target.immune() || (*target != Level::None && perm != "quarantine") {
        return Err(Denied::TargetProtected);
    }
    if target_above_bot {
        return Err(Denied::TargetAboveBot);
    }
    Ok(())
}

/// Per-permit command rate limit (spec 2.5.1). Sliding windows over recorded timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct Limit {
    pub per_min: u32,
    pub per_hour: u32,
}

#[derive(Default)]
pub struct RateLimiter {
    hits: HashMap<(UserId, String), VecDeque<Instant>>,
}

impl RateLimiter {
    /// Records the use and returns `false` when it exceeds the limit (caller quarantines the actor).
    pub fn allow(&mut self, actor: UserId, action: &str, limit: Limit, now: Instant) -> bool {
        let q = self.hits.entry((actor, action.to_owned())).or_default();
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(3600))
        {
            q.pop_front();
        }
        q.push_back(now);
        let last_min = q
            .iter()
            .filter(|t| now.duration_since(**t) < Duration::from_secs(60))
            .count();
        last_min as u32 <= limit.per_min && q.len() as u32 <= limit.per_hour
    }
}

impl RateLimiter {
    /// Puts back a use from before a restart (N3). Call oldest first.
    pub fn seed(&mut self, actor: UserId, action: &str, at: Instant) {
        self.hits
            .entry((actor, action.to_owned()))
            .or_default()
            .push_back(at);
    }
}

/// The limit a command use counts against: the permit's own for `action`, else `fallback`
/// (anti-nuke's limit for the same action, D5). None: unlimited (no permit, or immune).
pub fn command_limit(level: &Level, action: &str, fallback: Option<Limit>) -> Option<Limit> {
    match level {
        Level::Custom { limits, .. } => limits.get(action).copied().or(fallback),
        _ => None,
    }
}

/// Whether `actor` may run `action` now; `false` means the caller quarantines the actor.
pub fn command_allowed(
    rl: &mut RateLimiter,
    actor: UserId,
    level: &Level,
    action: &str,
    now: Instant,
) -> bool {
    command_limit(level, action, None).is_none_or(|l| rl.allow(actor, action, l, now))
}

/// The live EO/TA lists and custom permits as one document: the `permits` copy in each config version.
pub async fn snapshot(c: &mut sqlx::PgConnection, guild: i64) -> sqlx::Result<serde_json::Value> {
    let users = |table: &str| {
        format!("select user_id::text from {table} where guild_id = $1 order by user_id")
    };
    let eo: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(users("extra_owners")))
        .bind(guild)
        .fetch_all(&mut *c)
        .await?;
    let ta: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(users("trusted_admins")))
        .bind(guild)
        .fetch_all(&mut *c)
        .await?;
    type Row = (
        i64,
        String,
        i32,
        Vec<String>,
        serde_json::Value,
        Vec<String>,
    );
    let rows: Vec<Row> = sqlx::query_as("select id, name, rank, perms, limits, whitelist from permits where guild_id = $1 order by rank desc, name")
        .bind(guild)
        .fetch_all(&mut *c)
        .await?;
    let mut custom = vec![];
    for (id, name, rank, perms, limits, whitelist) in rows {
        let roles: Vec<String> = sqlx::query_scalar(
            "select role_id::text from permit_roles where permit_id = $1 order by role_id",
        )
        .bind(id)
        .fetch_all(&mut *c)
        .await?;
        custom.push(serde_json::json!({"name": name, "rank": rank, "perms": perms, "limits": limits, "whitelist": whitelist, "roles": roles}));
    }
    Ok(serde_json::json!({"eo": eo, "ta": ta, "custom": custom}))
}

/// Every role tied to one of the guild's permits; never handed out by verification or sticky roles.
pub async fn permit_roles(pool: &sqlx::PgPool, guild_id: i64) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar(
        "SELECT r.role_id FROM permit_roles r JOIN permits p ON p.id = r.permit_id WHERE p.guild_id = $1",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
}

/// Roles of the guild's permits that hold `perm` (staff for tickets).
pub async fn roles_with(pool: &sqlx::PgPool, guild_id: i64, perm: &str) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar(
        "SELECT DISTINCT r.role_id FROM permit_roles r JOIN permits p ON p.id = r.permit_id
         WHERE p.guild_id = $1 AND $2 = ANY(p.perms)",
    )
    .bind(guild_id)
    .bind(perm)
    .fetch_all(pool)
    .await
}

/// Resolves a member's level from the DB. `roles` are the member's role ids.
pub async fn resolve(
    pool: &sqlx::PgPool,
    guild_id: i64,
    owner_id: UserId,
    user_id: UserId,
    roles: &[i64],
) -> anyhow::Result<Level> {
    if user_id == owner_id {
        return Ok(Level::Owner);
    }
    let (eo,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM extra_owners WHERE guild_id=$1 AND user_id=$2)",
    )
    .bind(guild_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    if eo {
        return Ok(Level::ExtraOwner);
    }
    let (ta,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM trusted_admins WHERE guild_id=$1 AND user_id=$2)",
    )
    .bind(guild_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    if ta {
        return Ok(Level::TrustedAdmin);
    }
    // Highest-ranked permit among the member's roles; perms are the union of that permit only.
    let row: Option<(i32, Vec<String>, Vec<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT p.rank, p.perms, p.whitelist, p.limits FROM permits p JOIN permit_roles r ON r.permit_id = p.id
         WHERE p.guild_id = $1 AND r.role_id = ANY($2) ORDER BY p.rank DESC LIMIT 1",
    )
    .bind(guild_id)
    .bind(roles)
    .fetch_optional(pool)
    .await?;
    Ok(row.map_or(Level::None, |(rank, perms, whitelist, limits)| {
        // Malformed entries are dropped one by one so a bad action does not void the rest.
        let limits = limits
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.clone(), serde_json::from_value(v.clone()).ok()?)))
            .collect();
        Level::Custom {
            rank,
            perms,
            whitelist,
            limits,
        }
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn catalog_keys_are_unique_per_kind_and_known() {
        let mut seen = std::collections::HashSet::new();
        for d in CATALOG.iter().flat_map(|s| s.items) {
            assert!(seen.insert((d.key, d.flag)), "duplicate {}", d.key);
            assert_eq!(known(d.key, d.flag), d.key != "owner");
        }
        assert!(known("antinuke", true) && !known("made_up", false) && !known("ban", true));
    }

    #[test]
    fn per_action_whitelist_is_narrower_than_antinuke() {
        let w = |f: &[&str]| Level::Custom {
            rank: 1,
            perms: vec![],
            whitelist: f.iter().map(|x| x.to_string()).collect(),
            limits: Default::default(),
        };
        assert!(w(&["wl_kick_ban"]).whitelisted("wl_kick_ban"));
        assert!(!w(&["wl_kick_ban"]).whitelisted("wl_role_delete"));
        assert!(w(&["antinuke"]).whitelisted("wl_role_delete"));
        let locker = Level::Custom {
            rank: 1,
            perms: vec!["lock_joins".into()],
            whitelist: vec![],
            limits: Default::default(),
        };
        assert!(locker.has("lockdown"));
    }

    #[test]
    fn older_permits_imply_the_new_commands() {
        let permit = |perms: &[&str]| Level::Custom {
            rank: 1,
            perms: perms.iter().map(|p| p.to_string()).collect(),
            whitelist: vec![],
            limits: Default::default(),
        };
        assert!(permit(&["timeout"]).has("purge"));
        assert!(permit(&["ban"]).has("quarantine"));
        assert!(!permit(&["warn"]).has("quarantine"));
        assert!(!permit(&["warn"]).has("purge"));
        assert!(!permit(&[]).has("cases"));
    }

    use super::*;

    fn custom(rank: i32, perms: &[&str]) -> Level {
        Level::Custom {
            rank,
            perms: perms.iter().map(|s| s.to_string()).collect(),
            whitelist: vec![],
            limits: HashMap::new(),
        }
    }

    #[test]
    fn permit_checks() {
        let mod_ = custom(1, &["ban"]);
        assert_eq!(
            check(&Level::None, "ban", 1, &Level::None, 2, false),
            Err(Denied::NoPermit)
        );
        assert_eq!(
            check(&mod_, "kick", 1, &Level::None, 2, false),
            Err(Denied::NoPermit)
        );
        assert_eq!(check(&mod_, "ban", 1, &Level::None, 2, false), Ok(()));
        assert_eq!(
            check(&mod_, "ban", 1, &Level::TrustedAdmin, 2, false),
            Err(Denied::TargetProtected)
        );
        assert_eq!(
            check(&mod_, "ban", 1, &custom(1, &[]), 2, false),
            Err(Denied::TargetProtected)
        );
        assert_eq!(
            check(&custom(2, &["ban"]), "ban", 1, &custom(1, &[]), 2, false),
            Err(Denied::TargetProtected)
        );
        let q = custom(1, &["quarantine"]);
        assert_eq!(
            check(&q, "quarantine", 1, &custom(1, &[]), 2, false),
            Ok(())
        );
        assert_eq!(
            check(&q, "quarantine", 1, &Level::TrustedAdmin, 2, false),
            Err(Denied::TargetProtected)
        );
        assert_eq!(
            check(&mod_, "ban", 1, &Level::None, 2, true),
            Err(Denied::TargetAboveBot)
        );
        assert_eq!(
            check(&Level::Owner, "ban", 1, &Level::None, 1, false),
            Err(Denied::SelfTarget)
        );
    }

    #[test]
    fn rate_limit_windows() {
        let mut rl = RateLimiter::default();
        let lim = Limit {
            per_min: 2,
            per_hour: 3,
        };
        let t0 = Instant::now();
        assert!(rl.allow(1, "ban", lim, t0));
        assert!(rl.allow(1, "ban", lim, t0));
        assert!(!rl.allow(1, "ban", lim, t0)); // 3rd within the minute
        assert!(rl.allow(2, "ban", lim, t0)); // other actor unaffected
        let later = t0 + Duration::from_secs(61);
        assert!(!rl.allow(1, "ban", lim, later)); // 4th in the hour breaches per_hour
    }

    #[test]
    fn whitelist_flags() {
        let w = |f: &[&str]| Level::Custom {
            rank: 1,
            perms: vec![],
            limits: HashMap::new(),
            whitelist: f.iter().map(|s| s.to_string()).collect(),
        };
        assert!(Level::Owner.whitelisted("antinuke"));
        assert!(!Level::None.whitelisted("antinuke"));
        assert!(w(&["antinuke"]).whitelisted("antinuke"));
        assert!(!w(&["antinuke"]).whitelisted("automod"));
        assert!(w(&["all"]).whitelisted("quarantine"));
    }

    #[test]
    fn command_allowed_applies_limits() {
        let lvl = Level::Custom {
            rank: 1,
            perms: vec![],
            whitelist: vec![],
            limits: HashMap::from([(
                "ban".into(),
                Limit {
                    per_min: 1,
                    per_hour: 5,
                },
            )]),
        };
        let mut rl = RateLimiter::default();
        let t = Instant::now();
        assert!(command_allowed(&mut rl, 1, &lvl, "ban", t));
        assert!(!command_allowed(&mut rl, 1, &lvl, "ban", t));
        assert!(command_allowed(&mut rl, 1, &lvl, "kick", t)); // no limit configured
        assert!(command_allowed(&mut rl, 1, &Level::TrustedAdmin, "ban", t));
        // Without its own limit, a permit takes the fallback (anti-nuke's); immune never.
        let nuke = Limit {
            per_min: 4,
            per_hour: 14,
        };
        assert_eq!(command_limit(&lvl, "kick", Some(nuke)), Some(nuke));
        assert_eq!(command_limit(&lvl, "ban", Some(nuke)).unwrap().per_min, 1);
        assert_eq!(command_limit(&Level::Owner, "ban", Some(nuke)), None);
        // A use from before a restart still counts.
        let mut rl = RateLimiter::default();
        rl.seed(1, "ban", t);
        assert!(!command_allowed(&mut rl, 1, &lvl, "ban", t));
    }

    #[tokio::test]
    async fn resolve_reads_whitelist_and_limits() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = crate::db::connect(&url).await.unwrap();
        let gid = 8_000_000_000 + std::process::id() as i64;
        let (pid,): (i64,) = sqlx::query_as(
            "INSERT INTO permits (guild_id, name, rank, perms, whitelist, limits)
             VALUES ($1,'t',3,'{ban}','{antinuke}','{\"ban\":{\"per_min\":5,\"per_hour\":20},\"bad\":1}') RETURNING id",
        )
        .bind(gid)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1,$2,77)")
            .bind(pid)
            .bind(gid)
            .execute(&pool)
            .await
            .unwrap();
        let got = resolve(&pool, gid, 1, 2, &[77]).await;
        sqlx::query("DELETE FROM permits WHERE guild_id=$1")
            .bind(gid)
            .execute(&pool)
            .await
            .unwrap();
        let Level::Custom {
            rank,
            whitelist,
            limits,
            ..
        } = got.unwrap()
        else {
            panic!("not custom")
        };
        assert_eq!((rank, whitelist), (3, vec!["antinuke".to_string()]));
        assert_eq!(limits.len(), 1);
        assert_eq!(limits["ban"].per_hour, 20);
    }
}

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
            Level::Custom { perms, .. } => perms.iter().any(|p| p == perm),
            _ => true,
        }
    }

    pub fn whitelisted(&self, flag: &str) -> bool {
        match self {
            Level::None => false,
            Level::Custom { whitelist, .. } => whitelist.iter().any(|w| w == flag || w == "all"),
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
    // Any permit holder is protected from ban/kick/etc. unless the actor strictly outranks them.
    if target.immune() || (*target != Level::None && target.order() >= actor.order()) {
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

/// Whether `actor` may run `action` now; `false` means the caller quarantines the actor.
pub fn command_allowed(
    rl: &mut RateLimiter,
    actor: UserId,
    level: &Level,
    action: &str,
    now: Instant,
) -> bool {
    match level {
        Level::Custom { limits, .. } => limits
            .get(action)
            .is_none_or(|l| rl.allow(actor, action, *l, now)),
        _ => true,
    }
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
            Ok(())
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

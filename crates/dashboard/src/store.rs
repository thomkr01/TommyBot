//! Versioned config and permit tables. Every write path goes through `commit`.
use crate::ApiError;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use tommybot_core::permits;

/// Version 0 with an empty config until the first save: nothing else creates a guild's first row.
pub async fn current(db: &PgPool, guild: i64) -> Result<(i32, Value), ApiError> {
    Ok(sqlx::query_as("select version, config from guild_config where guild_id = $1 order by version desc limit 1")
        .bind(guild).fetch_optional(db).await?.unwrap_or((0, json!({}))))
}

/// The newest version under the config lock, so a `commit` in the same transaction cannot conflict.
pub async fn locked(
    tx: &mut Transaction<'_, Postgres>,
    guild: i64,
) -> Result<(i32, Value), ApiError> {
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(guild)
        .execute(&mut **tx)
        .await?;
    Ok(sqlx::query_as("select version, config from guild_config where guild_id = $1 order by version desc limit 1")
        .bind(guild).fetch_optional(&mut **tx).await?.unwrap_or((0, json!({}))))
}

/// Records a change made to the permit tables alone as a new version, so rollbacks and stale saves see it.
pub async fn bump(
    tx: &mut Transaction<'_, Postgres>,
    guild: i64,
    user: i64,
) -> Result<i32, ApiError> {
    let (cur, config) = locked(tx, guild).await?;
    commit(tx, guild, cur, &config, user).await
}

/// Appends a new config version if `expected` is still the newest, then tells the bot.
/// The version's `permits` copy is always read from the live tables, never taken from `config`.
pub async fn commit(
    tx: &mut Transaction<'_, Postgres>,
    guild: i64,
    expected: i32,
    config: &Value,
    user: i64,
) -> Result<i32, ApiError> {
    let (cur, _) = locked(tx, guild).await?;
    if cur != expected {
        return Err(ApiError::Conflict(cur));
    }
    let mut config = config.clone();
    config["permits"] = read_permits(tx, guild).await?;
    sqlx::query(
        "insert into guild_config (guild_id, version, config, changed_by) values ($1,$2,$3,$4)",
    )
    .bind(guild)
    .bind(cur + 1)
    .bind(&config)
    .bind(user)
    .execute(&mut **tx)
    .await?;
    sqlx::query("select pg_notify('config_changed', $1)")
        .bind(format!("{guild}:{}", cur + 1))
        .execute(&mut **tx)
        .await?;
    Ok(cur + 1)
}

fn ids(v: &Value) -> Vec<i64> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str()?.parse().ok())
        .collect()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(String::from))
        .collect()
}

pub async fn read_permits(c: &mut PgConnection, guild: i64) -> Result<Value, ApiError> {
    Ok(permits::snapshot(c, guild).await?)
}

/// Replaces the guild's EO/TA lists and custom permits with `p`. Permits are upserted by name.
pub async fn write_permits(c: &mut PgConnection, guild: i64, p: &Value) -> Result<(), ApiError> {
    let bad = |m: &str| ApiError::BadRequest(m.into());
    let custom = p["custom"]
        .as_array()
        .ok_or_else(|| bad("permits.custom must be a list"))?;
    let mut names = vec![];
    for x in custom {
        let name = x["name"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty() && n.len() <= 32)
            .ok_or_else(|| bad("permit needs a name of 1-32 chars"))?;
        if names.contains(&name) {
            return Err(bad("duplicate permit name"));
        }
        names.push(name);
        let perms = strs(&x["perms"]);
        let whitelist = strs(&x["whitelist"]);
        if let Some(k) = perms
            .iter()
            .find(|k| !permits::known(k, false))
            .or(whitelist.iter().find(|k| !permits::known(k, true)))
        {
            return Err(ApiError::BadRequest(format!("unknown permission `{k}`")));
        }
        let id: i64 = sqlx::query_scalar(
            "insert into permits (guild_id, name, rank, perms, limits, whitelist) values ($1,$2,$3,$4,$5,$6)
             on conflict (guild_id, name) do update set rank = $3, perms = $4, limits = $5, whitelist = $6 returning id")
            .bind(guild).bind(name).bind(x["rank"].as_i64().unwrap_or(0) as i32).bind(&perms)
            .bind(if x["limits"].is_object() { x["limits"].clone() } else { json!({}) }).bind(&whitelist)
            .fetch_one(&mut *c).await?;
        sqlx::query("delete from permit_roles where permit_id = $1")
            .bind(id)
            .execute(&mut *c)
            .await?;
        for r in ids(&x["roles"]) {
            sqlx::query("insert into permit_roles (permit_id, guild_id, role_id) values ($1,$2,$3) on conflict do nothing")
                .bind(id).bind(guild).bind(r).execute(&mut *c).await?;
        }
    }
    sqlx::query("delete from permits where guild_id = $1 and name <> all($2)")
        .bind(guild)
        .bind(&names)
        .execute(&mut *c)
        .await?;
    for (table, key) in [("extra_owners", "eo"), ("trusted_admins", "ta")] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "delete from {table} where guild_id = $1"
        )))
        .bind(guild)
        .execute(&mut *c)
        .await?;
        for u in ids(&p[key]) {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "insert into {table} (guild_id, user_id) values ($1,$2) on conflict do nothing"
            )))
            .bind(guild)
            .bind(u)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn versions_conflicts_and_permits() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_000_000_000 + (std::process::id() as i64);
        assert_eq!(current(&db, g).await.unwrap(), (0, json!({})));
        let mut tx = db.begin().await.unwrap();
        assert_eq!(commit(&mut tx, g, 0, &json!({"a": 1}), 7).await.unwrap(), 1);
        assert!(matches!(
            commit(&mut tx, g, 0, &json!({}), 7).await,
            Err(ApiError::Conflict(1))
        ));
        let p = json!({"eo": ["5"], "ta": [], "custom": [{"name": "Mod", "rank": 1, "perms": ["ban", "dashboard_view"], "limits": {"ban": {"per_min": 5}}, "whitelist": [], "roles": ["10", "11"]}]});
        write_permits(&mut tx, g, &p).await.unwrap();
        assert_eq!(read_permits(&mut tx, g).await.unwrap(), p);
        write_permits(&mut tx, g, &json!({"eo": [], "ta": ["6"], "custom": []}))
            .await
            .unwrap();
        assert_eq!(read_permits(&mut tx, g).await.unwrap()["custom"], json!([]));
        assert!(write_permits(
            &mut tx,
            g,
            &json!({"custom": [{"name": "X", "perms": ["BAD"]}]})
        )
        .await
        .is_err());
        tx.rollback().await.unwrap();
    }

    /// A version's `permits` copy is the live tables, also for a save that carried a stale copy, and
    /// a change to the tables alone (rescue claim) gets a version of its own.
    #[tokio::test]
    async fn every_version_holds_the_live_permits() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_840_000_000 + (std::process::id() as i64);
        let mut tx = db.begin().await.unwrap();
        sqlx::query("insert into trusted_admins (guild_id, user_id) values ($1, 6)")
            .bind(g)
            .execute(&mut *tx)
            .await
            .unwrap();
        let stale = json!({"a": 1, "permits": {"eo": [], "ta": ["6", "9"], "custom": []}});
        assert_eq!(commit(&mut tx, g, 0, &stale, 7).await.unwrap(), 1);
        sqlx::query("insert into extra_owners (guild_id, user_id) values ($1, 5)")
            .bind(g)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_eq!(bump(&mut tx, g, 5).await.unwrap(), 2);
        let rows: Vec<(i32, Value)> = sqlx::query_as(
            "select version, config from guild_config where guild_id = $1 order by version",
        )
        .bind(g)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        let live = |eo: &[&str]| json!({"a": 1, "permits": {"eo": eo, "ta": ["6"], "custom": []}});
        assert_eq!(rows, vec![(1, live(&[])), (2, live(&["5"]))]);
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn history_keeps_newest_200_versions() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_300_000_000 + (std::process::id() as i64);
        let mut tx = db.begin().await.unwrap();
        for v in 0..205 {
            commit(&mut tx, g, v, &json!({}), 7).await.unwrap();
        }
        let (n, low): (i64, i32) =
            sqlx::query_as("select count(*), min(version) from guild_config where guild_id = $1")
                .bind(g)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!((n, low), (200, 6));
        tx.rollback().await.unwrap();
    }
}

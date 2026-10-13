//! Versioned config and permit tables. Every write path goes through `commit`.
use crate::ApiError;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

pub async fn current(db: &PgPool, guild: i64) -> Result<Option<(i32, Value)>, ApiError> {
    Ok(sqlx::query_as("select version, config from guild_config where guild_id = $1 order by version desc limit 1")
        .bind(guild).fetch_optional(db).await?)
}

/// Appends a new config version if `expected` is still the newest, then tells the bot.
pub async fn commit(tx: &mut Transaction<'_, Postgres>, guild: i64, expected: i32, config: &Value, user: i64) -> Result<i32, ApiError> {
    sqlx::query("select pg_advisory_xact_lock($1)").bind(guild).execute(&mut **tx).await?;
    let cur: i32 = sqlx::query_scalar("select coalesce(max(version), 0) from guild_config where guild_id = $1")
        .bind(guild).fetch_one(&mut **tx).await?;
    if cur != expected {
        return Err(ApiError::Conflict(cur));
    }
    sqlx::query("insert into guild_config (guild_id, version, config, changed_by) values ($1,$2,$3,$4)")
        .bind(guild).bind(cur + 1).bind(config).bind(user).execute(&mut **tx).await?;
    sqlx::query("select pg_notify('config_changed', $1)").bind(guild.to_string()).execute(&mut **tx).await?;
    Ok(cur + 1)
}

type PermitRow = (i64, String, i32, Vec<String>, Value, Vec<String>);

fn ids(v: &Value) -> Vec<i64> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str()?.parse().ok()).collect()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()
}

pub async fn read_permits(c: &mut PgConnection, guild: i64) -> Result<Value, ApiError> {
    let eo: Vec<i64> = sqlx::query_scalar("select user_id from extra_owners where guild_id = $1 order by user_id").bind(guild).fetch_all(&mut *c).await?;
    let ta: Vec<i64> = sqlx::query_scalar("select user_id from trusted_admins where guild_id = $1 order by user_id").bind(guild).fetch_all(&mut *c).await?;
    let rows: Vec<PermitRow> =
        sqlx::query_as("select id, name, rank, perms, limits, whitelist from permits where guild_id = $1 order by rank desc, name").bind(guild).fetch_all(&mut *c).await?;
    let mut custom = vec![];
    for (id, name, rank, perms, limits, whitelist) in rows {
        let roles: Vec<i64> = sqlx::query_scalar("select role_id from permit_roles where permit_id = $1 order by role_id").bind(id).fetch_all(&mut *c).await?;
        custom.push(json!({"name": name, "rank": rank, "perms": perms, "limits": limits, "whitelist": whitelist,
            "roles": roles.iter().map(i64::to_string).collect::<Vec<_>>()}));
    }
    let s = |v: Vec<i64>| v.iter().map(i64::to_string).collect::<Vec<_>>();
    Ok(json!({"eo": s(eo), "ta": s(ta), "custom": custom}))
}

/// Replaces the guild's EO/TA lists and custom permits with `p`. Permits are upserted by name.
pub async fn write_permits(c: &mut PgConnection, guild: i64, p: &Value) -> Result<(), ApiError> {
    let bad = |m: &str| ApiError::BadRequest(m.into());
    let custom = p["custom"].as_array().ok_or_else(|| bad("permits.custom must be a list"))?;
    let mut names = vec![];
    for x in custom {
        let name = x["name"].as_str().map(str::trim).filter(|n| !n.is_empty() && n.len() <= 32).ok_or_else(|| bad("permit needs a name of 1-32 chars"))?;
        if names.contains(&name) {
            return Err(bad("duplicate permit name"));
        }
        names.push(name);
        let perms = strs(&x["perms"]);
        if perms.iter().any(|s| s.is_empty() || !s.chars().all(|c| c.is_ascii_lowercase() || c == '_')) {
            return Err(bad("permission names are lowercase snake_case"));
        }
        let id: i64 = sqlx::query_scalar(
            "insert into permits (guild_id, name, rank, perms, limits, whitelist) values ($1,$2,$3,$4,$5,$6)
             on conflict (guild_id, name) do update set rank = $3, perms = $4, limits = $5, whitelist = $6 returning id")
            .bind(guild).bind(name).bind(x["rank"].as_i64().unwrap_or(0) as i32).bind(&perms)
            .bind(if x["limits"].is_object() { x["limits"].clone() } else { json!({}) }).bind(strs(&x["whitelist"]))
            .fetch_one(&mut *c).await?;
        sqlx::query("delete from permit_roles where permit_id = $1").bind(id).execute(&mut *c).await?;
        for r in ids(&x["roles"]) {
            sqlx::query("insert into permit_roles (permit_id, guild_id, role_id) values ($1,$2,$3) on conflict do nothing")
                .bind(id).bind(guild).bind(r).execute(&mut *c).await?;
        }
    }
    sqlx::query("delete from permits where guild_id = $1 and name <> all($2)").bind(guild).bind(&names).execute(&mut *c).await?;
    for (table, key) in [("extra_owners", "eo"), ("trusted_admins", "ta")] {
        sqlx::query(&format!("delete from {table} where guild_id = $1")).bind(guild).execute(&mut *c).await?;
        for u in ids(&p[key]) {
            sqlx::query(&format!("insert into {table} (guild_id, user_id) values ($1,$2) on conflict do nothing")).bind(guild).bind(u).execute(&mut *c).await?;
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
        let Ok(url) = std::env::var("DATABASE_URL") else { return };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_000_000_000 + (std::process::id() as i64);
        let mut tx = db.begin().await.unwrap();
        assert_eq!(commit(&mut tx, g, 0, &json!({"a": 1}), 7).await.unwrap(), 1);
        assert!(matches!(commit(&mut tx, g, 0, &json!({}), 7).await, Err(ApiError::Conflict(1))));
        let p = json!({"eo": ["5"], "ta": [], "custom": [{"name": "Mod", "rank": 1, "perms": ["ban", "dashboard_view"], "limits": {"ban": {"per_min": 5}}, "whitelist": [], "roles": ["10", "11"]}]});
        write_permits(&mut tx, g, &p).await.unwrap();
        assert_eq!(read_permits(&mut tx, g).await.unwrap(), p);
        write_permits(&mut tx, g, &json!({"eo": [], "ta": ["6"], "custom": []})).await.unwrap();
        assert_eq!(read_permits(&mut tx, g).await.unwrap()["custom"], json!([]));
        assert!(write_permits(&mut tx, g, &json!({"custom": [{"name": "X", "perms": ["BAD"]}]})).await.is_err());
        tx.rollback().await.unwrap();
    }
}

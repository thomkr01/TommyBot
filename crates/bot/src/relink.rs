//! A channel or role that a restore made again has a new id. Every place the config (and the
//! permit roles) named the old id gets the new one, as a config version (D68), so log, catch,
//! ticket, appeal and report channels and the quarantine and main roles keep working.
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use tommybot_logging::LogConfig;
use twilight_model::id::{marker::GuildMarker, Id};

use crate::app::App;

/// Swaps old ids for new ones; only logs a failure, since the restore itself is done.
pub async fn relink(app: &App, gid: Id<GuildMarker>, map: &HashMap<u64, u64>) {
    let map: HashMap<u64, u64> = map
        .iter()
        .filter(|(o, n)| o != n)
        .map(|(o, n)| (*o, *n))
        .collect();
    if map.is_empty() {
        return;
    }
    match run(app, gid, &map).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(%gid, n, "config ids moved to recreated objects"),
        Err(e) => tracing::warn!(?e, %gid, "moving config ids to recreated objects failed"),
    }
}

async fn run(app: &App, gid: Id<GuildMarker>, map: &HashMap<u64, u64>) -> Result<usize> {
    let g = gid.get() as i64;
    let (mut tx, version, mut doc) = crate::settings::locked(app, gid).await?;
    let mut n = swap(&mut doc, map);
    // A deleted log channel already left `logging` when it was deleted (`forget_log_channel`):
    // its routes come back from the newest version that still named it.
    let olds: Vec<String> = map.keys().map(u64::to_string).collect();
    let before: Option<Value> = sqlx::query_scalar(
        "SELECT config->'logging' FROM guild_config WHERE guild_id = $1
         AND EXISTS (SELECT 1 FROM unnest($2::text[]) o WHERE strpos((config->'logging')::text, '\"' || o || '\"') > 0)
         ORDER BY version DESC LIMIT 1",
    )
    .bind(g)
    .bind(&olds)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(before) = before {
        n += relink_logs(&mut doc, &before, map);
    }
    for (old, new) in map {
        n += sqlx::query(
            "UPDATE permit_roles SET role_id = $3 WHERE guild_id = $1 AND role_id = $2",
        )
        .bind(g)
        .bind(*old as i64)
        .bind(*new as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;
    }
    if n == 0 {
        tx.commit().await?;
        return Ok(0);
    }
    crate::settings::append(app, tx, gid, version, &mut doc, app.me.get()).await?;
    app.forget_cfg(gid.get());
    Ok(n)
}

/// Replaces every id in `map` (string or number value, or object key); returns how many.
fn swap(v: &mut Value, map: &HashMap<u64, u64>) -> usize {
    let hit = |s: &str| s.parse::<u64>().ok().and_then(|i| map.get(&i)).copied();
    match v {
        Value::String(s) => hit(s).map_or(0, |n| {
            *s = n.to_string();
            1
        }),
        Value::Number(x) => match x.as_u64().and_then(|i| map.get(&i)) {
            Some(n) => {
                *v = Value::from(*n);
                1
            }
            None => 0,
        },
        Value::Array(a) => a.iter_mut().map(|x| swap(x, map)).sum(),
        Value::Object(o) => {
            let mut n = 0;
            for (k, x) in std::mem::take(o) {
                let k = match hit(&k) {
                    Some(new) => {
                        n += 1;
                        new.to_string()
                    }
                    None => k,
                };
                let mut x = x;
                n += swap(&mut x, map);
                o.insert(k, x);
            }
            n
        }
        _ => 0,
    }
}

/// Puts back the log routes `before` sent to a recreated channel, where the slot is still free.
fn relink_logs(doc: &mut Value, before: &Value, map: &HashMap<u64, u64>) -> usize {
    let (Ok(old), Ok(mut now)) = (
        serde_json::from_value::<LogConfig>(before.clone()),
        serde_json::from_value::<LogConfig>(doc["logging"].clone()),
    ) else {
        return 0;
    };
    let new = |c: &Id<_>| map.get(&c.get()).map(|n| Id::new(*n));
    let mut n = 0;
    if let Some(f) = old
        .fallback
        .as_ref()
        .and_then(new)
        .filter(|_| now.fallback.is_none())
    {
        now.fallback = Some(f);
        n += 1;
    }
    for (k, c) in &old.routes {
        if let Some(c) = new(c).filter(|_| !now.routes.contains_key(k)) {
            now.routes.insert(*k, c);
            n += 1;
        }
    }
    for (k, c) in &old.events {
        if let Some(c) = new(c).filter(|_| !now.events.contains_key(k)) {
            now.events.insert(*k, c);
            n += 1;
        }
    }
    for c in &old.custom {
        if let Some(ch) = new(&c.channel).filter(|ch| !now.custom.iter().any(|x| x.channel == *ch))
        {
            now.custom.push(tommybot_logging::CustomLog {
                events: c.events.clone(),
                channel: ch,
            });
            n += 1;
        }
    }
    if n > 0 {
        // Merged in, so keys LogConfig does not model (the dashboard's config_changes) survive.
        if let (Some(o), Ok(Value::Object(new))) = (
            doc["logging"].as_object_mut(),
            serde_json::to_value(now.clone()),
        ) {
            o.extend(new);
        } else if let Ok(v) = serde_json::to_value(now) {
            doc["logging"] = v;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_id_moves_and_forgotten_log_routes_come_back() {
        let (old_ch, new_ch, old_role, new_role) = (
            111_111_111_111_111_111u64,
            222_222_222_222_222_222u64,
            333_333_333_333_333_333u64,
            444_444_444_444_444_444u64,
        );
        let map = HashMap::from([(old_ch, new_ch), (old_role, new_role)]);
        let mut doc = json!({
            "catch": {"channel": old_ch.to_string()},
            "tickets": {"category": old_ch.to_string()},
            "antinuke": {"quarantine_role": old_role.to_string()},
            "lockdown": {"main_roles": [old_role.to_string(), "5"]},
            "whitelist": {"roles": {old_role.to_string(): ["x"]}},
            "misc": {"count": 7},
            "logging": {"fallback": "9", "config_changes": "8"}
        });
        assert_eq!(swap(&mut doc, &map), 5);
        assert_eq!(doc["catch"]["channel"], json!(new_ch.to_string()));
        assert_eq!(
            doc["lockdown"]["main_roles"],
            json!([new_role.to_string(), "5"])
        );
        assert!(doc["whitelist"]["roles"]
            .get(new_role.to_string())
            .is_some());
        assert_eq!(doc["misc"]["count"], json!(7));
        let before =
            json!({"fallback": "9", "routes": {"anti_nuke": old_ch.to_string(), "panic": "10"}});
        assert_eq!(relink_logs(&mut doc, &before, &map), 1);
        assert_eq!(
            doc["logging"]["routes"]["anti_nuke"],
            json!(new_ch.to_string())
        );
        assert!(
            doc["logging"]["routes"].get("panic").is_none(),
            "only routes to the recreated channel"
        );
        assert_eq!(
            doc["logging"]["config_changes"],
            json!("8"),
            "unmodelled keys survive"
        );
        assert_eq!(
            relink_logs(&mut doc, &before, &map),
            0,
            "a taken slot stays"
        );
    }

    #[tokio::test]
    async fn a_relink_is_a_config_version_with_the_permit_roles() {
        let Some(t) = crate::bansync::tests::T::new(203).await else {
            return;
        };
        let g = t.g(0);
        let (old, new) = (t.base * 100 + 1, t.base * 100 + 2);
        t.server_config(
            0,
            t.owner(),
            json!({"logging": {"routes": {"anti_nuke": old.to_string()}}}),
        )
        .await;
        sqlx::query("INSERT INTO guild_config (guild_id, version, config) VALUES ($1, 2, $2)")
            .bind(g.get() as i64)
            .bind(json!({"logging": {}, "catch": {"channel": old.to_string()}}))
            .execute(&t.app.pool)
            .await
            .unwrap();
        let pid: i64 = sqlx::query_scalar(
            "INSERT INTO permits (guild_id, name) VALUES ($1, 'Admins') RETURNING id",
        )
        .bind(g.get() as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $2, $3)")
            .bind(pid)
            .bind(g.get() as i64)
            .bind(old as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        relink(&t.app, g, &HashMap::from([(old, new)])).await;
        let (v, c): (i32, Value) = sqlx::query_as(
            "SELECT version, config FROM guild_config WHERE guild_id = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(g.get() as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
        assert_eq!(v, 3);
        assert_eq!(c["catch"]["channel"], json!(new.to_string()));
        assert_eq!(c["logging"]["routes"]["anti_nuke"], json!(new.to_string()));
        let role: i64 = sqlx::query_scalar("SELECT role_id FROM permit_roles WHERE permit_id = $1")
            .bind(pid)
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
        assert_eq!(role, new as i64);
        t.cleanup().await;
    }
}

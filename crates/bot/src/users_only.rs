//! Only users can be Trusted Admin, Extra Owner or on the quarantine whitelist (Thomas 11-13). A bot
//! gets a bot permit instead: a normal permit on its own bot role, with exactly the exemptions picked.
use anyhow::Result;
use serde_json::Value;
use tommybot_logging::{embed, embed::ORANGE, LogKind};
use twilight_model::{
    guild::Member,
    id::{marker::GuildMarker, Id},
};

use crate::app::App;

const BOT_PERMIT: &str = "A bot permit gives a bot exactly the exemptions you pick: make a permit on the bot's own bot role (the role Discord made for it) on the dashboard's Permits page.";

async fn is_bot(app: &App, id: i64) -> Result<bool> {
    let id = Id::new_checked(id as u64).ok_or_else(|| anyhow::anyhow!("user id 0"))?;
    Ok(app.http.user(id).await?.model().await?.bot)
}

/// None when every id is a user, else the refusal. A failed lookup refuses too.
pub(crate) async fn refuse(app: &App, ids: &[i64], what: &str) -> Option<String> {
    for &id in ids {
        match is_bot(app, id).await {
            Ok(false) => {}
            Ok(true) => {
                return Some(format!(
                    "<@{id}> is a bot, and only users can be {what}. Give the bot a permit on its own bot role (the role Discord made for it) instead: it gets exactly the exemptions you pick."
                ))
            }
            Err(e) => {
                tracing::warn!(?e, id, "bot check failed");
                return Some(format!(
                    "Could not check that <@{id}> is a user and not a bot, so nothing was changed. Try again."
                ));
            }
        }
    }
    None
}

fn uid(u: &Value) -> Option<i64> {
    u.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| u.as_i64())
}

fn qw_ids(doc: &Value) -> Vec<i64> {
    let w = doc["antinuke"]["quarantine_whitelist"].as_array();
    w.into_iter().flatten().filter_map(uid).collect()
}

/// GuildCreate: takes bots that are already Extra Owner, Trusted Admin or on the quarantine whitelist
/// off those lists, as one config version, then logs it and DMs the owner. `members` is the
/// GuildCreate's member list; ids not in it are looked up. A failed lookup keeps the id.
pub(crate) async fn drop_bots(app: &App, gid: Id<GuildMarker>, members: &[Member]) -> Result<()> {
    let g = gid.get() as i64;
    let mut listed: Vec<i64> = sqlx::query_scalar(
        "SELECT user_id FROM extra_owners WHERE guild_id = $1 UNION SELECT user_id FROM trusted_admins WHERE guild_id = $1",
    )
    .bind(g)
    .fetch_all(&app.pool)
    .await?;
    let cfg: Option<Value> = sqlx::query_scalar(
        "SELECT config FROM guild_config WHERE guild_id = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(g)
    .fetch_optional(&app.pool)
    .await?;
    listed.extend(cfg.as_ref().map(qw_ids).unwrap_or_default());
    listed.sort();
    listed.dedup();
    let mut bots = vec![];
    let mut unread = vec![];
    for id in listed {
        let known = members.iter().find(|m| m.user.id.get() as i64 == id);
        let bot = match known {
            Some(m) => Ok(m.user.bot),
            None => is_bot(app, id).await,
        };
        match bot {
            Ok(true) => bots.push(id),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(?e, id, %gid, "bot check failed, the id stays listed");
                unread.push(format!("<@{id}>"));
            }
        }
    }
    if !unread.is_empty() {
        let text = format!(
            "Could not check if {} are bots, so they stay listed. Only users can be Extra Owner, Trusted Admin or on the quarantine whitelist; the check runs again at the next start.",
            unread.join(", ")
        );
        let log = embed::Log::new("🤖 Bot check failed", ORANGE).text(&text);
        if let Err(e) = crate::events::send_log(app, gid.get(), LogKind::AntiNuke, log).await {
            tracing::warn!(?e, %gid, "bot check log failed");
        }
    }
    if bots.is_empty() {
        return Ok(());
    }
    let (mut tx, version, mut doc) = crate::settings::locked(app, gid).await?;
    let mut removed = vec![];
    for (table, label) in [
        ("extra_owners", "Extra Owner"),
        ("trusted_admins", "Trusted Admin"),
    ] {
        let gone: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE guild_id = $1 AND user_id = ANY($2) RETURNING user_id"
        )))
        .bind(g)
        .bind(&bots)
        .fetch_all(&mut *tx)
        .await?;
        removed.extend(gone.into_iter().map(|u| format!("<@{u}> ({label})")));
    }
    let qw: Vec<i64> = qw_ids(&doc)
        .into_iter()
        .filter(|u| bots.contains(u))
        .collect();
    if let Some(w) = doc["antinuke"]["quarantine_whitelist"].as_array_mut() {
        w.retain(|u| uid(u).is_none_or(|u| !bots.contains(&u)));
    }
    removed.extend(qw.iter().map(|u| format!("<@{u}> (quarantine whitelist)")));
    if removed.is_empty() {
        return Ok(()); // dropping tx rolls back; another writer removed them meanwhile
    }
    crate::settings::append(app, tx, gid, version, &mut doc, app.me.get()).await?;
    app.invalidate_guild(gid);
    let text = format!(
        "Bot accounts were removed: {}. Only users can be Extra Owner, Trusted Admin or on the quarantine whitelist. {BOT_PERMIT}",
        removed.join(", ")
    );
    let log = embed::Log::new("🤖 Bots removed from user-only lists", ORANGE).text(&text);
    if let Err(e) = crate::events::send_log(app, gid.get(), LogKind::AntiNuke, log).await {
        tracing::warn!(?e, %gid, "bot removal log failed");
    }
    let name = crate::reply::md(&crate::verify::guild_name(app, gid).await);
    crate::reply::dm_owner(app, gid, &format!("**{name}**: {text}")).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{edit_trusted_admins, Op};
    use crate::test_support::T;
    use serde_json::json;
    use tommybot_core::permits::Level;

    const LOGS: u64 = 940_001;

    #[tokio::test]
    async fn a_bot_cannot_be_added_as_trusted_admin() {
        let Some(t) = T::new(940).await else { return };
        let (actor, user, bot) = (t.base + 1, t.base * 10 + 1, t.base * 10 + 2);
        t.exec("INSERT INTO dash_session (id, user_id, username, access_token, csrf, step_up_until, expires_at) VALUES ('uo-' || $1::text, $1 + 1, 'o', 'x', 'x', now() + interval '5 minutes', now() + interval '1 day')").await;
        t.set(|m| m.bots.push(bot));
        let add = |v: u64| Op::Add("10".into(), format!("<@{v}>"));
        let r = edit_trusted_admins(&t.app, t.g(0), (actor, &Level::Owner), &add(bot))
            .await
            .unwrap();
        assert!(r.contains("is a bot") && r.contains("bot role"), "{r}");
        t.set(|m| m.fail_user = true);
        let r = edit_trusted_admins(&t.app, t.g(0), (actor, &Level::Owner), &add(user))
            .await
            .unwrap();
        assert!(r.contains("Could not check"), "fails closed: {r}");
        assert_eq!(
            t.count("SELECT count(*) FROM trusted_admins WHERE guild_id = $1")
                .await,
            0
        );
        t.set(|m| m.fail_user = false);
        let r = edit_trusted_admins(&t.app, t.g(0), (actor, &Level::Owner), &add(user))
            .await
            .unwrap();
        assert!(r.contains(&format!("<@{user}>")), "a user is added: {r}");
        t.cleanup().await;
    }

    #[tokio::test]
    async fn the_quarantine_whitelist_refusal_names_the_list() {
        let Some(t) = T::new(943).await else { return };
        let bot = t.base * 10 + 2;
        t.set(|m| m.bots.push(bot));
        let r = refuse(&t.app, &[bot as i64], "on the quarantine whitelist").await;
        assert!(r
            .unwrap()
            .contains("only users can be on the quarantine whitelist"));
        assert_eq!(refuse(&t.app, &[bot as i64 + 1], "x").await, None);
    }

    /// GuildCreate: bots on the three lists are removed as one config version, logged and DMed;
    /// users stay.
    #[tokio::test]
    async fn bots_already_listed_are_removed_on_guild_create() {
        let Some(t) = T::new(941).await else { return };
        let (user, bot, bot2) = (t.base * 10 + 1, t.base * 10 + 2, t.base * 10 + 3);
        let qw = json!([user.to_string(), bot2.to_string()]);
        let cfg = json!({"antinuke": {"quarantine_whitelist": qw},
                         "logging": {"routes": {"anti_nuke": LOGS.to_string()}}});
        t.server_config(0, t.owner(), cfg).await;
        let g = t.g(0).get() as i64;
        for (table, u) in [
            ("extra_owners", bot),
            ("trusted_admins", bot),
            ("trusted_admins", user),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {table} (guild_id, user_id) VALUES ($1, $2)"
            )))
            .bind(g)
            .bind(u as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        }
        t.set(|m| {
            // `bot` is known only from the GuildCreate payload.
            m.bots.push(bot2);
            m.text_channels.insert(LOGS, (g as u64, false));
            m.members
                .insert((g as u64, crate::test_support::ME), vec![]);
        });
        let member = |id: u64, is_bot: bool| -> Member {
            serde_json::from_value(json!({
                "roles": [], "deaf": false, "mute": false, "flags": 0,
                "joined_at": "2026-01-01T00:00:00.000000+00:00",
                "user": {"id": id.to_string(), "username": "x", "discriminator": "0",
                         "avatar": null, "bot": is_bot},
            }))
            .unwrap()
        };
        let members = [member(user, false), member(bot, true)];
        drop_bots(&t.app, t.g(0), &members).await.unwrap();
        assert_eq!(
            t.calls(&format!("GET /users/{bot}")),
            0,
            "read from the payload"
        );
        assert_eq!(t.calls(&format!("GET /users/{user}")), 0);
        let users = |table: &'static str| {
            let pool = t.app.pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                    "SELECT user_id FROM {table} WHERE guild_id = $1"
                )))
                .bind(g)
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        assert!(users("extra_owners").await.is_empty());
        assert_eq!(users("trusted_admins").await, vec![user as i64]);
        let (v, c): (i32, Value) = sqlx::query_as(
            "SELECT version, config FROM guild_config WHERE guild_id = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(g)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
        assert_eq!(v, 2, "one config version");
        assert_eq!(
            c["antinuke"]["quarantine_whitelist"],
            json!([user.to_string()])
        );
        assert_eq!(c["permits"]["ta"], json!([user.to_string()]));
        assert_eq!(t.calls(&format!("POST /channels/{LOGS}/messages")), 1);
        assert_eq!(t.calls("POST /users/@me/channels"), 1, "the owner is DMed");
        // Nothing left: a second GuildCreate writes no version.
        drop_bots(&t.app, t.g(0), &[]).await.unwrap();
        let n = t.count("SELECT count(*) FROM guild_config WHERE guild_id = $1");
        assert_eq!(n.await, 2);
        t.cleanup().await;
    }

    #[tokio::test]
    async fn a_failed_lookup_keeps_the_id_listed() {
        let Some(t) = T::new(942).await else { return };
        let bot = t.base * 10 + 2;
        let logs = json!({"routes": {"anti_nuke": LOGS.to_string()}});
        t.server_config(0, t.owner(), json!({ "logging": logs }))
            .await;
        t.exec("INSERT INTO trusted_admins (guild_id, user_id) VALUES ($1, $1 * 10 + 2)")
            .await;
        let g = t.g(0).get();
        t.set(|m| {
            m.bots.push(bot);
            m.fail_user = true;
            m.text_channels.insert(LOGS, (g, false));
            m.members.insert((g, crate::test_support::ME), vec![]);
        });
        drop_bots(&t.app, t.g(0), &[]).await.unwrap();
        let n = t.count("SELECT count(*) FROM trusted_admins WHERE guild_id = $1");
        assert_eq!(n.await, 1);
        let logged = t.calls(&format!("POST /channels/{LOGS}/messages"));
        assert_eq!(logged, 1, "the failed check is logged");
        t.cleanup().await;
    }
}

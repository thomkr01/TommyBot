use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::{self, Sess},
    ApiError, AppState,
};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use tommybot_antinuke::{
    security::{
        checks, expression_targets, move_under_bot, overwrite_targets, private_overwrites,
        setting_fix, strip_targets, stripped, stripped_overwrite, synced_children, GuildSafety,
        Setup, EXPRESSIONS, EXPRESSION_PERMS, PRIVATE, QUARANTINE_CHANNELS,
    },
    Inverse, RoleImg,
};

type ActionRow = (
    i64,
    i64,
    String,
    Option<i64>,
    Option<Value>,
    bool,
    chrono::DateTime<chrono::Utc>,
);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/score", get(score))
        .route("/api/guilds/{guild}/score/fix", post(fix))
        .route("/api/guilds/{guild}/actions", get(actions))
        .route("/api/guilds/{guild}/revert", post(revert))
        .route("/api/guilds/{guild}/rescue", get(rescue_status))
        .route(
            "/api/guilds/{guild}/rescue/regenerate",
            post(rescue_regenerate),
        )
        .route("/api/rescue/claim", post(rescue_claim))
}

#[derive(Deserialize, Clone)]
pub(crate) struct Role {
    pub id: String,
    pub position: i64,
    #[serde(default)]
    pub managed: bool,
    permissions: String,
}

impl Role {
    pub fn perms(&self) -> u64 {
        self.permissions.parse().unwrap_or(0)
    }

    /// [`tommybot_core::roles::unfit`]; `position` is already a rank.
    pub fn unfit(&self, guild: i64, bot_pos: i64) -> Option<&'static str> {
        let id = self.id.parse().unwrap_or(0);
        tommybot_core::roles::unfit(guild as u64, id, self.managed, self.position, bot_pos)
    }
}

/// Positions are ranks in Discord's order (ties broken by id), as in `recovery::live_image`.
async fn roles_of(st: &AppState, guild: i64) -> Result<Vec<Role>, ApiError> {
    let mut roles: Vec<Role> = serde_json::from_value(
        st.discord
            .call(Method::GET, &format!("/guilds/{guild}/roles"), None)
            .await?,
    )
    .map_err(anyhow::Error::from)?;
    let rank = tommybot_core::roles::ranks(
        roles
            .iter()
            .map(|r| (r.id.parse().unwrap_or(0), r.position)),
        guild as u64,
    );
    for r in &mut roles {
        r.position = rank[&r.id.parse().unwrap_or(0)];
    }
    Ok(roles)
}

/// Guild roles and the position of the bot's top role.
pub(crate) async fn roles_and_bot_pos(
    st: &AppState,
    guild: i64,
) -> Result<(Vec<Role>, i64), ApiError> {
    let (roles, mine) = roles_and_mine(st, guild).await?;
    let bot_pos = top_of(&roles, &mine);
    Ok((roles, bot_pos))
}

/// Highest position among the roles with ids in `mine` (0 = @everyone only).
fn top_of(roles: &[Role], mine: &[String]) -> i64 {
    roles
        .iter()
        .filter(|r| mine.contains(&r.id))
        .map(|r| r.position)
        .max()
        .unwrap_or(0)
}

/// The server's roles and the ids of the bot's own roles.
async fn roles_and_mine(st: &AppState, guild: i64) -> Result<(Vec<Role>, Vec<String>), ApiError> {
    let roles = roles_of(st, guild).await?;
    let me = st.discord.call(Method::GET, "/users/@me", None).await?;
    let mine = st
        .discord
        .member_roles(guild, me["id"].as_str().unwrap_or_default())
        .await?;
    Ok((roles, mine))
}

/// What the security score reads from Discord, fetched once for the check and its fixes.
pub(crate) struct Live {
    pub safety: GuildSafety,
    pub setup: Setup,
    pub bot_pos: i64,
    pub image: tommybot_antinuke::Image,
    /// Raw role and channel JSON, kept for the backup a fix takes first.
    pub roles: Value,
    pub channels: Value,
}

pub(crate) async fn live(st: &AppState, guild: i64, config: &Value) -> Result<Live, ApiError> {
    let get = |path: String| async move { st.discord.call(Method::GET, &path, None).await };
    let (g, roles, channels, me) = tokio::try_join!(
        get(format!("/guilds/{guild}")),
        get(format!("/guilds/{guild}/roles")),
        get(format!("/guilds/{guild}/channels")),
        get("/users/@me".into()),
    )?;
    let me = me["id"].as_str().unwrap_or_default();
    let mine = st.discord.member_roles(guild, me).await?;
    let image = crate::recovery::live_image(guild, &roles, &channels);
    let bot_pos = image
        .roles
        .iter()
        .filter(|r| mine.contains(&r.id.to_string()))
        .map(|r| r.position)
        .max()
        .unwrap_or(0);
    let extra: Vec<i64> =
        sqlx::query_scalar("select user_id from extra_owners where guild_id = $1")
            .bind(guild)
            .fetch_all(&st.db)
            .await?;
    let id = |v: &Value| v.as_str().and_then(|x| x.parse::<u64>().ok());
    let n = |k: &str| g[k].as_u64().unwrap_or(0) as u8;
    Ok(Live {
        safety: GuildSafety {
            id: guild as u64,
            mfa: n("mfa_level"),
            verification: n("verification_level"),
            content_filter: n("explicit_content_filter"),
            notifications: n("default_message_notifications"),
        },
        setup: Setup {
            quarantine_role: id(&config["antinuke"]["quarantine_role"]),
            log_fallback: config["logging"]["fallback"]
                .as_str()
                .is_some_and(|f| !f.is_empty()),
            roleless_staff: tommybot_core::permits::roleless_staff(&st.db, guild).await?,
            whitelisted: serde_json::from_value::<tommybot_core::sections::WhitelistCfg>(
                config["whitelist"].clone(),
            )
            .is_ok_and(|w| !w.is_empty()),
            bot_roles: mine.iter().filter_map(|r| r.parse().ok()).collect(),
            mod_log: config["logging"]["routes"]["mod_command"].is_string(),
            // Every module defaults to off, so a missing switch reads as off.
            modules: [
                ("Anti-nuke", "antinuke"),
                ("Join gate", "joingate"),
                ("Join raid", "joinraid"),
                ("Verification", "verification"),
                ("ban_sync", "ban_sync"),
            ]
            .map(|(name, k)| (name, config[k]["enabled"] == true))
            .to_vec(),
            main_roles: config["lockdown"]["main_roles"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(id)
                .collect(),
            mod_roles: tommybot_core::permits::mod_roles(&st.db, guild)
                .await?
                .into_iter()
                .map(|r| r as u64)
                .collect(),
            trusted: extra
                .into_iter()
                .map(|u| u as u64)
                .chain(id(&g["owner_id"]))
                .chain(me.parse().ok())
                .collect(),
            log_channels: serde_json::from_value::<tommybot_logging::LogConfig>(
                config["logging"].clone(),
            )
            .unwrap_or_default()
            .channels()
            .iter()
            .map(|c| c.get())
            .collect(),
            verification_channel: id(&config["verification"]["channel"]),
        },
        bot_pos,
        image,
        roles,
        channels,
    })
}

async fn score(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let (_, config) = crate::store::current(&st.db, guild).await?;
    let l = live(&st, guild, &config).await?;
    let mut rows = checks(
        &l.safety,
        &l.image.roles,
        &l.image.channels,
        l.bot_pos,
        &l.setup,
    );
    rows.extend(crate::channels::overview_check(&st, guild).await?);
    Ok(Json(json!(rows)))
}

/// One of a row's `fix`: a role id (`*` for every strippable role and overwrite), an overwrite
/// (`channel_id` + its target in `role_id`), or a server setting.
#[derive(Deserialize)]
struct FixBody {
    role_id: Option<String>,
    channel_id: Option<String>,
    setting: Option<String>,
    #[serde(default)]
    move_under_bot: bool,
}

/// Role and overwrite strips take a backup first, so Backups can put the old permissions back.
async fn fix(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<FixBody>,
) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    let (_, c) = crate::store::current(&st.db, guild).await?;
    if b.setting.as_deref() == Some(QUARANTINE_CHANNELS) {
        let role = c["antinuke"]["quarantine_role"]
            .as_str()
            .ok_or_else(|| ApiError::BadRequest("no quarantine role".into()))?;
        // Only adds the deny; Backups is not needed to undo a stricter quarantine.
        let failed = crate::sections::quarantine_channels(&st, guild, role, &c)
            .await
            .ok_or_else(|| anyhow::anyhow!("could not list the channels"))?;
        log_change(
            &st,
            guild,
            &c,
            format!(
                "{} (<@{}>) denied the quarantine role in every channel from the security score ({failed} failed)",
                s.username, s.user_id
            ),
        )
        .await;
        return Ok(Json(json!({"ok": true, "failed": failed})));
    }
    if b.setting.as_deref() == Some(PRIVATE) {
        return make_private(&st, &s, guild, &c, b.channel_id.as_deref()).await;
    }
    // Strips roles like a role fix, so it takes the backup below.
    let expressions = b.setting.as_deref() == Some(EXPRESSIONS);
    if let Some(setting) = b.setting.filter(|_| !expressions) {
        let (key, value) =
            setting_fix(&setting).ok_or_else(|| ApiError::BadRequest("unknown setting".into()))?;
        st.discord
            .call(
                Method::PATCH,
                &format!("/guilds/{guild}"),
                Some(json!({ key: value })),
            )
            .await?;
        log_change(
            &st,
            guild,
            &c,
            format!(
                "{} (<@{}>) set `{key}` to `{value}` from the security score",
                s.username, s.user_id
            ),
        )
        .await;
        return Ok(Json(json!({"ok": true})));
    }
    let target = match b.role_id {
        Some(t) if !expressions => t,
        None if expressions => String::new(),
        _ => return Err(ApiError::BadRequest("nothing to fix".into())),
    };
    let l = live(&st, guild, &c).await?;
    let (g, roles, channels) = (guild as u64, &l.image.roles, &l.image.channels);
    let backup = || async {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO backups (guild_id, image) VALUES ($1, $2) RETURNING id",
        )
        .bind(guild)
        .bind(sqlx::types::Json(crate::recovery::live_image(
            guild,
            &l.roles,
            &l.channels,
        )))
        .fetch_one(&st.db)
        .await
    };
    if b.move_under_bot {
        let moves = target
            .parse()
            .map(|t| move_under_bot(roles, t, l.bot_pos, &l.setup))
            .unwrap_or_default();
        let Some(q) = moves
            .first()
            .and_then(|(id, _)| roles.iter().find(|r| r.id == *id))
        else {
            return Err(ApiError::BadRequest(
                "only the quarantine role can be moved, and only when a role sits between it and the bot".into(),
            ));
        };
        let backup = backup().await?;
        let body: Vec<Value> = moves
            .iter()
            .map(|(id, p)| json!({"id": id.to_string(), "position": p}))
            .collect();
        st.discord
            .call(
                Method::PATCH,
                &format!("/guilds/{guild}/roles"),
                Some(json!(body)),
            )
            .await?;
        log_change(
            &st,
            guild,
            &c,
            format!(
                "{} (<@{}>) moved role `{}` right below the bot from the security score, backup #{backup}",
                s.username, s.user_id, q.name
            ),
        )
        .await;
        return Ok(Json(json!({"ok": true, "backup": backup})));
    }
    let all = target == "*";
    let flagged = overwrite_targets(channels, roles, g, &l.setup);
    let overwrites: Vec<_> = flagged
        .iter()
        .filter(|(ch, o)| {
            !expressions
                && (all
                    || (b.channel_id.as_deref() == Some(&ch.id.to_string())
                        && o.target.to_string() == target))
        })
        .collect();
    let role_targets: Vec<&RoleImg> = if expressions {
        expression_targets(roles, g, l.bot_pos, &l.setup)
    } else if all {
        strip_targets(roles, g, l.bot_pos)
    } else if b.channel_id.is_some() {
        if overwrites.is_empty() {
            return Err(ApiError::NotFound);
        }
        vec![]
    } else {
        let r = roles
            .iter()
            .find(|r| r.id.to_string() == target)
            .ok_or(ApiError::NotFound)?;
        if r.managed {
            return Err(ApiError::BadRequest(
                "managed roles cannot be changed here".into(),
            ));
        }
        vec![r]
    };
    let backup = backup().await?;
    // Keep going past a failed call (e.g. a private channel the bot cannot see), so everything
    // that can be fixed is, and the log lists both.
    let (mut done, mut failed) = (vec![], vec![]);
    for r in &role_targets {
        let res = st
            .discord
            .call(
                Method::PATCH,
                &format!("/guilds/{guild}/roles/{}", r.id),
                Some(json!({"permissions": if expressions {
                    r.permissions & !EXPRESSION_PERMS
                } else {
                    stripped(r.id, g, r.permissions)
                }
                .to_string()})),
            )
            .await;
        let line = format!("role `{}`", r.name);
        match res {
            Ok(_) => done.push(line),
            Err(e) => failed.push(format!("{line} ({e})")),
        }
    }
    // A category fix also goes to its synced children, so they stay synced.
    for (ch, o) in &overwrites {
        let fixed = stripped_overwrite(o, g, &l.setup);
        let who = match (o.target == g, o.is_role) {
            (true, _) => "@everyone".into(),
            (_, true) => format!("<@&{}>", o.target),
            _ => format!("<@{}>", o.target),
        };
        for id in std::iter::once(ch.id).chain(synced_children(channels, ch).iter().map(|x| x.id)) {
            let res = st
                .discord
                .call(
                    Method::PUT,
                    &format!("/channels/{id}/permissions/{}", o.target),
                    Some(overwrite_body(&fixed)),
                )
                .await;
            let line = format!("<#{id}> for {who}");
            match res {
                Ok(_) => done.push(line),
                Err(e) => failed.push(format!("{line} ({e})")),
            }
        }
    }
    let mut text = format!(
        "{} (<@{}>) stripped {} permissions from {}, backup #{backup}",
        s.username,
        s.user_id,
        if expressions {
            "emoji, sticker, soundboard and event"
        } else {
            "dangerous"
        },
        if done.is_empty() {
            "nothing".into()
        } else {
            done.join(", ")
        }
    );
    if !failed.is_empty() {
        text.push_str(&format!("\nFailed: {}", failed.join(", ")));
    }
    log_change(&st, guild, &c, text).await;
    if !failed.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "Fixed {}, but could not fix: {}. Backup #{backup} holds the state before.",
            done.len(),
            failed.join(", ")
        )));
    }
    Ok(Json(json!({"ok": true, "backup": backup})))
}

/// The body of Discord's edit-channel-permissions call.
pub(crate) fn overwrite_body(o: &tommybot_antinuke::OverwriteImg) -> Value {
    json!({"type": if o.is_role { 0 } else { 1 }, "allow": o.allow.to_string(), "deny": o.deny.to_string()})
}

/// Hides a public log channel: @everyone and the main roles lose View Channel, the bot keeps access.
async fn make_private(
    st: &AppState,
    s: &Sess,
    guild: i64,
    config: &Value,
    channel: Option<&str>,
) -> Res {
    let l = live(st, guild, config).await?;
    let ch = l
        .image
        .channels
        .iter()
        .find(|x| {
            Some(x.id.to_string().as_str()) == channel && l.setup.log_channels.contains(&x.id)
        })
        .ok_or(ApiError::NotFound)?;
    // A backup first, so Backups can put the old overwrites back.
    let backup: i64 =
        sqlx::query_scalar("INSERT INTO backups (guild_id, image) VALUES ($1, $2) RETURNING id")
            .bind(guild)
            .bind(sqlx::types::Json(crate::recovery::live_image(
                guild,
                &l.roles,
                &l.channels,
            )))
            .fetch_one(&st.db)
            .await?;
    let bot = st.discord.client_id.parse().unwrap_or_default();
    for o in private_overwrites(ch, guild as u64, bot, &l.setup.main_roles) {
        st.discord
            .call(
                Method::PUT,
                &format!("/channels/{}/permissions/{}", ch.id, o.target),
                Some(overwrite_body(&o)),
            )
            .await?;
    }
    log_change(
        st,
        guild,
        config,
        format!(
            "{} (<@{}>) made the log channel <#{}> private from the security score, backup #{backup}",
            s.username, s.user_id, ch.id
        ),
    )
    .await;
    Ok(Json(json!({"ok": true, "backup": backup})))
}

#[derive(Deserialize)]
struct ActionsQ {
    actor: Option<i64>,
    hours: Option<i32>,
}

async fn actions(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(q): Query<ActionsQ>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let rows: Vec<ActionRow> = sqlx::query_as(
        "select id, actor_id, action, target_id, inverse, reverted, created_at from action_log
         where guild_id = $1 and ($2::bigint is null or actor_id = $2)
           and ($3::int is null or created_at > now() - make_interval(hours => $3))
         order by created_at desc, id desc limit 200",
    )
    .bind(guild)
    .bind(q.actor)
    .bind(q.hours)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows.iter().map(|(id, actor, action, target, inverse, reverted, at)| json!({
        "id": id, "actor_id": actor.to_string(), "action": action, "target_id": target.map(|t| t.to_string()),
        "inverse": inverse, "reverted": reverted, "at": at})).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct RevertBody {
    ids: Vec<i64>,
    /// Also undo what an Owner, Extra Owner or Trusted Admin did; only owners may.
    #[serde(default)]
    owners: bool,
}

/// Ok(false): a server name or icon revert skipped, since the server no longer has the value the
/// entry set (renamed again since).
async fn run_inverse(st: &AppState, guild: i64, inv: &Inverse) -> anyhow::Result<bool> {
    let live = match inv {
        Inverse::GuildName { from: Some(_), .. } | Inverse::GuildIcon { from: Some(_), .. } => {
            st.discord
                .call(Method::GET, &format!("/guilds/{guild}"), None)
                .await?
        }
        _ => Value::Null,
    };
    let still = match inv {
        Inverse::GuildName { from, .. } => {
            tommybot_antinuke::still_set(from, &live["name"].as_str().unwrap_or_default().into())
        }
        Inverse::GuildIcon { from, .. } => {
            tommybot_antinuke::still_set(from, &live["icon"].as_str().map(String::from))
        }
        _ => true,
    };
    if !still {
        return Ok(false);
    }
    let (m, path, body) = match inv {
        // The old icon is downloaded first, so it cannot be a plain request.
        Inverse::GuildIcon { hash, .. } => {
            let icon = match hash {
                Some(h) => Some(
                    tommybot_automod::discord_api::icon_data_uri(&st.discord.http, guild as u64, h)
                        .await?,
                ),
                None => None,
            };
            (
                Method::PATCH,
                format!("/guilds/{guild}"),
                Some(json!({ "icon": icon })),
            )
        }
        _ => inverse_request(guild, inv),
    };
    st.discord.call(m, &path, body).await.map(|_| true)
}

/// The Discord call that undoes `inv`.
fn inverse_request(guild: i64, inv: &Inverse) -> (Method, String, Option<Value>) {
    match inv {
        Inverse::Unban { user } => (Method::DELETE, format!("/guilds/{guild}/bans/{user}"), None),
        Inverse::Ban { user } => (Method::PUT, format!("/guilds/{guild}/bans/{user}"), None),
        Inverse::Timeout { user, until } => (
            Method::PATCH,
            format!("/guilds/{guild}/members/{user}"),
            Some(
                json!({"communication_disabled_until": chrono::DateTime::from_timestamp(*until, 0).map(|t| t.to_rfc3339())}),
            ),
        ),
        Inverse::DeleteChannel { id } => (Method::DELETE, format!("/channels/{id}"), None),
        Inverse::DeleteRole { id } => (Method::DELETE, format!("/guilds/{guild}/roles/{id}"), None),
        Inverse::DeleteWebhook { id } => (Method::DELETE, format!("/webhooks/{id}"), None),
        Inverse::RestoreRoles { user, roles } => (
            Method::PATCH,
            format!("/guilds/{guild}/members/{user}"),
            Some(json!({"roles": roles.iter().map(u64::to_string).collect::<Vec<_>>()})),
        ),
        Inverse::RolePerms { id, perms } => (
            Method::PATCH,
            format!("/guilds/{guild}/roles/{id}"),
            Some(json!({"permissions": perms.to_string()})),
        ),
        Inverse::DeleteIntegration { id } => (
            Method::DELETE,
            format!("/guilds/{guild}/integrations/{id}"),
            None,
        ),
        Inverse::DeleteEmoji { id } => {
            (Method::DELETE, format!("/guilds/{guild}/emojis/{id}"), None)
        }
        Inverse::DeleteSticker { id } => (
            Method::DELETE,
            format!("/guilds/{guild}/stickers/{id}"),
            None,
        ),
        Inverse::DeleteScheduledEvent { id } => (
            Method::DELETE,
            format!("/guilds/{guild}/scheduled-events/{id}"),
            None,
        ),
        Inverse::GuildName { name, .. } => (
            Method::PATCH,
            format!("/guilds/{guild}"),
            Some(json!({ "name": name })),
        ),
        Inverse::Unquarantine { .. } | Inverse::Reinvite { .. } | Inverse::GuildIcon { .. } => {
            unreachable!("handled by the caller")
        }
    }
}

async fn revert(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<RevertBody>,
) -> Res {
    let level = require(&st, &s, guild, Level::Admin, true).await?;
    // Newest first so later changes are undone before the ones they depend on.
    let rows: Vec<(i64, i64, Value)> = sqlx::query_as(
        "select id, actor_id, inverse from action_log where guild_id = $1 and id = any($2) and not reverted and inverse is not null
         order by created_at desc, id desc")
        .bind(guild).bind(&b.ids).fetch_all(&st.db).await?;
    let mut immune: Vec<i64> = sqlx::query_scalar(
        "select user_id from extra_owners where guild_id = $1 union select user_id from trusted_admins where guild_id = $1",
    )
    .bind(guild)
    .fetch_all(&st.db)
    .await?;
    immune.extend(st.discord.guild_owner(guild).await?.parse::<i64>().ok());
    let (mut done, mut failed, mut skipped, mut reinvite, mut protected) =
        (0, vec![], vec![], vec![], vec![]);
    for (id, actor, raw) in rows {
        // An owner's or TA's actions are undone only when an owner confirms it.
        if immune.contains(&actor) && !(b.owners && level >= Level::Owner) {
            protected.push(id);
            continue;
        }
        let inv: Inverse = match serde_json::from_value(raw) {
            Ok(i) => i,
            Err(e) => {
                failed.push(json!({"id": id, "error": e.to_string()}));
                continue;
            }
        };
        match &inv {
            Inverse::Unquarantine { .. } => {
                skipped.push(id);
                continue;
            }
            Inverse::Reinvite { user } => reinvite.push(user.to_string()),
            _ => match run_inverse(&st, guild, &inv).await {
                Ok(true) => {}
                // Renamed again since: left for staff, not marked reverted.
                Ok(false) => {
                    skipped.push(id);
                    continue;
                }
                Err(e) => {
                    failed.push(json!({"id": id, "error": e.to_string()}));
                    continue;
                }
            },
        }
        // The revert unbans as the bot, so the bot cannot tell staff lifted a Raider List ban.
        if let Inverse::Unban { user } = inv {
            tommybot_core::raiders::allow(&st.db, guild, user as i64, s.user_id).await?;
            tommybot_core::jobs::cancel_user(&st.db, guild, &["unban"], user, None).await?;
        }
        sqlx::query("update action_log set reverted = true where id = $1")
            .bind(id)
            .execute(&st.db)
            .await?;
        done += 1;
    }
    {
        let (_, c) = crate::store::current(&st.db, guild).await?;
        log_change(
            &st,
            guild,
            &c,
            format!(
                "{} (<@{}>) reverted {done} action(s)",
                s.username, s.user_id
            ),
        )
        .await;
    }
    Ok(Json(
        json!({"done": done, "failed": failed, "skipped": skipped, "reinvite": reinvite,
            "protected": protected, "can_include": level >= Level::Owner}),
    ))
}

fn hash(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn rescue_status(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Owner, false).await?;
    // The key makes its holder an Extra Owner, so an Extra Owner must not mint one for later.
    if !crate::api::is_discord_owner(&st, &s, guild).await? {
        return Err(ApiError::Forbidden(
            "only the server owner can manage the rescue key",
        ));
    }
    let has_key: bool =
        sqlx::query_scalar("select exists(select 1 from rescue_key where guild_id = $1)")
            .bind(guild)
            .fetch_one(&st.db)
            .await?;
    Ok(Json(json!({"has_key": has_key})))
}

async fn rescue_regenerate(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Owner, true).await?;
    // The key makes its holder an Extra Owner, so an Extra Owner must not mint one for later.
    if !crate::api::is_discord_owner(&st, &s, guild).await? {
        return Err(ApiError::Forbidden(
            "only the server owner can manage the rescue key",
        ));
    }
    let key = auth::token();
    let secret = crate::otp::new_secret();
    sqlx::query(
        "insert into rescue_key (guild_id, key_hash, totp_secret) values ($1, $2, $3)
                 on conflict (guild_id) do update set key_hash = $2, totp_secret = $3, created_at = now()",
    )
    .bind(guild)
    .bind(hash(&key))
    .bind(&secret)
    .execute(&st.db)
    .await?;
    let uri = crate::otp::uri(&secret, &format!("rescue {guild}"))?;
    Ok(Json(json!({"key": key, "uri": uri})))
}

/// The rescue key, plus a current 6-digit code from its authenticator.
#[derive(Deserialize)]
struct Claim {
    guild_id: String,
    key: Option<String>,
    code: Option<String>,
}

/// Claim attempts per user in the last 10 minutes.
static CLAIMS: LazyLock<Mutex<HashMap<i64, Vec<Instant>>>> = LazyLock::new(Default::default);
const MAX_CLAIMS: usize = 5;

fn claim_allowed(user: i64) -> bool {
    let mut m = CLAIMS.lock().unwrap();
    let v = m.entry(user).or_default();
    v.retain(|t| t.elapsed() < Duration::from_secs(600));
    v.push(Instant::now());
    v.len() <= MAX_CLAIMS
}

async fn claim_key(
    db: &sqlx::PgPool,
    guild: i64,
    user: i64,
    (key, code): (Option<&str>, Option<&str>),
) -> Result<(), ApiError> {
    let mut tx = db.begin().await?;
    // Both are needed: the key alone (a leaked note) or the phone alone is not enough.
    let secret: Option<Option<String>> = sqlx::query_scalar(
        "select totp_secret from rescue_key where guild_id = $1 and key_hash = $2 for update",
    )
    .bind(guild)
    .bind(hash(key.unwrap_or_default()))
    .fetch_optional(&mut *tx)
    .await?;
    let ok = match secret {
        Some(Some(s)) => match code {
            Some(c) => crate::otp::step(&s, c)?.is_some(),
            None => false,
        },
        // Keys made before the authenticator existed have no secret.
        Some(None) => true,
        None => false,
    };
    // The delete is the single-use gate: the row lock makes a concurrent claim wait and then miss.
    if ok {
        sqlx::query("delete from rescue_key where guild_id = $1")
            .bind(guild)
            .execute(&mut *tx)
            .await?;
    }
    if !ok {
        return Err(ApiError::Forbidden("invalid rescue key or code"));
    }
    sqlx::query(
        "insert into extra_owners (guild_id, user_id) values ($1, $2) on conflict do nothing",
    )
    .bind(guild)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn rescue_claim(State(st): State<AppState>, s: Sess, Json(b): Json<Claim>) -> Res {
    let guild: i64 = b
        .guild_id
        .trim()
        .parse()
        .map_err(|_| ApiError::BadRequest("bad guild id".into()))?;
    if !claim_allowed(s.user_id) {
        return Err(ApiError::Forbidden(
            "too many attempts, try again in 10 minutes",
        ));
    }
    let (_, config) = crate::store::current(&st.db, guild).await?;
    let r = claim_key(
        &st.db,
        guild,
        s.user_id,
        (
            b.key.as_deref().map(str::trim),
            b.code.as_deref().map(str::trim),
        ),
    )
    .await;
    let what = if r.is_ok() {
        "claimed Extra Owner with the rescue key"
    } else {
        "failed a rescue claim"
    };
    log_change(
        &st,
        guild,
        &config,
        format!("{} (<@{}>) {what}", s.username, s.user_id),
    )
    .await;
    r?;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_role_permissions_are_restored_on_the_role() {
        let (m, path, body) = inverse_request(5, &Inverse::RolePerms { id: 7, perms: 8 });
        assert_eq!((m, path.as_str()), (Method::PATCH, "/guilds/5/roles/7"));
        assert_eq!(body, Some(json!({"permissions": "8"})));
    }

    #[tokio::test]
    async fn icon_revert_clears_a_new_icon_and_refuses_a_bad_hash() {
        let seen: crate::Shared<String, Value> = Default::default();
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/guilds/5",
            axum::routing::patch(move |Json(b): Json<Value>| async move {
                s2.lock().unwrap().insert("body".into(), b);
                "{}"
            })
            .get(|| async { r#"{"name":"Now","icon":null}"# }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        let st = AppState {
            db: sqlx::PgPool::connect_lazy("postgres://x@127.0.0.1:1/x").unwrap(),
            discord: crate::discord::Discord {
                http: reqwest::Client::builder().no_proxy().build().unwrap(),
                client_id: String::new(),
                client_secret: String::new(),
                bot_token: String::new(),
                redirect_uri: String::new(),
                api,
                memo: Default::default(),
            },
            secure: false,
            totp: Default::default(),
            rate: Default::default(),
            ips: Default::default(),
            vpn_logged: Default::default(),
        };
        let clear = Inverse::GuildIcon {
            hash: None,
            from: None,
        };
        assert!(run_inverse(&st, 5, &clear).await.unwrap());
        assert_eq!(seen.lock().unwrap()["body"], json!({"icon": null}));
        let bad = Inverse::GuildIcon {
            hash: Some("../../users/@me".into()),
            from: None,
        };
        assert!(run_inverse(&st, 5, &bad).await.is_err());
        // Compare-and-set: renamed again since the entry, so the old name is not put back.
        let name = |from: &str| Inverse::GuildName {
            name: "Home".into(),
            from: Some(from.into()),
        };
        assert!(!run_inverse(&st, 5, &name("Nuked")).await.unwrap());
        assert!(run_inverse(&st, 5, &name("Now")).await.unwrap());
        assert_eq!(seen.lock().unwrap()["body"], json!({"name": "Home"}));
        let icon = Inverse::GuildIcon {
            hash: None,
            from: Some(Some("0123456789abcdef0123456789abcdef".into())),
        };
        assert!(!run_inverse(&st, 5, &icon).await.unwrap());
    }

    #[test]
    fn new_inverses_call_the_matching_routes() {
        for (inv, m, path, body) in [
            (
                Inverse::DeleteIntegration { id: 7 },
                Method::DELETE,
                "/guilds/5/integrations/7",
                None,
            ),
            (
                Inverse::DeleteEmoji { id: 7 },
                Method::DELETE,
                "/guilds/5/emojis/7",
                None,
            ),
            (
                Inverse::DeleteSticker { id: 7 },
                Method::DELETE,
                "/guilds/5/stickers/7",
                None,
            ),
            (
                Inverse::DeleteScheduledEvent { id: 7 },
                Method::DELETE,
                "/guilds/5/scheduled-events/7",
                None,
            ),
            (
                Inverse::GuildName {
                    name: "Home".into(),
                    from: None,
                },
                Method::PATCH,
                "/guilds/5",
                Some(json!({"name": "Home"})),
            ),
        ] {
            assert_eq!(inverse_request(5, &inv), (m, path.to_string(), body));
        }
    }

    #[tokio::test]
    async fn claim_needs_key_and_code_once() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_200_000_000 + std::process::id() as i64;
        let secret = crate::otp::new_secret();
        sqlx::query("insert into rescue_key (guild_id, key_hash, totp_secret) values ($1, $2, $3) on conflict (guild_id) do update set key_hash = $2, totp_secret = $3")
            .bind(g).bind(hash("k")).bind(&secret).execute(&db).await.unwrap();
        let code = crate::otp::current(&secret);
        assert!(claim_key(&db, g, 6, (None, Some(&code))).await.is_err());
        assert!(claim_key(&db, g, 6, (Some("k"), None)).await.is_err());
        claim_key(&db, g, 6, (Some("k"), Some(&code)))
            .await
            .unwrap();
        assert!(claim_key(&db, g, 6, (Some("k"), Some(&code)))
            .await
            .is_err());
        sqlx::query("delete from extra_owners where guild_id = $1")
            .bind(g)
            .execute(&db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn claim_is_single_use() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_100_000_000 + std::process::id() as i64;
        sqlx::query("insert into rescue_key (guild_id, key_hash) values ($1, $2) on conflict (guild_id) do update set key_hash = $2").bind(g).bind(hash("k")).execute(&db).await.unwrap();
        assert!(matches!(
            claim_key(&db, g, 5, (Some("bad"), None)).await,
            Err(ApiError::Forbidden(_))
        ));
        claim_key(&db, g, 5, (Some("k"), None)).await.unwrap();
        assert!(matches!(
            claim_key(&db, g, 5, (Some("k"), None)).await,
            Err(ApiError::Forbidden(_))
        ));
        let n: i64 = sqlx::query_scalar(
            "select count(*) from extra_owners where guild_id = $1 and user_id = 5",
        )
        .bind(g)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 1);
        sqlx::query("delete from extra_owners where guild_id = $1")
            .bind(g)
            .execute(&db)
            .await
            .unwrap();
    }
}

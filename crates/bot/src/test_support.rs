//! Test harness shared by the DB tests: a scratch Postgres (DATABASE_URL) and a small local mock
//! of Discord's REST API. The tests run only when DATABASE_URL is set.
use crate::app::App;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use twilight_model::id::{marker::GuildMarker, Id};

pub(crate) const ME: u64 = 1;
pub(crate) const BOT_OWNER: u64 = 2;

#[derive(Default)]
pub(crate) struct Mock {
    pub(crate) bans: HashMap<(u64, u64), String>,
    pub(crate) owners: HashMap<u64, u64>,
    /// Members by (guild, user) with their role ids.
    pub(crate) members: HashMap<(u64, u64), Vec<u64>>,
    pub(crate) fail_get: bool,
    pub(crate) fail_put: bool,
    pub(crate) fail_put_5xx: bool,
    pub(crate) fail_member: bool,
    /// Member counts by guild; 500 when not set.
    pub(crate) sizes: HashMap<u64, u64>,
    /// Guilds where the bot (ME) joined only now.
    pub(crate) new_bot: Vec<u64>,
    /// User ids that are bot accounts.
    pub(crate) bots: Vec<u64>,
    pub(crate) fail_user: bool,
    /// Roles by guild: (id, managed, permissions, position). A member role list naming a role
    /// missing here is refused, as Discord does, once the guild has any.
    pub(crate) roles: HashMap<u64, Vec<(u64, bool, u64, i64)>>,
    /// Channels whose messages fail (a log channel the bot cannot see).
    pub(crate) fail_channels: Vec<u64>,
    /// Role permission edits fail (the role is above the bot).
    pub(crate) fail_role_edit: bool,
    /// Members by (guild, user) timed out until this ISO time.
    pub(crate) timeouts: HashMap<(u64, u64), String>,
    /// Managed role -> the bot it belongs to (its tags.bot_id).
    pub(crate) bot_roles: HashMap<u64, u64>,
    /// Role -> its tags as Discord's JSON, over `bot_roles` (linked or subscription roles).
    pub(crate) role_tags: HashMap<u64, Value>,
    /// Log and DM messages fail with 403.
    pub(crate) fail_message: bool,
    /// Log and DM messages fail with 502.
    pub(crate) fail_message_5xx: bool,
    /// Channel deletes fail with 500.
    pub(crate) fail_delete_channel: bool,
    /// Audit log entries any audit log lookup returns.
    pub(crate) audit: Vec<Value>,
    /// The next this many audit log lookups fail with 500.
    pub(crate) fail_audit: u32,
    /// Permission overwrites by channel, as a channel lookup returns them.
    pub(crate) overwrites: HashMap<u64, Vec<Value>>,
    /// The guild an `overwrites` channel answers with; none when not set.
    pub(crate) channel_guild: HashMap<u64, u64>,
    /// Channels the mock can GET: id -> (guild, whether @everyone may view it).
    pub(crate) text_channels: HashMap<u64, (u64, bool)>,
    /// Member role-list edits fail with 500.
    pub(crate) fail_member_edit: bool,
    /// Channels by guild, as Discord's JSON array; `[]` when not set.
    pub(crate) channels: HashMap<u64, Value>,
    /// Audit log entries by guild, newest first; `audit` when not set.
    pub(crate) audit_by_guild: HashMap<u64, Vec<Value>>,
    /// Native AutoMod rules by guild, as Discord's JSON.
    pub(crate) rules: HashMap<u64, Vec<Value>>,
    /// AutoMod rule lookups fail with 500.
    pub(crate) fail_rules: bool,
    /// Onboarding by guild, as Discord's JSON; 404 (none set up) when not set.
    pub(crate) onboarding: HashMap<u64, Value>,
    pub(crate) log: Vec<String>,
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (vec![], 0);
    while i < b.len() {
        match (
            b[i],
            s.get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok()),
        ) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into()
}

fn guild(id: &str, owner: u64, members: u64) -> Value {
    json!({
        "id": id, "name": format!("Server {id}"), "owner_id": owner.to_string(), "afk_timeout": 300,
        "approximate_member_count": members,
        "default_message_notifications": 0, "emojis": [], "explicit_content_filter": 0, "features": [],
        "large": false, "mfa_level": 0, "nsfw_level": 0, "preferred_locale": "en-US",
        "premium_progress_bar_enabled": false, "roles": [], "stickers": [], "stage_instances": [],
        "system_channel_flags": 0, "verification_level": 0,
    })
}

fn role((id, managed, perms, position): (u64, bool, u64, i64), bot: Option<&u64>) -> Value {
    let tags = bot.map(|b| json!({"bot_id": b.to_string()}));
    json!({"tags": tags, "id": id.to_string(), "name": format!("r{id}"), "color": 0, "colors": {"primary_color": 0}, "hoist": false, "managed": managed,
           "mentionable": false, "permissions": perms.to_string(), "position": position, "flags": 0})
}

fn route(m: &mut Mock, method: &str, target: &str, reason: String, body: &str) -> (u16, String) {
    m.log.push(format!("{method} {target} {body}"));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let q = |k: &str| {
        query
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{k}=")))
            .and_then(|v| v.parse::<u64>().ok())
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let user = |id: &str| json!({"id": id, "username": "u", "discriminator": "0", "avatar": null});
    let err = |s: u16, code: u64| (s, json!({"code": code, "message": "x"}).to_string());
    let n = |s: &str| s.parse::<u64>().unwrap();
    let bot = m.bots.clone();
    let timeouts = m.timeouts.clone();
    let member = |g: &str, u: &str, roles: &[u64]| {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        let mut usr = user(u);
        usr["bot"] = json!(bot.iter().any(|b| b.to_string() == u));
        let until = timeouts.get(&(n(g), n(u)));
        json!({"user": usr, "roles": roles, "joined_at": "2020-01-01T00:00:00.000000+00:00",
               "deaf": false, "mute": false, "flags": 0, "communication_disabled_until": until})
    };
    match (method, parts.as_slice()) {
        ("GET", ["guilds", _, "bans", _]) if m.fail_get => err(500, 0),
        ("GET", ["guilds", g, "bans", u]) => match m.bans.get(&(n(g), n(u))) {
            Some(r) => (200, json!({"reason": r, "user": user(u)}).to_string()),
            None => err(404, 10026),
        },
        ("GET", ["guilds", g, "bans"]) => {
            let mut all: Vec<_> = m.bans.iter().filter(|b| b.0 .0 == n(g)).collect();
            all.sort_by_key(|b| b.0 .1);
            let page: Vec<Value> = all
                .into_iter()
                .filter(|b| b.0 .1 > q("after").unwrap_or(0))
                .take(q("limit").unwrap_or(1000) as usize)
                .map(|b| json!({"reason": b.1, "user": user(&b.0 .1.to_string())}))
                .collect();
            (200, json!(page).to_string())
        }
        ("PUT", ["guilds", _, "bans", _]) if m.fail_put => err(403, 50013),
        ("PUT", ["guilds", _, "bans", _]) if m.fail_put_5xx => err(502, 0),
        // Discord keeps the first ban's reason when a banned user is banned again (live 11-13).
        ("PUT", ["guilds", g, "bans", u]) => {
            m.bans.entry((n(g), n(u))).or_insert(reason);
            (204, String::new())
        }
        ("DELETE", ["guilds", g, "bans", u]) => match m.bans.remove(&(n(g), n(u))) {
            Some(_) => (204, String::new()),
            None => err(404, 10026),
        },
        ("GET", ["guilds", _, "members", _]) if m.fail_member => err(500, 0),
        ("GET", ["guilds", g, "members", u]) => match m.members.get(&(n(g), n(u))) {
            Some(r) if n(u) == ME && m.new_bot.contains(&n(g)) => {
                let mut j = member(g, u, r);
                j["joined_at"] = json!("2099-01-01T00:00:00.000000+00:00");
                (200, j.to_string())
            }
            Some(r) => (200, member(g, u, r).to_string()),
            None => err(404, 10007),
        },
        ("GET", ["guilds", g, "members"]) => {
            let mut all: Vec<_> = m.members.iter().filter(|x| x.0 .0 == n(g)).collect();
            all.sort_by_key(|x| x.0 .1);
            let page: Vec<Value> = all
                .into_iter()
                .filter(|x| x.0 .1 > q("after").unwrap_or(0))
                .take(q("limit").unwrap_or(1) as usize)
                .map(|x| member(g, &x.0 .1.to_string(), x.1))
                .collect();
            (200, json!(page).to_string())
        }
        ("PATCH", ["guilds", g, "members", u])
            if m.members.contains_key(&(n(g), n(u)))
                && body.contains("communication_disabled_until") =>
        {
            let b: Value = serde_json::from_str(body).unwrap_or_default();
            match b["communication_disabled_until"].as_str() {
                Some(t) => m.timeouts.insert((n(g), n(u)), t.into()),
                None => m.timeouts.remove(&(n(g), n(u))),
            };
            let roles = m.members[&(n(g), n(u))].clone();
            let mut j = member(g, u, &roles);
            j["communication_disabled_until"] = b["communication_disabled_until"].clone();
            (200, j.to_string())
        }
        ("DELETE", ["channels", _]) if m.fail_delete_channel => err(500, 0),
        ("GET", ["guilds", _, "audit-logs"]) if m.fail_audit > 0 => {
            m.fail_audit -= 1;
            err(500, 0)
        }
        ("GET", ["guilds", g, "audit-logs"]) => {
            let entries = m
                .audit_by_guild
                .get(&n(g))
                .cloned()
                .unwrap_or_else(|| m.audit.clone());
            let log = json!({"audit_log_entries": entries, "users": [], "webhooks": [],
                "integrations": [], "threads": [], "application_commands": [],
                "auto_moderation_rules": [], "guild_scheduled_events": []});
            (200, log.to_string())
        }
        ("DELETE", ["guilds", g, "members", u]) => match m.members.remove(&(n(g), n(u))) {
            Some(_) => (204, String::new()),
            None => err(404, 10007),
        },
        ("GET", ["channels", c]) if m.text_channels.contains_key(&n(c)) => {
            let (g, open) = m.text_channels[&n(c)];
            let (allow, deny) = if open { ("1024", "0") } else { ("0", "1024") };
            let ch = json!({"id": c, "type": 0, "guild_id": g.to_string(), "name": "log", "position": 0,
                "permission_overwrites": [{"id": g.to_string(), "type": 0, "allow": allow, "deny": deny}]});
            (200, ch.to_string())
        }
        ("GET", ["guilds", g]) => match m.owners.get(&n(g)) {
            Some(o) => {
                let size = m.sizes.get(&n(g)).copied().unwrap_or(500);
                (200, guild(g, *o, size).to_string())
            }
            None => err(404, 10004),
        },
        ("GET", ["users", _]) if m.fail_user => err(502, 0),
        ("GET", ["users", u]) => {
            let mut j = user(u);
            j["bot"] = json!(m.bots.iter().any(|b| b.to_string() == *u));
            (200, j.to_string())
        }
        ("GET", ["guilds", _, "auto-moderation", ..]) if m.fail_rules => err(500, 0),
        ("GET", ["guilds", g, "auto-moderation", "rules"]) => (
            200,
            json!(m.rules.get(&n(g)).cloned().unwrap_or_default()).to_string(),
        ),
        ("GET", ["guilds", g, "auto-moderation", "rules", r]) => {
            match m
                .rules
                .get(&n(g))
                .into_iter()
                .flatten()
                .find(|x| x["id"] == *r)
            {
                Some(x) => (200, x.to_string()),
                None => err(404, 10066),
            }
        }
        ("GET", ["guilds", g, "roles"]) => {
            let all = m.roles.get(&n(g)).cloned().unwrap_or_default();
            let all: Vec<Value> = all
                .iter()
                .map(|r| {
                    let mut v = role(*r, m.bot_roles.get(&r.0));
                    if let Some(t) = m.role_tags.get(&r.0) {
                        v["tags"] = t.clone();
                    }
                    v
                })
                .collect();
            (200, json!(all).to_string())
        }
        ("GET", ["guilds", g, "onboarding"]) => match m.onboarding.get(&n(g)) {
            Some(o) => (200, o.to_string()),
            None => err(404, 10004),
        },
        ("GET", ["guilds", g, "channels"]) => match m.channels.get(&n(g)) {
            Some(c) => (200, c.to_string()),
            None => (200, "[]".into()),
        },
        // Only channels a test gave overwrites; others answer 404 as before.
        ("GET", ["channels", c]) if m.overwrites.contains_key(&n(c)) => {
            let ows = m.overwrites[&n(c)].clone();
            let mut j = json!({"id": c, "type": 0, "permission_overwrites": ows});
            if let Some(g) = m.channel_guild.get(&n(c)) {
                j["guild_id"] = json!(g.to_string());
            }
            (200, j.to_string())
        }
        ("PATCH", ["guilds", _, "roles", _]) if m.fail_role_edit => err(403, 50013),
        ("PATCH", ["guilds", g, "roles", r]) => {
            let perms = serde_json::from_str::<Value>(body).unwrap_or_default()["permissions"]
                .as_str()
                .and_then(|p| p.parse().ok());
            let bot = m.bot_roles.get(&n(r)).copied();
            let all = m.roles.entry(n(g)).or_default();
            match all.iter_mut().find(|x| x.0 == n(r)) {
                Some(x) => {
                    x.2 = perms.unwrap_or(x.2);
                    (200, role(*x, bot.as_ref()).to_string())
                }
                None => err(404, 10011),
            }
        }
        ("PATCH", ["guilds", _, "members", _]) if m.fail_member_edit => err(500, 0),
        ("PATCH", ["guilds", g, "members", u]) => {
            let ids: Option<Vec<u64>> = serde_json::from_str::<Value>(body).unwrap_or_default()
                ["roles"]
                .as_array()
                .map(|a| a.iter().filter_map(|r| r.as_str()?.parse().ok()).collect());
            let known = m.roles.get(&n(g)).cloned().unwrap_or_default();
            let Some(have) = m.members.get_mut(&(n(g), n(u))) else {
                return err(404, 10007);
            };
            if let Some(ids) = ids {
                if !known.is_empty() && ids.iter().any(|r| !known.iter().any(|k| k.0 == *r)) {
                    return err(400, 10011);
                }
                *have = ids;
            }
            let have = have.clone();
            (200, member(g, u, &have).to_string())
        }
        ("PUT", ["guilds", g, "members", u, "roles", r]) => {
            match m.members.get_mut(&(n(g), n(u))) {
                Some(have) => {
                    if !have.contains(&n(r)) {
                        have.push(n(r));
                    }
                    (204, String::new())
                }
                None => err(404, 10007),
            }
        }
        ("DELETE", ["guilds", g, "members", u, "roles", r]) => {
            let managed = m
                .roles
                .get(&n(g))
                .is_some_and(|all| all.iter().any(|x| x.0 == n(r) && x.1));
            match m.members.get_mut(&(n(g), n(u))) {
                _ if managed => err(403, 50013),
                Some(have) => {
                    have.retain(|x| *x != n(r));
                    (204, String::new())
                }
                None => err(404, 10007),
            }
        }
        ("PUT" | "DELETE", ["channels", _, "permissions", _]) => (204, String::new()),
        ("POST", ["guilds", g, "channels"]) => {
            let ch = json!({"id": "610", "type": 0, "guild_id": g, "name": "ticket"});
            (200, ch.to_string())
        }
        ("POST", ["channels", c, "messages"]) if m.fail_channels.contains(&n(c)) => err(403, 50001),
        ("POST", ["channels", _, "messages"]) if m.fail_message => err(403, 50013),
        ("POST", ["channels", _, "messages"]) if m.fail_message_5xx => err(502, 0),
        ("POST", ["users", "@me", "channels"]) => (200, json!({"id": "77", "type": 1}).to_string()),
        ("POST", ["channels", c, "messages"]) => {
            let msg = json!({"id": "1", "channel_id": c, "author": user("1"), "content": "",
                "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false,
                "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [],
                "embeds": [], "components": [], "pinned": false, "type": 0});
            (200, msg.to_string())
        }
        _ => err(404, 10004),
    }
}

async fn answer(mut s: TcpStream, mock: Arc<StdMutex<Mock>>) {
    let (mut buf, mut tmp) = (vec![], [0u8; 4096]);
    let end = loop {
        let n = s.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let header = |name: &str| {
        head.lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            })
            .map(|(_, v)| v.trim().to_string())
    };
    let len: usize = header("content-length")
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    while buf.len() < end + len {
        let n = s.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[end..]).to_string();
    let mut first = head.split_whitespace();
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let target = target.split_once("/api/v10").map_or(target, |p| p.1);
    let reason = decode(&header("x-audit-log-reason").unwrap_or_default());
    let (status, out) = route(&mut mock.lock().unwrap(), method, target, reason, &body);
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
        out.len()
    );
    let _ = s.write_all(resp.as_bytes()).await;
    let _ = s.shutdown().await;
}

pub(crate) struct T {
    pub(crate) app: App,
    pub(crate) mock: Arc<StdMutex<Mock>>,
    pub(crate) base: u64,
}

impl T {
    /// `n` (below 1000) picks this test's guild ids, so it must be unique across every test file using `T`.
    pub(crate) async fn new(n: u64) -> Option<T> {
        assert!(
            n < 1000,
            "test number {n} would leave this process's guild ids"
        );
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let mock = Arc::new(StdMutex::new(Mock::default()));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let m = mock.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                tokio::spawn(answer(s, m.clone()));
            }
        });
        let http = || {
            twilight_http::Client::builder()
                .proxy(addr.clone(), true)
                .ratelimiter(None)
                .token("Bot test".into())
                .build()
        };
        let mut app = App::new(
            http(),
            http(),
            pool,
            tommybot_automod::discord_api::Api::new(""),
            Id::new(ME),
        );
        app.bot_owner = Some(Id::new(BOT_OWNER));
        let t = T {
            app,
            mock,
            // Ten guild ids per test number; n < 1000 stays inside this process's 10_000 ids, so
            // test binaries running side by side never share a guild.
            base: 3_300_000_000_000 + std::process::id() as u64 * 10_000 + n * 10,
        };
        t.cleanup().await;
        Some(t)
    }

    pub(crate) fn g(&self, i: u64) -> Id<GuildMarker> {
        Id::new(self.base + i)
    }

    pub(crate) fn owner(&self) -> u64 {
        self.base + 9
    }

    /// Server `i` with the whole config `cfg`, owned by `owner` in the DB, Discord and the cache.
    pub(crate) async fn server_config(&self, i: u64, owner: u64, cfg: Value) {
        sqlx::query("INSERT INTO guild_config (guild_id, version, config) VALUES ($1, 1, $2)")
            .bind(self.g(i).get() as i64)
            .bind(cfg)
            .execute(&self.app.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO guild_owners (guild_id, owner_id) VALUES ($1, $2)")
            .bind(self.g(i).get() as i64)
            .bind(owner as i64)
            .execute(&self.app.pool)
            .await
            .unwrap();
        self.app.seed_guild(self.g(i), Id::new(owner));
        self.mock
            .lock()
            .unwrap()
            .owners
            .insert(self.g(i).get(), owner);
    }

    pub(crate) fn ban(&self, i: u64, user: u64, reason: &str) {
        let mut m = self.mock.lock().unwrap();
        m.bans.insert((self.g(i).get(), user), reason.into());
    }

    pub(crate) fn banned(&self, i: u64, user: u64) -> Option<String> {
        self.mock
            .lock()
            .unwrap()
            .bans
            .get(&(self.g(i).get(), user))
            .cloned()
    }

    pub(crate) fn set(&self, f: impl FnOnce(&mut Mock)) {
        f(&mut self.mock.lock().unwrap())
    }

    pub(crate) fn calls(&self, what: &str) -> usize {
        let m = self.mock.lock().unwrap();
        m.log.iter().filter(|l| l.contains(what)).count()
    }

    /// (server index, payload, seconds until it runs) of the queued `kind` jobs, which are removed.
    pub(crate) async fn take(&self, kind: &str) -> Vec<(u64, Value, i64)> {
        let jobs: Vec<(i64, Value, i64)> = sqlx::query_as(
            "DELETE FROM scheduled_jobs WHERE kind = $1 AND guild_id BETWEEN $2 AND $2 + 9
             RETURNING guild_id, payload, extract(epoch FROM run_at - now())::bigint",
        )
        .bind(kind)
        .bind(self.base as i64)
        .fetch_all(&self.app.pool)
        .await
        .unwrap();
        jobs.into_iter()
            .map(|(g, p, s)| (g as u64 - self.base, p, s))
            .collect()
    }

    pub(crate) async fn exec(&self, sql: &'static str) {
        sqlx::query(sql)
            .bind(self.base as i64)
            .execute(&self.app.pool)
            .await
            .unwrap();
    }

    pub(crate) async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.base as i64)
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }

    pub(crate) async fn cleanup(&self) {
        for sql in [
            "DELETE FROM guild_config WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM guild_owners WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM scheduled_jobs WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM cases WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM action_log WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM quarantines WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM joingate_actions WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM extra_owners WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM trusted_admins WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM permits WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM nuke_incidents WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM dash_session WHERE user_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM raider_actions WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM raider_allow WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM automod_rule_untrusted WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM automod_rule_approved WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM guild_left WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM owner_handovers WHERE guild_id BETWEEN $1 AND $1 + 9",
            "DELETE FROM unpermittable_roles WHERE guild_id BETWEEN $1 AND $1 + 9",
        ] {
            self.exec(sql).await;
        }
    }
}

//! Tests of `bansync` (own file to keep bansync.rs short). The DB tests run only when
//! DATABASE_URL points at a scratch Postgres; Discord is a small local mock.
use super::actions::{ban_click, send_rest_click};
use super::*;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub(crate) const ME: u64 = 1;
pub(crate) const BOT_OWNER: u64 = 2;

#[test]
fn every_exemption_is_named() {
    let (me, bo, u) = (Id::new(ME), Some(Id::new(BOT_OWNER)), Id::new(50));
    assert_eq!(exempt(&Level::None, me, me, bo), Some("the bot itself"));
    assert_eq!(
        exempt(&Level::None, Id::new(BOT_OWNER), me, bo),
        Some("the bot owner")
    );
    assert_eq!(exempt(&Level::Owner, u, me, bo), Some("server owner"));
    assert_eq!(exempt(&Level::ExtraOwner, u, me, bo), Some("extra owner"));
    assert_eq!(
        exempt(&Level::TrustedAdmin, u, me, bo),
        Some("trusted admin")
    );
    let perm = |wl: &[&str]| Level::Custom {
        rank: 1,
        perms: vec!["ban".into()],
        whitelist: wl.iter().map(|w| w.to_string()).collect(),
        limits: Default::default(),
    };
    assert_eq!(
        exempt(&perm(&["antinuke"]), u, me, bo),
        Some("on the anti-nuke whitelist")
    );
    assert_eq!(exempt(&perm(&[]), u, me, bo), None);
    assert_eq!(exempt(&Level::None, u, me, None), None);
}

#[test]
fn a_copy_replaces_only_a_local_temp_ban_it_outlasts() {
    assert!(outlasts(None, Some(100)) && outlasts(Some(200), Some(100)));
    assert!(!outlasts(Some(100), Some(100)) && !outlasts(Some(50), Some(100)));
    // A permanent ban here (no unban job) is never replaced.
    assert!(!outlasts(None, None) && !outlasts(Some(200), None));
}

#[test]
fn automatic_bans_follow_their_switches() {
    let c = BanSyncCfg::default();
    for (o, on) in [
        (Origin::StaffCmd, true),
        (Origin::Discord, true),
        (Origin::JoinRaid, true),
        (Origin::Automod, true),
        (Origin::JoinGate, false),
        (Origin::Verify, false),
    ] {
        assert_eq!(sends(&c, o), on, "{o:?}");
    }
    assert_eq!(gate_origin("join raid"), Origin::JoinRaid);
    assert_eq!(gate_origin("discord automod rule"), Origin::Automod);
    assert_eq!(
        gate_origin("join gate: accounts must be at least 7d old"),
        Origin::JoinGate
    );
}

#[test]
fn the_staff_member_behind_a_ban_is_checked() {
    let me = Id::new(ME);
    assert_eq!(behind(me, Some(9), None), Behind::Staff(Id::new(9)));
    assert_eq!(behind(me, Some(ME), Some(9)), Behind::Staff(Id::new(9)));
    assert_eq!(behind(me, Some(ME), Some(ME as i64)), Behind::Bot);
    assert_eq!(behind(me, Some(ME), None), Behind::Bot);
    assert_eq!(behind(me, None, None), Behind::Unknown);
    assert_eq!(staff(Behind::Staff(Id::new(9))), Some(9));
    assert_eq!(staff(Behind::Bot), None);
}

#[test]
fn reasons_and_names_stay_one_plain_line() {
    assert_eq!(
        md("a\nb [x](y) **c** `d` ~e~ |f| >g _h_ <@1>"),
        r"a b \[x\](y) \*\*c\*\* \`d\` \~e\~ \|f\| \>g \_h\_ \<@1\>"
    );
    let r = copy_reason("Main*", 5, "spam\nlink");
    assert_eq!(r, r"Ban Sync from Main\* (5): spam link");
    assert!(ours(Some(&format!("{r} | Moderator: 1")), Id::new(5)));
    assert!(!ours(Some(&r), Id::new(6)));
    assert!(!ours(Some("spam"), Id::new(5)));
}

#[test]
fn the_ban_time_comes_from_the_audit_entry() {
    let ms = 1_700_000_000_000u64;
    let id = (ms - 1_420_070_400_000) << 22;
    assert_eq!(audit_secs(id), 1_700_000_000 - 5);
}

/// What the mock Discord holds: bans by (guild, user) with their reason, guild owners, and every request.
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
        ("GET", ["guilds", _, "audit-logs"]) => {
            let log = json!({"audit_log_entries": m.audit, "users": [], "webhooks": [],
                "integrations": [], "threads": [], "application_commands": [],
                "auto_moderation_rules": [], "guild_scheduled_events": []});
            (200, log.to_string())
        }
        ("DELETE", ["guilds", g, "members", u]) => match m.members.remove(&(n(g), n(u))) {
            Some(_) => (204, String::new()),
            None => err(404, 10007),
        },
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
        ("GET", ["guilds", g, "roles"]) => {
            let all = m.roles.get(&n(g)).cloned().unwrap_or_default();
            let all: Vec<Value> = all
                .iter()
                .map(|r| role(*r, m.bot_roles.get(&r.0)))
                .collect();
            (200, json!(all).to_string())
        }
        ("GET", ["guilds", _, "channels"]) => (200, "[]".into()),
        // Only channels a test gave overwrites; others answer 404 as before.
        ("GET", ["channels", c]) if m.overwrites.contains_key(&n(c)) => {
            let ows = m.overwrites[&n(c)].clone();
            (
                200,
                json!({"id": c, "type": 0, "permission_overwrites": ows}).to_string(),
            )
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
        ("PUT", ["channels", _, "permissions", _]) => (204, String::new()),
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
    pub(crate) async fn new(n: u64) -> Option<T> {
        assert!(n < 1000, "T::new takes 0-999");
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
            // n < 1000, ten guilds each, so every (process, n) has its own range.
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

    /// Server `i` with Ban Sync on (receiving as `receive`), owned by `owner` in the DB, Discord and the cache.
    pub(crate) async fn server(&self, i: u64, owner: u64, receive: &str) {
        self.server_cfg(i, owner, json!({"enabled": true, "receive": receive}))
            .await
    }

    pub(crate) async fn server_cfg(&self, i: u64, owner: u64, cfg: Value) {
        self.server_config(i, owner, json!({ "ban_sync": cfg }))
            .await
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

    /// (receiving server index, state, skip reason) of the rows for `user`.
    pub(crate) async fn rows(&self, user: u64) -> Vec<(u64, String, Option<String>)> {
        let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT guild_id, state, skip_reason FROM ban_sync_links
             WHERE user_id = $1 AND guild_id BETWEEN $2 AND $2 + 9 ORDER BY guild_id",
        )
        .bind(user as i64)
        .bind(self.base as i64)
        .fetch_all(&self.app.pool)
        .await
        .unwrap();
        rows.into_iter()
            .map(|(g, s, w)| (g as u64 - self.base, s, w))
            .collect()
    }

    pub(crate) fn row(i: u64, state: &str, why: Option<&str>) -> (u64, String, Option<String>) {
        (i, state.into(), why.map(Into::into))
    }

    pub(crate) async fn link(&self, to: u64, user: u64, src: u64, state: &str) {
        sqlx::query(
            "INSERT INTO ban_sync_links (guild_id, user_id, source_guild, origin, reason, state)
             VALUES ($1, $2, $3, 'discord', 'raiding', $4)",
        )
        .bind(self.g(to).get() as i64)
        .bind(user as i64)
        .bind(self.g(src).get() as i64)
        .bind(state)
        .execute(&self.app.pool)
        .await
        .unwrap();
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

    pub(crate) async fn run_copies(&self) {
        for (to, p, _) in self.take("ban_sync_copy").await {
            copy_job(&self.app, self.g(to), &p).await.unwrap();
        }
    }

    /// An audit entry made now (its id is a snowflake of the current time).
    pub(crate) fn entry(&self, i: u64, kind: u16, user: u64, actor: Option<u64>) -> AuditLogEntry {
        let id = ((now_secs() as u64 * 1000) - 1_420_070_400_000) << 22;
        serde_json::from_value(json!({
            "id": id.to_string(), "action_type": kind, "changes": [], "guild_id": self.g(i).to_string(),
            "target_id": user.to_string(), "user_id": actor.map(|a| a.to_string()), "reason": "raiding",
        }))
        .unwrap()
    }

    pub(crate) fn share(&self, user: u64, actor: Option<u64>) -> Value {
        json!({"user": user, "actor": actor, "reason": "raiding", "at": now_secs() - 61})
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
            "DELETE FROM ban_sync_links WHERE guild_id BETWEEN $1 AND $1 + 9",
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
        ] {
            self.exec(sql).await;
        }
    }
}

#[tokio::test]
async fn a_ban_is_held_then_copied_once_and_never_shared_on() {
    let Some(t) = T::new(0).await else { return };
    let o = t.owner();
    // Anti-nuke is off in all three servers.
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.server(2, o, "alert").await;
    t.ban(0, 500, "raiding");
    on_ban(&t.app, &t.entry(0, 22, 500, Some(77)))
        .await
        .unwrap();
    let held = t.take("ban_sync_share").await;
    assert_eq!(held.len(), 1);
    assert!(
        (55..=60).contains(&held[0].2),
        "after the hold: {}",
        held[0].2
    );
    let at = held[0].1["at"].as_i64().unwrap();
    assert!(
        (now_secs() - at - 5).abs() <= 1,
        "ban time from the entry id"
    );
    share_job(&t.app, t.g(0), &held[0].1).await.unwrap();
    assert_eq!(
        t.calls(&format!("GET /guilds/{} ", t.g(0))),
        1,
        "the source owner is looked up once"
    );
    let pending = T::row(1, "pending", None);
    assert_eq!(t.rows(500).await, [pending, T::row(2, "pending", None)]);
    let copies = t.take("ban_sync_copy").await;
    for (to, p, _) in copies.iter().chain(copies.iter()) {
        copy_job(&t.app, t.g(*to), p).await.unwrap();
    }
    assert_eq!(
        t.rows(500).await,
        [T::row(1, "banned", None), T::row(2, "alerted", None)]
    );
    assert!(t.banned(1, 500).unwrap().starts_with(PREFIX));
    assert_eq!(t.banned(2, 500), None, "alert only");
    assert_eq!(t.calls(&format!("PUT /guilds/{}/bans/500", t.g(1))), 1);
    let cases = t
        .count("SELECT count(*) FROM cases WHERE guild_id = $1 + 1 AND kind = 'ban'")
        .await;
    assert_eq!(cases, 1);
    // The copy's own audit event in server 1 is not shared again.
    on_ban(&t.app, &t.entry(1, 22, 500, Some(ME)))
        .await
        .unwrap();
    assert!(t.take("ban_sync_share").await.is_empty());
    t.cleanup().await;
}

#[tokio::test]
async fn a_contained_actor_a_lifted_ban_or_a_discord_error_shares_nothing() {
    let Some(t) = T::new(1).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.ban(0, 501, "raiding");
    t.exec("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 77, '{}')")
        .await;
    share_job(&t.app, t.g(0), &t.share(501, Some(77)))
        .await
        .unwrap();
    assert!(t.rows(501).await.is_empty(), "contained actor");
    // Reverted in the hold: Discord says 404.
    share_job(&t.app, t.g(0), &t.share(502, Some(78)))
        .await
        .unwrap();
    assert!(t.rows(502).await.is_empty(), "ban lifted");
    // A 5xx is no answer: the job fails and runs again.
    t.set(|m| m.fail_get = true);
    t.ban(0, 503, "raiding");
    assert!(share_job(&t.app, t.g(0), &t.share(503, Some(78)))
        .await
        .is_err());
    t.set(|m| m.fail_get = false);
    share_job(&t.app, t.g(0), &t.share(503, Some(78)))
        .await
        .unwrap();
    // Lifted between share and copy: the copy is cancelled.
    t.set(|m| m.bans.clear());
    t.run_copies().await;
    let gone = T::row(1, "cancelled", Some("source ban lifted"));
    assert_eq!(t.rows(503).await, [gone]);
    t.cleanup().await;
}

#[tokio::test]
async fn a_staff_command_whose_moderator_is_contained_syncs_nothing() {
    let Some(t) = T::new(9).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    // The bot banned for moderator 9 (case 1) and lifted for them (case 2); then 9 was quarantined.
    t.exec("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id) VALUES ($1, 1, 'ban', 520, 9), ($1, 2, 'unban', 521, 9)")
        .await;
    t.ban(0, 520, "raiding");
    t.link(1, 521, 0, "banned").await;
    t.ban(1, 521, &format!("{PREFIX} Main ({}): raiding", t.g(0)));
    t.exec("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 9, '{}')")
        .await;
    share_job(&t.app, t.g(0), &t.share(520, Some(ME)))
        .await
        .unwrap();
    assert!(t.rows(520).await.is_empty(), "ban by a contained moderator");
    lift_job(&t.app, t.g(0), &t.share(521, Some(ME)))
        .await
        .unwrap();
    assert!(t.banned(1, 521).is_some(), "unban by a contained moderator");
    // A bot ban without a case is held while a permit holder there was contained.
    t.exec("INSERT INTO trusted_admins (guild_id, user_id) VALUES ($1, 9)")
        .await;
    // An automod ban: the bot is moderator too.
    t.exec("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id) VALUES ($1, 3, 'ban', 522, 1)")
        .await;
    t.ban(0, 522, "raiding");
    share_job(&t.app, t.g(0), &t.share(522, Some(ME)))
        .await
        .unwrap();
    assert!(t.rows(522).await.is_empty());
    // Contained after the share: the copy checks again from the stored moderator.
    t.exec("DELETE FROM quarantines WHERE guild_id = $1").await;
    share_job(&t.app, t.g(0), &t.share(520, Some(ME)))
        .await
        .unwrap();
    t.exec("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 9, '{}')")
        .await;
    t.run_copies().await;
    let stopped = T::row(1, "cancelled", Some("source moderator contained"));
    assert_eq!(t.rows(520).await, [stopped]);
    t.cleanup().await;
}

#[tokio::test]
async fn over_the_hourly_cap_rows_wait_for_the_owner() {
    let Some(t) = T::new(2).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    for u in 0..BAN_SYNC_HOURLY as u64 {
        t.link(1, 10_000 + u, 0, "banned").await;
    }
    t.ban(0, 504, "raiding");
    share_job(&t.app, t.g(0), &t.share(504, None))
        .await
        .unwrap();
    assert_eq!(t.rows(504).await, [T::row(1, "skipped", Some("rate cap"))]);
    assert!(t.take("ban_sync_copy").await.is_empty());
    assert_eq!(t.calls("POST /users/@me/channels"), 1);
    assert_eq!(t.calls(&format!("bansync:rest:{}", t.g(0))), 1);
    // The button needs the live owner and a fresh 2FA.
    let click = |who| send_rest_click(&t.app, Id::new(who), t.g(0));
    assert!(click(o - 1).await.unwrap().starts_with("Only the owner"));
    assert!(click(o).await.unwrap().contains("needs 2FA"));
    sqlx::query("INSERT INTO dash_session (id, user_id, username, access_token, csrf, step_up_until, expires_at) VALUES ($1, $2, 'o', 'x', 'x', now() + interval '5 minutes', now() + interval '1 day')")
        .bind(format!("bansync-{o}"))
        .bind(o as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    assert!(click(o).await.unwrap().starts_with("Sending"));
    let (_, p, _) = t.take("ban_sync_send_rest").await.remove(0);
    assert_eq!(p, json!({"source": t.g(0).get()}));
    send_rest_job(&t.app, &json!({"source": t.g(0).to_string()}))
        .await
        .unwrap();
    assert_eq!(t.rows(504).await, [T::row(1, "pending", None)]);
    t.run_copies().await;
    assert_eq!(t.rows(504).await[0].1, "banned");
    t.cleanup().await;
}

#[tokio::test]
async fn two_offers_at_once_cannot_both_pass_the_cap() {
    let Some(t) = T::new(18).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    for u in 0..BAN_SYNC_HOURLY as u64 - 1 {
        t.link(1, 10_000 + u, 0, "banned").await;
    }
    let copy = Copy {
        origin: Origin::StaffCmd,
        case: None,
        reason: "raiding".into(),
        expires: None,
        actor: None,
        moderator: None,
        banned_at: None,
    };
    // One slot is left. Another offer holds the cap lock and has written its row, not committed
    // yet: this offer waits for it instead of counting without it, then finds the cap reached.
    let mut other = t.app.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('ban_sync_cap:' || $1, 0))")
        .bind(t.g(0).get() as i64)
        .execute(&mut *other)
        .await
        .unwrap();
    sqlx::query("INSERT INTO ban_sync_links (guild_id, user_id, source_guild, origin, reason, state) VALUES ($1, 531, $2, 'staff_cmd', 'x', 'pending')")
        .bind(t.g(1).get() as i64)
        .bind(t.g(0).get() as i64)
        .execute(&mut *other)
        .await
        .unwrap();
    let key = (t.g(1), Id::new(532), t.g(0));
    let wait = std::time::Duration::from_millis(300);
    let early = tokio::time::timeout(wait, offer(&t.app, key, &copy)).await;
    assert!(
        early.is_err(),
        "the offer counted without waiting for the lock"
    );
    other.commit().await.unwrap();
    assert_eq!(offer(&t.app, key, &copy).await.unwrap(), (false, true));
    t.cleanup().await;
}

#[tokio::test]
async fn another_owner_a_transfer_or_a_left_server_gets_no_copy() {
    let Some(t) = T::new(3).await else { return };
    let (o, other) = (t.owner(), t.base + 8);
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.server(2, o, "ban").await;
    t.server(3, o, "ban").await;
    t.server(4, o, "ban").await;
    // Server 1 changed hands but `guild_owners` is stale: Discord's live owner decides.
    t.set(|m| {
        m.owners.insert(t.g(1).get(), other);
    });
    t.link(2, 505, 0, "pending").await;
    t.link(2, 506, 0, "alerted").await;
    on_owner(&t.app, t.g(2), Id::new(other)).await.unwrap();
    let cancelled = T::row(2, "cancelled", Some("owner changed"));
    assert_eq!(t.rows(505).await, std::slice::from_ref(&cancelled));
    assert_eq!(t.rows(506).await, [cancelled]);
    assert!(
        t.app.guilds.lock().unwrap().get(&t.g(2)).is_none(),
        "cache dropped"
    );
    // The bot left server 3 (no row) and cannot see server 4 any more (403/404): both are skipped.
    on_removed(&t.app, t.g(3)).await.unwrap();
    t.set(|m| {
        m.owners.remove(&t.g(4).get());
    });
    t.server(5, o, "ban").await;
    t.ban(0, 507, "raiding");
    share_job(&t.app, t.g(0), &t.share(507, None))
        .await
        .unwrap();
    assert_eq!(t.rows(507).await, [T::row(5, "pending", None)]);
    assert_eq!(
        t.calls(&format!("GET /guilds/{} ", t.g(3))),
        0,
        "no call for a left server"
    );
    t.cleanup().await;
}

#[tokio::test]
async fn exempt_members_are_skipped_in_the_receiving_server() {
    let Some(t) = T::new(4).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.exec("INSERT INTO extra_owners (guild_id, user_id) VALUES ($1 + 1, 507)")
        .await;
    for u in [507, o, BOT_OWNER, ME] {
        t.ban(0, u, "raiding");
        share_job(&t.app, t.g(0), &t.share(u, None)).await.unwrap();
    }
    t.run_copies().await;
    for (u, why) in [
        (507, "extra owner"),
        (o, "server owner"),
        (BOT_OWNER, "the bot owner"),
        (ME, "the bot itself"),
    ] {
        assert_eq!(t.rows(u).await, [T::row(1, "skipped", Some(why))], "{why}");
    }
    assert_eq!(t.calls("PUT"), 0);
    t.cleanup().await;
}

#[tokio::test]
async fn a_failed_ban_retries_a_refused_one_skips_and_a_made_one_gets_its_case() {
    let Some(t) = T::new(10).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    let copy = |u: u64| json!({"user": u, "source": t.g(0).get()});
    for u in [530, 531, 532] {
        t.ban(0, u, "raiding");
        t.link(1, u, 0, "pending").await;
    }
    // Discord fails (500 on the member lookup): the row stays pending and the job runs again.
    t.set(|m| m.fail_member = true);
    assert!(copy_job(&t.app, t.g(1), &copy(530)).await.is_err());
    assert_eq!(t.rows(530).await, [T::row(1, "pending", None)]);
    t.set(|m| m.fail_member = false);
    // Discord refuses the ban: skipped with its reason.
    t.set(|m| m.fail_put = true);
    copy_job(&t.app, t.g(1), &copy(531)).await.unwrap();
    let r = t.rows(531).await;
    assert_eq!(
        (
            r[0].1.as_str(),
            r[0].2.as_deref().map(|w| w.starts_with("Failed to ban"))
        ),
        ("skipped", Some(true))
    );
    t.set(|m| m.fail_put = false);
    // An earlier try made the ban but stopped before its case: the retry writes only the case.
    t.ban(
        1,
        532,
        &format!("{PREFIX} Main ({}): raiding | Moderator: 1", t.g(0)),
    );
    copy_job(&t.app, t.g(1), &copy(532)).await.unwrap();
    copy_job(&t.app, t.g(1), &copy(532)).await.unwrap();
    assert_eq!(t.rows(532).await, [T::row(1, "banned", None)]);
    assert_eq!(t.calls("PUT"), 1, "only the refused try");
    let cases = t
        .count("SELECT count(*) FROM cases WHERE guild_id = $1 + 1 AND user_id = 532")
        .await;
    assert_eq!(cases, 1);
    // A banned row without a case (the case failed) gets it on the next run.
    t.ban(
        1,
        533,
        &format!("{PREFIX} Main ({}): raiding | Moderator: 1", t.g(0)),
    );
    t.link(1, 533, 0, "banned").await;
    copy_job(&t.app, t.g(1), &copy(533)).await.unwrap();
    let no: Option<i32> = sqlx::query_scalar(
        "SELECT case_no FROM ban_sync_links WHERE guild_id = $1 AND user_id = 533",
    )
    .bind(t.g(1).get() as i64)
    .fetch_one(&t.app.pool)
    .await
    .unwrap();
    assert!(no.is_some());
    t.cleanup().await;
}

#[tokio::test]
async fn bot_bans_are_classified_after_the_hold() {
    let Some(t) = T::new(5).await else { return };
    let (g, now) = (t.g(0), now_secs());
    t.exec("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id) VALUES ($1, 1, 'ban', 1, 9), ($1, 2, 'ban', 2, 1)")
        .await;
    t.exec("INSERT INTO joingate_actions (guild_id, user_id, reason, action, ok) VALUES ($1, 3, 'join raid', 'Ban', true), ($1, 4, 'join gate', 'Ban', true)")
        .await;
    let app = &t.app;
    let of = |u: u64, actor: Option<u64>, reason: &'static str| async move {
        let user = Id::new(u);
        let case = match actor {
            Some(a) => bot_case(app, g, user, Some(a), &["ban"], now)
                .await
                .unwrap(),
            None => last_case(app, g, user, &["ban"], None).await.unwrap(),
        };
        let since = actor.map(|_| now - CASE_WINDOW_S);
        origin(app, (g, user), actor, case, since, reason)
            .await
            .unwrap()
    };
    assert_eq!(of(1, Some(ME), "").await, Origin::StaffCmd);
    assert_eq!(of(2, Some(ME), "").await, Origin::Automod);
    assert_eq!(of(3, Some(ME), "").await, Origin::JoinRaid);
    assert_eq!(of(4, Some(ME), "").await, Origin::JoinGate);
    assert_eq!(of(5, Some(ME), "").await, Origin::Verify);
    assert_eq!(of(1, Some(9), "").await, Origin::Discord);
    // An import has no audit entry: records first, then the reason.
    assert_eq!(of(1, None, "").await, Origin::StaffCmd);
    assert_eq!(of(5, None, "spam").await, Origin::Discord);
    assert_eq!(of(5, None, verify::AUDIT_REASON).await, Origin::Verify);
    // A timeout carries its audit reason unless that is off: only then is an empty one Verify.
    assert_eq!(of(5, None, "").await, Origin::Discord);
    t.exec("INSERT INTO guild_config (guild_id, version, config) VALUES ($1 + 2, 1, '{\"verification\": {\"audit_reason\": false}}')")
        .await;
    let off = origin(app, (t.g(2), Id::new(5)), None, None, None, "").await;
    assert_eq!(off.unwrap(), Origin::Verify);
    // A user with both a case and a Join Gate record: the newer one made the ban.
    t.exec("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id, created_at) VALUES ($1, 3, 'ban', 6, 9, now() - interval '1 hour'), ($1, 4, 'ban', 7, 9, now())")
        .await;
    t.exec("INSERT INTO joingate_actions (guild_id, user_id, reason, action, ok, created_at) VALUES ($1, 6, 'join raid', 'Ban', true, now()), ($1, 7, 'join raid', 'Ban', true, now() - interval '1 hour')")
        .await;
    assert_eq!(of(6, None, "").await, Origin::JoinRaid);
    assert_eq!(of(7, None, "").await, Origin::StaffCmd);
    // A Join Gate ban stays home while its switch is off (the default).
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.ban(0, 4, "join gate");
    share_job(&t.app, g, &t.share(4, Some(ME))).await.unwrap();
    assert!(t.rows(4).await.is_empty());
    t.cleanup().await;
}

#[tokio::test]
async fn an_unban_lifts_only_copies_that_still_carry_the_prefix() {
    let Some(t) = T::new(6).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.server(2, o, "ban").await;
    t.server(3, o, "alert").await;
    t.link(1, 508, 0, "banned").await;
    t.link(2, 508, 0, "banned").await;
    t.link(3, 508, 0, "alerted").await;
    t.ban(
        1,
        508,
        &format!("{PREFIX} Main ({}): raiding | Moderator: 1", t.g(0)),
    );
    t.ban(2, 508, "banned again by staff");
    on_unban(&t.app, &t.entry(0, 23, 508, Some(77)))
        .await
        .unwrap();
    let (_, p, wait) = t.take("ban_sync_lift").await.remove(0);
    assert!(wait > 50);
    lift_job(&t.app, t.g(0), &p).await.unwrap();
    assert_eq!(t.banned(1, 508), None);
    assert!(t.banned(2, 508).is_some());
    assert_eq!(
        t.rows(508).await,
        [
            T::row(1, "lifted", None),
            T::row(2, "skipped", Some("banned again by hand")),
            T::row(3, "lifted", None),
        ]
    );
    let unbans = t
        .count("SELECT count(*) FROM cases WHERE guild_id = $1 + 1 AND kind = 'unban'")
        .await;
    assert_eq!(unbans, 1);
    // An unban in a receiving server (an appeal there) lifts only that copy.
    t.link(1, 509, 0, "banned").await;
    t.link(2, 509, 0, "banned").await;
    on_unban(&t.app, &t.entry(2, 23, 509, Some(ME)))
        .await
        .unwrap();
    assert_eq!(
        t.rows(509).await,
        [T::row(1, "banned", None), T::row(2, "lifted", None)]
    );
    t.cleanup().await;
}

#[tokio::test]
async fn a_reban_or_a_staff_ban_over_an_old_row_is_shared_again() {
    let Some(t) = T::new(7).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.link(1, 510, 0, "lifted").await;
    t.ban(0, 510, "raiding");
    share_job(&t.app, t.g(0), &t.share(510, None))
        .await
        .unwrap();
    assert_eq!(t.rows(510).await[0].1, "pending");
    // Server 1 once got 511 from server 0; a real staff ban there now is shared.
    for state in ["skipped", "lifted", "alerted"] {
        t.exec("DELETE FROM ban_sync_links WHERE guild_id = $1 + 1")
            .await;
        t.link(1, 511, 0, state).await;
        on_ban(&t.app, &t.entry(1, 22, 511, Some(77)))
            .await
            .unwrap();
        assert_eq!(t.take("ban_sync_share").await.len(), 1, "{state}");
    }
    t.cleanup().await;
}

#[tokio::test]
async fn a_temp_ban_keeps_its_end_and_an_ended_one_is_skipped() {
    let Some(t) = T::new(8).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    let end = now_secs() + 3600;
    let job = json!({"user": 512});
    jobs::enqueue(&t.app.pool, t.g(0).get() as i64, "unban", job, end)
        .await
        .unwrap();
    t.ban(0, 512, "raiding");
    share_job(&t.app, t.g(0), &t.share(512, None))
        .await
        .unwrap();
    t.run_copies().await;
    let unbans = t.take("unban").await;
    let here: Vec<_> = unbans.iter().filter(|j| j.0 == 1).collect();
    assert_eq!(here.len(), 1);
    assert!((here[0].2 - 3600).abs() <= 2, "same end: {}", here[0].2);
    t.link(1, 513, 0, "pending").await;
    t.exec("UPDATE ban_sync_links SET expires_at = now() - interval '1 minute' WHERE user_id = 513 AND guild_id = $1 + 1")
        .await;
    t.ban(0, 513, "raiding");
    copy_job(
        &t.app,
        t.g(1),
        &json!({"user": 513, "source": t.g(0).get()}),
    )
    .await
    .unwrap();
    let ended = T::row(1, "skipped", Some("temp ban already ended"));
    assert_eq!(t.rows(513).await, [ended]);
    t.cleanup().await;
}

#[tokio::test]
async fn ban_here_claims_the_row_once_and_gives_it_back_on_failure() {
    let Some(t) = T::new(11).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "alert").await;
    let click = |u: u64| ban_click(&t.app, t.g(1), Id::new(o), (t.g(0), Id::new(u)));
    for u in [540, 541, 542, 543] {
        t.ban(0, u, "raiding");
        t.link(1, u, 0, "alerted").await;
    }
    // Discord refuses: the alert stays for another try.
    t.set(|m| m.fail_put = true);
    assert!(click(540).await.unwrap().starts_with("Failed to ban"));
    assert_eq!(t.rows(540).await, [T::row(1, "alerted", None)]);
    t.set(|m| m.fail_put = false);
    // A lookup fails: an error, and the alert is still there.
    t.set(|m| m.fail_member = true);
    assert!(click(540).await.is_err());
    t.set(|m| m.fail_member = false);
    assert_eq!(t.rows(540).await, [T::row(1, "alerted", None)]);
    assert!(!click(540).await.unwrap().starts_with("Failed"));
    assert_eq!(t.rows(540).await, [T::row(1, "banned", None)]);
    assert!(t.banned(1, 540).unwrap().starts_with(PREFIX));
    assert_eq!(click(540).await.unwrap(), "This alert was already handled.");
    // Every claim queued a copy job long after, in case the bot stops mid-click.
    let jobs = t.take("ban_sync_copy").await;
    assert!(!jobs.is_empty() && jobs.iter().all(|j| (295..=300).contains(&j.2)));
    // The staff member behind the source ban was contained since.
    t.exec("UPDATE ban_sync_links SET mod_id = 9, actor_id = 1, banned_at = now() - interval '1 minute' WHERE user_id = 543")
        .await;
    t.exec("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 9, '{}')")
        .await;
    assert!(click(543).await.unwrap().starts_with("Anti-nuke stopped"));
    let stopped = T::row(1, "cancelled", Some("source moderator contained"));
    assert_eq!(t.rows(543).await, [stopped]);
    assert_eq!(t.banned(1, 543), None);
    // An ended temp ban, and a source ban lifted since the alert.
    t.exec(
        "UPDATE ban_sync_links SET expires_at = now() - interval '1 minute' WHERE user_id = 541",
    )
    .await;
    assert!(click(541).await.unwrap().contains("already ended"));
    t.set(|m| {
        m.bans.remove(&(t.g(0).get(), 542));
    });
    assert!(click(542).await.unwrap().contains("was lifted"));
    assert_eq!(t.rows(542).await, [T::row(1, "lifted", None)]);
    t.cleanup().await;
}

#[tokio::test]
async fn an_import_pages_obeys_the_switches_and_the_cap() {
    let Some(t) = T::new(12).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    for u in 0..(BAN_SYNC_HOURLY - 2) as u64 {
        t.link(1, 10_000 + u, 0, "banned").await;
    }
    // Seven bans over three pages; one is a verification timeout and stays home.
    for u in 550..557 {
        t.ban(0, u, "raiding");
    }
    t.ban(0, 557, verify::AUDIT_REASON);
    // A member quarantined on join holds no permit: no reason to refuse.
    t.exec("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 70, '{}')")
        .await;
    let job = json!({"source": t.g(0).to_string(), "target": t.g(1).to_string()});
    import_job(&t.app, &job).await.unwrap();
    assert_eq!(t.calls(&format!("GET /guilds/{}/bans?", t.g(0))), 3);
    let mut states = vec![];
    for u in 550..558 {
        states.push(t.rows(u).await.first().map(|r| (r.1.clone(), r.2.clone())));
    }
    let s = |st: &str, w: Option<&str>| Some((st.to_string(), w.map(String::from)));
    let cap = s("skipped", Some("rate cap"));
    assert_eq!(
        states,
        [
            s("pending", None),
            s("pending", None),
            cap.clone(),
            cap.clone(),
            cap.clone(),
            cap.clone(),
            cap,
            None
        ]
    );
    // Anti-nuke acted there today: no import.
    t.exec("DELETE FROM ban_sync_links WHERE guild_id = $1 + 1")
        .await;
    t.exec("INSERT INTO nuke_incidents (guild_id, case_id, data) VALUES ($1, 'n1', '{}')")
        .await;
    import_job(&t.app, &job).await.unwrap();
    assert!(t.rows(550).await.is_empty());
    t.cleanup().await;
}

#[tokio::test]
async fn the_panel_shows_only_the_live_group() {
    let Some(t) = T::new(13).await else { return };
    let o = t.owner();
    t.server(0, o, "ban").await;
    t.server(1, o, "ban").await;
    t.link(0, 560, 1, "banned").await;
    // A row with a server that is no longer in the group stays hidden.
    t.link(0, 561, 2, "banned").await;
    let text = panel_text(&t.app, t.g(0)).await.unwrap();
    assert!(text.contains(&format!("Server {}", t.g(1))), "{text}");
    assert!(
        text.contains("<@560>") && !text.contains("<@561>"),
        "{text}"
    );
    t.cleanup().await;
}

#[tokio::test]
async fn only_a_nuke_case_or_a_contained_permit_holder_holds_a_bot_ban() {
    let Some(t) = T::new(14).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    // Automod bans: the bot is moderator too.
    t.exec("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id) VALUES ($1, 1, 'ban', 600, 1), ($1, 2, 'ban', 601, 1), ($1, 3, 'ban', 602, 1)")
        .await;
    // A raider quarantined on join (or by `t!quarantine`) cannot stop them, nor can a member
    // whose permit only warns.
    t.exec("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 70, '{}'), ($1, 72, '{554}')")
        .await;
    t.exec(
        "INSERT INTO permits (id, guild_id, name, perms) VALUES ($1 + 1, $1, 'Helpers', '{warn}')",
    )
    .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1 + 1, $1, 554)")
        .await;
    t.ban(0, 600, "raiding");
    share_job(&t.app, t.g(0), &t.share(600, Some(ME)))
        .await
        .unwrap();
    assert_eq!(t.rows(600).await, [T::row(1, "pending", None)]);
    // A quarantined member whose saved roles carry a permit that can kick does.
    t.exec("INSERT INTO permits (id, guild_id, name, perms) VALUES ($1, $1, 'Mods', '{kick}')")
        .await;
    t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $1, 555)")
        .await;
    t.exec("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 71, '{555}')")
        .await;
    t.ban(0, 601, "raiding");
    share_job(&t.app, t.g(0), &t.share(601, Some(ME)))
        .await
        .unwrap();
    assert!(t.rows(601).await.is_empty());
    // So does any nuke case.
    t.exec("DELETE FROM quarantines WHERE guild_id = $1").await;
    t.exec("INSERT INTO nuke_incidents (guild_id, case_id, data) VALUES ($1, 'n1', '{}')")
        .await;
    t.ban(0, 602, "raiding");
    share_job(&t.app, t.g(0), &t.share(602, Some(ME)))
        .await
        .unwrap();
    assert!(t.rows(602).await.is_empty());
    t.cleanup().await;
}

#[tokio::test]
async fn a_ban_left_by_a_stopped_try_is_recorded_or_lifted() {
    let Some(t) = T::new(15).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "alert").await;
    let copy = |u: u64| json!({"user": u, "source": t.g(0).get()});
    let ours = format!("{PREFIX} Main ({}): raiding | Moderator: {o}", t.g(0));
    // Ban here banned (with its case by the clicker) and the bot stopped: the recovery job
    // records the ban and takes over that case.
    t.ban(0, 610, "raiding");
    t.link(1, 610, 0, "pending").await;
    t.ban(1, 610, &ours);
    t.exec("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id) VALUES ($1 + 1, 1, 'ban', 610, $1 + 9)")
        .await;
    copy_job(&t.app, t.g(1), &copy(610)).await.unwrap();
    assert_eq!(t.rows(610).await, [T::row(1, "banned", None)]);
    let cases = t
        .count("SELECT count(*) FROM cases WHERE guild_id = $1 + 1 AND user_id = 610")
        .await;
    assert_eq!(cases, 1, "no second case");
    // Stopped before the ban: the recovery job alerts again.
    t.ban(0, 611, "raiding");
    t.link(1, 611, 0, "pending").await;
    copy_job(&t.app, t.g(1), &copy(611)).await.unwrap();
    assert_eq!(t.rows(611).await, [T::row(1, "alerted", None)]);
    // The ban was made but the source ban is gone by the retry: the copy is lifted, not left.
    t.link(1, 612, 0, "pending").await;
    t.ban(1, 612, &ours);
    copy_job(&t.app, t.g(1), &copy(612)).await.unwrap();
    assert_eq!(t.banned(1, 612), None);
    assert_eq!(t.rows(612).await, [T::row(1, "lifted", None)]);
    let unbans = t
        .count("SELECT count(*) FROM cases WHERE guild_id = $1 + 1 AND user_id = 612 AND kind = 'unban'")
        .await;
    assert_eq!(unbans, 1);
    // An unban lifts only copies from its own server, not one from another server of the group.
    t.link(1, 613, 0, "banned").await;
    t.ban(1, 613, &format!("{PREFIX} Other ({}): raiding", t.g(2)));
    lift_job(&t.app, t.g(0), &t.share(613, Some(77)))
        .await
        .unwrap();
    assert!(t.banned(1, 613).is_some());
    let by_hand = T::row(1, "skipped", Some("banned again by hand"));
    assert_eq!(t.rows(613).await, [by_hand]);
    // Made, then the member turned out exempt: the ban stays (and the mod log says so).
    t.exec("INSERT INTO extra_owners (guild_id, user_id) VALUES ($1 + 1, 614)")
        .await;
    t.ban(0, 614, "raiding");
    t.link(1, 614, 0, "pending").await;
    t.ban(1, 614, &ours);
    copy_job(&t.app, t.g(1), &copy(614)).await.unwrap();
    assert!(t.banned(1, 614).is_some());
    assert_eq!(t.rows(614).await, [T::row(1, "banned", None)]);
    // Made, then the server left the group: the copy stays as an ordinary ban.
    t.ban(0, 615, "raiding");
    t.link(1, 615, 0, "pending").await;
    t.ban(1, 615, &ours);
    t.set(|m| {
        m.owners.insert(t.g(1).get(), t.base + 8);
    });
    copy_job(&t.app, t.g(1), &copy(615)).await.unwrap();
    assert!(t.banned(1, 615).is_some());
    assert_eq!(t.rows(615).await, [T::row(1, "banned", None)]);
    t.cleanup().await;
}

#[tokio::test]
async fn a_copy_dropped_after_its_last_try_is_skipped() {
    let Some(t) = T::new(17).await else { return };
    t.link(1, 630, 0, "pending").await;
    let job = jobs::Job {
        id: 0,
        guild_id: t.g(1).get() as i64,
        kind: "ban_sync_copy".into(),
        payload: json!({"user": 630, "source": t.g(0).get()}),
        attempts: jobs::MAX_ATTEMPTS,
    };
    copy_gave_up(&t.app, &job, &anyhow::anyhow!("Discord is down")).await;
    let gave_up = T::row(1, "skipped", Some("gave up: see logs"));
    assert_eq!(t.rows(630).await, [gave_up]);
    // No longer a copy: a new ban there is shared again.
    assert!(!is_copy(&t.app, t.g(1), Id::new(630)).await.unwrap());
    t.cleanup().await;
}

#[tokio::test]
async fn a_discord_outage_on_the_ban_retries_instead_of_skipping() {
    let Some(t) = T::new(16).await else { return };
    let o = t.owner();
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.ban(0, 620, "raiding");
    t.link(1, 620, 0, "pending").await;
    let copy = json!({"user": 620, "source": t.g(0).get()});
    t.set(|m| m.fail_put_5xx = true);
    assert!(copy_job(&t.app, t.g(1), &copy).await.is_err());
    assert_eq!(t.rows(620).await, [T::row(1, "pending", None)]);
    t.set(|m| m.fail_put_5xx = false);
    copy_job(&t.app, t.g(1), &copy).await.unwrap();
    assert_eq!(t.rows(620).await, [T::row(1, "banned", None)]);
    t.cleanup().await;
}

/// D59: the unauthorized-bot filter reads who added a bot once more after 1.5 s, and a lookup
/// that fails or finds nothing twice blocks the bot.
#[tokio::test]
async fn a_bot_adder_is_read_twice_then_the_bot_is_blocked() {
    let Some(t) = T::new(100).await else {
        return;
    };
    let (g, bot) = (t.g(0), Id::new(77));
    let entry =
        json!({"id": "5", "action_type": 28, "target_id": "77", "user_id": "9", "changes": []});
    let app = &t.app;
    let adder = |_: &T| crate::welcome::bot_adder_retry(app, g, bot);
    t.set(|m| m.audit = vec![entry.clone()]);
    assert_eq!(adder(&t).await.unwrap(), Id::new(9));
    t.set(|m| m.fail_audit = 1);
    assert_eq!(adder(&t).await.unwrap(), Id::new(9), "the retry reads it");
    t.set(|m| m.fail_audit = 2);
    assert!(adder(&t).await.is_err(), "failed twice");
    t.set(|m| m.audit.clear());
    assert!(adder(&t).await.is_err(), "no entry after the retry");
    t.cleanup().await;
}

/// D20: the last audit entry seen survives a restart, and the first `Ready` reads the entries
/// after it, oldest first.
#[tokio::test]
async fn a_restart_reads_the_audit_entries_it_missed() {
    let Some(t) = T::new(101).await else {
        return;
    };
    let g = t.g(0);
    crate::welcome::saw_audit(&t.app, g.get(), 5).await;
    crate::welcome::saw_audit(&t.app, g.get(), 4).await; // never goes back
                                                         // A second app over the same database stands for the restart.
    let fresh = T::new(102).await.unwrap();
    crate::welcome::load_audit_seen(&fresh.app).await.unwrap();
    let last = |t: &T| t.app.last_audit.lock().unwrap().get(&g.get()).copied();
    assert_eq!(last(&fresh), Some(5));
    let e = |id: &str| json!({"id": id, "action_type": 22, "target_id": "8", "user_id": "9", "changes": []});
    fresh.set(|m| m.audit = vec![e("9"), e("3"), e("7")]);
    crate::welcome::catch_up_audit(&fresh.app, g, 5)
        .await
        .unwrap();
    assert_eq!(last(&fresh), Some(9));
    assert_eq!(fresh.calls("after=5"), 1);
    sqlx::query("DELETE FROM audit_seen WHERE guild_id = $1")
        .bind(g.get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
}

/// D14: a managed role's overwrite that gives more access in a channel is denied to the really
/// quarantined members who hold that role, with their own overwrite saved for unquarantine.
#[tokio::test]
async fn a_managed_role_overwrite_does_not_reach_a_quarantined_member() {
    let Some(t) = T::new(103).await else {
        return;
    };
    let g = t.g(0);
    t.app.guilds.lock().unwrap().insert(
        g,
        Arc::new(crate::hierarchy::GuildInfo {
            owner: Id::new(2),
            roles: Default::default(),
            role_perms: Default::default(),
            managed: [Id::new(6)].into(),
            parents: Default::default(),
            channels: Default::default(),
            nsfw: Default::default(),
            bot_top: 0,
            fetched: std::time::Instant::now(),
        }),
    );
    for (u, on_join) in [(50, false), (51, true)] {
        sqlx::query("INSERT INTO quarantines (guild_id, user_id, saved_roles, on_join) VALUES ($1, $2, '{6, 7}', $3)")
            .bind(g.get() as i64)
            .bind(u)
            .bind(on_join)
            .execute(&t.app.pool)
            .await
            .unwrap();
    }
    let own = json!({"id": "50", "type": 1, "allow": "0", "deny": "2"});
    t.set(|m| {
        m.overwrites.insert(30, vec![own]);
    });
    let entry = |role: &str| -> AuditLogEntry {
        serde_json::from_value(json!({
            "action_type": 13, "id": "1", "user_id": "9", "target_id": "30", "guild_id": g.to_string(),
            "options": {"id": role, "type": "0"},
            "changes": [{"key": "allow", "new_value": "1024"}]
        }))
        .unwrap()
    };
    crate::antinuke::hold_overwrite(&t.app, &entry("7"), Id::new(9))
        .await
        .unwrap();
    assert_eq!(
        t.calls("PUT /channels/30/permissions"),
        0,
        "not a managed role"
    );
    crate::antinuke::hold_overwrite(&t.app, &entry("6"), Id::new(9))
        .await
        .unwrap();
    assert_eq!(t.calls("PUT /channels/30/permissions/50"), 1);
    assert_eq!(
        t.calls("PUT /channels/30/permissions/51"),
        0,
        "a join quarantine"
    );
    assert!(
        t.calls(r#""deny":"1026""#) == 1,
        "their deny and the new bits"
    );
    let saved: (bool, i64, i64) = sqlx::query_as(
        "SELECT had, allow, deny FROM quarantine_overwrites WHERE guild_id = $1 AND user_id = 50",
    )
    .bind(g.get() as i64)
    .fetch_one(&t.app.pool)
    .await
    .unwrap();
    assert_eq!(saved, (true, 0, 2));
    for q in [
        "DELETE FROM quarantines WHERE guild_id = $1",
        "DELETE FROM quarantine_overwrites WHERE guild_id = $1",
    ] {
        sqlx::query(q)
            .bind(g.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
    }
}

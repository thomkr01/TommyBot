//! Tests of `bansync` (own file to keep bansync.rs under 1000 lines). The DB tests run only when
//! DATABASE_URL points at a scratch Postgres; Discord is a small local mock.
use super::*;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const ME: u64 = 1;
const BOT_OWNER: u64 = 2;

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
    assert!(copy_reason("Main", 5, "").starts_with(PREFIX));
}

/// What the mock Discord holds: bans by (guild, user) with their reason, and every request.
#[derive(Default)]
struct Mock {
    bans: HashMap<(u64, u64), String>,
    fail_get: bool,
    log: Vec<String>,
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

fn route(m: &mut Mock, method: &str, path: &str, reason: String, body: &str) -> (u16, String) {
    m.log.push(format!("{method} {path} {body}"));
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let user = |id: &str| json!({"id": id, "username": "u", "discriminator": "0", "avatar": null});
    let missing = (
        404,
        json!({"code": 10026, "message": "Unknown Ban"}).to_string(),
    );
    match (method, parts.as_slice()) {
        ("GET", ["guilds", g, "bans", u]) if m.fail_get => {
            let _ = (g, u);
            (500, json!({"code": 0, "message": "boom"}).to_string())
        }
        ("GET", ["guilds", g, "bans", u]) => {
            match m.bans.get(&(g.parse().unwrap(), u.parse().unwrap())) {
                Some(r) => (200, json!({"reason": r, "user": user(u)}).to_string()),
                None => missing,
            }
        }
        ("PUT", ["guilds", g, "bans", u]) => {
            m.bans
                .insert((g.parse().unwrap(), u.parse().unwrap()), reason);
            (204, String::new())
        }
        ("DELETE", ["guilds", g, "bans", u]) => {
            match m.bans.remove(&(g.parse().unwrap(), u.parse().unwrap())) {
                Some(_) => (204, String::new()),
                None => missing,
            }
        }
        ("POST", ["users", "@me", "channels"]) => (200, json!({"id": "77", "type": 1}).to_string()),
        ("POST", ["channels", _, "messages"]) => (200, "{}".into()),
        _ => (
            404,
            json!({"code": 10004, "message": "Unknown"}).to_string(),
        ),
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
    let path = target.split('?').next().unwrap_or("");
    let path = path.split_once("/api/v10").map_or(path, |p| p.1);
    let reason = decode(&header("x-audit-log-reason").unwrap_or_default());
    let (status, out) = route(&mut mock.lock().unwrap(), method, path, reason, &body);
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
        out.len()
    );
    let _ = s.write_all(resp.as_bytes()).await;
    let _ = s.shutdown().await;
}

struct T {
    app: App,
    mock: Arc<StdMutex<Mock>>,
    base: u64,
}

impl T {
    async fn new(n: u64) -> Option<T> {
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
            base: 3_300_000_000_000 + std::process::id() as u64 * 1000 + n * 10,
        };
        t.cleanup().await;
        Some(t)
    }

    fn g(&self, i: u64) -> Id<GuildMarker> {
        Id::new(self.base + i)
    }

    /// Server `i` with Ban Sync on (receiving as `receive`), owned by `owner` in the DB and in the cache.
    async fn server(&self, i: u64, owner: u64, receive: &str) {
        let cfg = json!({"ban_sync": {"enabled": true, "receive": receive}});
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
    }

    fn ban(&self, i: u64, user: u64, reason: &str) {
        let mut m = self.mock.lock().unwrap();
        m.bans.insert((self.g(i).get(), user), reason.into());
    }

    fn banned(&self, i: u64, user: u64) -> Option<String> {
        self.mock
            .lock()
            .unwrap()
            .bans
            .get(&(self.g(i).get(), user))
            .cloned()
    }

    fn calls(&self, what: &str) -> usize {
        self.mock
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.contains(what))
            .count()
    }

    /// (receiving server index, state, skip reason) of the rows for `user`.
    async fn rows(&self, user: u64) -> Vec<(u64, String, Option<String>)> {
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

    async fn link(&self, to: u64, user: u64, src: u64, state: &str) {
        sqlx::query(
            "INSERT INTO ban_sync_links (guild_id, user_id, source_guild, origin, state) VALUES ($1, $2, $3, 'discord', $4)",
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
    async fn take(&self, kind: &str) -> Vec<(u64, Value, i64)> {
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

    async fn run_copies(&self) {
        for (to, p, _) in self.take("ban_sync_copy").await {
            copy_job(&self.app, self.g(to), &p).await.unwrap();
        }
    }

    fn entry(&self, i: u64, kind: u16, user: u64, actor: Option<u64>) -> AuditLogEntry {
        serde_json::from_value(json!({
            "id": "1", "action_type": kind, "changes": [], "guild_id": self.g(i).to_string(),
            "target_id": user.to_string(), "user_id": actor.map(|a| a.to_string()), "reason": "raiding",
        }))
        .unwrap()
    }

    fn share(&self, user: u64, actor: Option<u64>) -> Value {
        json!({"user": user, "actor": actor, "reason": "raiding", "at": now_secs() - 61})
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.base as i64)
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }

    async fn cleanup(&self) {
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
        ] {
            sqlx::query(sql)
                .bind(self.base as i64)
                .execute(&self.app.pool)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn a_ban_is_held_then_copied_once_and_never_shared_on() {
    let Some(t) = T::new(0).await else { return };
    let o = t.base + 9;
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
        "runs after the hold: {}",
        held[0].2
    );
    share_job(&t.app, t.g(0), &held[0].1).await.unwrap();
    assert_eq!(
        t.rows(500)
            .await
            .iter()
            .map(|r| r.1.as_str())
            .collect::<Vec<_>>(),
        ["pending", "pending"]
    );
    let copies = t.take("ban_sync_copy").await;
    for (to, p, _) in copies.iter().chain(copies.iter()) {
        copy_job(&t.app, t.g(*to), p).await.unwrap();
    }
    let rows = t.rows(500).await;
    assert_eq!(
        (rows[0].1.as_str(), rows[1].1.as_str()),
        ("banned", "alerted")
    );
    assert!(t.banned(1, 500).unwrap().starts_with(PREFIX));
    assert_eq!(t.banned(2, 500), None, "alert only");
    assert_eq!(t.calls(&format!("PUT /guilds/{}/bans/500", t.g(1))), 1);
    let cases = t
        .count("SELECT count(*) FROM cases WHERE guild_id = $1 + 1 AND kind = 'ban'")
        .await;
    assert_eq!(cases, 1);
    let no: Option<i32> =
        sqlx::query_scalar("SELECT case_no FROM ban_sync_links WHERE guild_id = $1")
            .bind(t.g(1).get() as i64)
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert!(no.is_some());
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
    let o = t.base + 9;
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.ban(0, 501, "raiding");
    sqlx::query("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 77, '{}')")
        .bind(t.g(0).get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
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
    t.mock.lock().unwrap().fail_get = true;
    t.ban(0, 503, "raiding");
    assert!(share_job(&t.app, t.g(0), &t.share(503, Some(78)))
        .await
        .is_err());
    t.mock.lock().unwrap().fail_get = false;
    share_job(&t.app, t.g(0), &t.share(503, Some(78)))
        .await
        .unwrap();
    // Lifted between share and copy: the copy is cancelled.
    t.mock.lock().unwrap().bans.clear();
    t.run_copies().await;
    assert_eq!(
        t.rows(503).await,
        [(1, "cancelled".into(), Some("source ban lifted".into()))]
    );
    t.cleanup().await;
}

#[tokio::test]
async fn over_the_hourly_cap_rows_wait_for_the_owner() {
    let Some(t) = T::new(2).await else { return };
    let o = t.base + 9;
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    for u in 0..BAN_SYNC_HOURLY as u64 {
        t.link(1, 10_000 + u, 0, "banned").await;
    }
    t.ban(0, 504, "raiding");
    share_job(&t.app, t.g(0), &t.share(504, None))
        .await
        .unwrap();
    assert_eq!(
        t.rows(504).await,
        [(1, "skipped".into(), Some("rate cap".into()))]
    );
    assert!(t.take("ban_sync_copy").await.is_empty());
    assert_eq!(t.calls("POST /users/@me/channels"), 1);
    assert_eq!(
        t.calls(&format!("bansync:rest:{}", t.g(0))),
        1,
        "DM has Send the rest"
    );
    send_rest_job(&t.app, &json!({"source": t.g(0).to_string()}))
        .await
        .unwrap();
    assert_eq!(t.rows(504).await, [(1, "pending".into(), None)]);
    t.run_copies().await;
    assert_eq!(t.rows(504).await[0].1, "banned");
    t.cleanup().await;
}

#[tokio::test]
async fn another_owner_or_a_transfer_gets_no_copy() {
    let Some(t) = T::new(3).await else { return };
    let (o, other) = (t.base + 9, t.base + 8);
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.server(2, o, "ban").await;
    // Server 1 changed hands but `guild_owners` is stale: the live owner decides.
    t.app.seed_guild(t.g(1), Id::new(other));
    t.link(2, 505, 0, "pending").await;
    on_owner(&t.app, t.g(2), Id::new(other)).await.unwrap();
    assert_eq!(
        t.rows(505).await,
        [(2, "cancelled".into(), Some("owner changed".into()))]
    );
    assert!(
        t.app.guilds.lock().unwrap().get(&t.g(2)).is_none(),
        "cache dropped"
    );
    t.app.seed_guild(t.g(2), Id::new(other));
    t.ban(0, 506, "raiding");
    share_job(&t.app, t.g(0), &t.share(506, None))
        .await
        .unwrap();
    assert!(t.rows(506).await.is_empty());
    t.cleanup().await;
}

#[tokio::test]
async fn exempt_members_are_skipped_in_the_receiving_server() {
    let Some(t) = T::new(4).await else { return };
    let o = t.base + 9;
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    sqlx::query("INSERT INTO extra_owners (guild_id, user_id) VALUES ($1, 507)")
        .bind(t.g(1).get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
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
        assert_eq!(
            t.rows(u).await,
            [(1, "skipped".into(), Some(why.into()))],
            "{why}"
        );
    }
    assert_eq!(t.calls("PUT"), 0);
    t.cleanup().await;
}

#[tokio::test]
async fn bot_bans_are_classified_after_the_hold() {
    let Some(t) = T::new(5).await else { return };
    let (g, now) = (t.g(0), now_secs());
    let case = |user: i64, by: i64| {
        sqlx::query("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id) VALUES ($1, $2, 'ban', $2, $3)")
            .bind(g.get() as i64)
            .bind(user)
            .bind(by)
    };
    case(1, 9).execute(&t.app.pool).await.unwrap();
    case(2, ME as i64).execute(&t.app.pool).await.unwrap();
    for (u, why) in [(3, "join raid"), (4, "join gate")] {
        sqlx::query("INSERT INTO joingate_actions (guild_id, user_id, reason, action, ok) VALUES ($1, $2, $3, 'Ban', true)")
            .bind(g.get() as i64)
            .bind(u)
            .bind(why)
            .execute(&t.app.pool)
            .await
            .unwrap();
    }
    let of = |u: u64, actor: u64| origin(&t.app, g, Id::new(u), Some(actor), now);
    assert_eq!(of(1, ME).await.unwrap(), (Origin::StaffCmd, Some(1)));
    assert_eq!(of(2, ME).await.unwrap(), (Origin::Automod, Some(2)));
    assert_eq!(of(3, ME).await.unwrap().0, Origin::JoinRaid);
    assert_eq!(of(4, ME).await.unwrap().0, Origin::JoinGate);
    assert_eq!(of(5, ME).await.unwrap().0, Origin::Verify);
    assert_eq!(of(1, 9).await.unwrap(), (Origin::Discord, None));
    // A Join Gate ban stays home while its switch is off (the default).
    let o = t.base + 9;
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
    let o = t.base + 9;
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    t.server(2, o, "ban").await;
    t.link(1, 508, 0, "banned").await;
    t.link(2, 508, 0, "banned").await;
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
            (1, "lifted".into(), None),
            (2, "skipped".into(), Some("banned again by hand".into()))
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
        t.rows(509)
            .await
            .iter()
            .map(|r| r.1.as_str())
            .collect::<Vec<_>>(),
        ["banned", "lifted"]
    );
    t.cleanup().await;
}

#[tokio::test]
async fn a_reban_or_a_staff_ban_over_an_old_row_is_shared_again() {
    let Some(t) = T::new(7).await else { return };
    let o = t.base + 9;
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
        sqlx::query("DELETE FROM ban_sync_links WHERE guild_id = $1")
            .bind(t.g(1).get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
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
    let o = t.base + 9;
    t.server(0, o, "off").await;
    t.server(1, o, "ban").await;
    let end = now_secs() + 3600;
    jobs::enqueue(
        &t.app.pool,
        t.g(0).get() as i64,
        "unban",
        json!({"user": 512}),
        end,
    )
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
    sqlx::query("UPDATE ban_sync_links SET expires_at = now() - interval '1 minute' WHERE user_id = 513 AND guild_id = $1")
        .bind(t.g(1).get() as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.ban(0, 513, "raiding");
    copy_job(
        &t.app,
        t.g(1),
        &json!({"user": 513, "source": t.g(0).get()}),
    )
    .await
    .unwrap();
    assert_eq!(
        t.rows(513).await,
        [(1, "skipped".into(), Some("temp ban already ended".into()))]
    );
    t.cleanup().await;
}

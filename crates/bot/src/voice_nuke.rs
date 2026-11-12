//! Mass voice disconnect and move (audit types 27 and 26). Discord merges these entries by one
//! actor into one entry with a growing `options.count`, and the growth fires no gateway event
//! (PITFALLS §1). So the first gateway entry watches the guild, and while it is watched every
//! voice update asks for a debounced audit-log fetch that counts how much each entry grew.
use anyhow::Result;
use std::{collections::HashMap, future::Future, sync::Mutex, time::Duration};
use tokio::time::Instant;
use tommybot_antinuke::Action;
use twilight_model::{
    guild::audit_log::{AuditLogEntry, AuditLogEventType as A},
    id::{
        marker::{ChannelMarker, GuildMarker},
        Id,
    },
    voice::VoiceState,
};

use crate::app::{App, GuildCfg};

const WATCH: Duration = Duration::from_secs(600);
/// At most one fetch per this gap per guild.
const GAP: Duration = Duration::from_secs(3);
/// Older entries are not counted and their state is dropped.
const MAX_AGE_MS: i64 = 15 * 60 * 1000;
const MAX_COUNT: u32 = 500;
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

struct Watch {
    until: Instant,
    running: bool,
    /// A voice update came in during the running fetch or its gap: fetch once more.
    pending: bool,
    last: Option<Instant>,
}

pub struct State {
    started_ms: i64,
    guilds: HashMap<u64, Watch>,
    /// Audit entry id -> the count already counted.
    seen: HashMap<u64, u32>,
}

impl State {
    pub fn new(started_ms: i64) -> Self {
        Self {
            started_ms,
            guilds: HashMap::new(),
            seen: HashMap::new(),
        }
    }

    fn watch(&mut self, gid: u64, now: Instant) {
        self.guilds
            .entry(gid)
            .or_insert(Watch {
                until: now,
                running: false,
                pending: false,
                last: None,
            })
            .until = now + WATCH;
    }

    /// True when the caller holds the claim and must run the fetch loop.
    fn request(&mut self, gid: u64, now: Instant) -> bool {
        let Some(w) = self.guilds.get_mut(&gid).filter(|w| w.until > now) else {
            return false;
        };
        if w.running {
            w.pending = true;
            return false;
        }
        w.running = true;
        true
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Delta {
    pub actor: u64,
    pub action: Action,
    pub n: u32,
    /// Where members were moved to (moves only).
    pub channel: Option<u64>,
}

fn created_ms(id: u64) -> i64 {
    (id >> 22) as i64 + DISCORD_EPOCH_MS
}

/// How much each voice entry grew since it was last seen. Entries older than the bot are seeded
/// (counted from now on), so a restart does not count an old merged entry again.
pub fn deltas(
    seen: &mut HashMap<u64, u32>,
    entries: &[AuditLogEntry],
    me: u64,
    started_ms: i64,
    now_ms: i64,
) -> Vec<Delta> {
    seen.retain(|id, _| now_ms - created_ms(*id) < MAX_AGE_MS);
    let mut out = vec![];
    for e in entries {
        let action = match e.action_type {
            A::MemberDisconnect => Action::VoiceDisconnect,
            A::MemberMove => Action::VoiceMove,
            _ => continue,
        };
        let o = e.options.as_ref();
        // `count` is a string from Discord: anything unparsable is ignored.
        let count = o.and_then(|o| o.count.as_deref()?.parse::<u32>().ok());
        let (Some(actor), Some(count)) = (e.user_id.filter(|u| u.get() != me), count) else {
            continue;
        };
        let (id, count) = (e.id.get(), count.min(MAX_COUNT));
        let at = created_ms(id);
        if now_ms - at >= MAX_AGE_MS {
            continue;
        }
        let base = seen
            .get(&id)
            .copied()
            .unwrap_or(if at < started_ms { count } else { 0 });
        seen.insert(id, count.max(base));
        if count > base {
            out.push(Delta {
                actor: actor.get(),
                action,
                n: count - base,
                channel: o.and_then(|o| o.channel_id).map(|c| c.get()),
            });
        }
    }
    out
}

/// The fetch loop of the claim holder: one fetch at a time, at most one per `GAP`, and once more
/// after a voice update that came in meanwhile, so the end of a burst is always counted.
async fn run<F, Fut>(state: &Mutex<State>, gid: u64, mut fetch: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        let last = state.lock().unwrap().guilds.get(&gid).and_then(|w| w.last);
        if let Some(l) = last {
            tokio::time::sleep_until(l + GAP).await;
        }
        if let Some(w) = state.lock().unwrap().guilds.get_mut(&gid) {
            w.pending = false;
            w.last = Some(Instant::now());
        }
        fetch().await;
        let mut s = state.lock().unwrap();
        let now = Instant::now();
        let more = s.guilds.get_mut(&gid).is_some_and(|w| {
            w.running = w.pending;
            w.pending
        });
        if !more {
            s.guilds.retain(|_, w| w.running || w.until > now);
            return;
        }
    }
}

/// A gateway entry of type 26 or 27 (not by the bot; anti-nuke is on): counts it, watches the
/// guild and fetches once, since voice updates of the same burst may have come before it.
pub async fn note(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    e: &AuditLogEntry,
) -> Result<()> {
    let action = match e.action_type {
        A::MemberDisconnect => Action::VoiceDisconnect,
        _ => Action::VoiceMove,
    };
    if cfg.antinuke.limit(action).is_none() {
        return Ok(());
    }
    let found = {
        let mut s = app.voice_audit.lock().unwrap();
        s.watch(gid.get(), Instant::now());
        let started = s.started_ms;
        deltas(
            &mut s.seen,
            std::slice::from_ref(e),
            app.me.get(),
            started,
            crate::automod::now_ms(),
        )
    };
    apply(app, cfg, gid, found).await?;
    recheck(app, gid).await;
    Ok(())
}

/// A voice leave or move (`old`: the channel before): rechecks the audit log while the guild is
/// watched. Costs one lookup otherwise. Joins and mute changes cannot be a disconnect or a move.
pub async fn on_voice(app: &App, v: &VoiceState, old: Option<Id<ChannelMarker>>) {
    let moved = v.channel_id.is_none() || old.is_some_and(|o| v.channel_id != Some(o));
    if let Some(gid) = v.guild_id.filter(|_| moved) {
        recheck(app, gid).await;
    }
}

async fn recheck(app: &App, gid: Id<GuildMarker>) {
    if !app
        .voice_audit
        .lock()
        .unwrap()
        .request(gid.get(), Instant::now())
    {
        return;
    }
    run(&app.voice_audit, gid.get(), || async move {
        if let Err(e) = fetch(app, gid).await {
            tracing::warn!(?e, guild = gid.get(), "voice audit-log recheck failed");
        }
    })
    .await;
}

async fn fetch(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    if !cfg.antinuke.enabled {
        return Ok(());
    }
    let mut entries = vec![];
    for t in [A::MemberDisconnect, A::MemberMove] {
        let log = app.http.audit_log(gid).action_type(t).limit(25).await?;
        entries.extend(log.model().await?.entries);
    }
    let found = {
        let mut s = app.voice_audit.lock().unwrap();
        let started = s.started_ms;
        deltas(
            &mut s.seen,
            &entries,
            app.me.get(),
            started,
            crate::automod::now_ms(),
        )
    };
    apply(app, &cfg, gid, found).await
}

async fn apply(app: &App, cfg: &GuildCfg, gid: Id<GuildMarker>, found: Vec<Delta>) -> Result<()> {
    for d in found {
        let actor = Id::new(d.actor);
        let exempt = app.whitelisted(gid, actor, "antinuke").await?;
        crate::antinuke::count(app, cfg, gid, actor, d.action, d.channel, &[], exempt, d.n).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc,
    };

    const NOW: i64 = 1_780_000_000_000;

    fn entry(id_ms: i64, seq: u64, kind: u16, actor: u64, count: &str) -> AuditLogEntry {
        let id = (((id_ms - DISCORD_EPOCH_MS) as u64) << 22) | seq;
        serde_json::from_value(serde_json::json!({
            "action_type": kind,
            "changes": [],
            "id": id.to_string(),
            "options": {"count": count, "channel_id": "55"},
            "target_id": null,
            "user_id": actor.to_string(),
        }))
        .unwrap()
    }

    fn sum(d: &[Delta]) -> u32 {
        d.iter().map(|d| d.n).sum()
    }

    #[test]
    fn deltas_count_growth_once() {
        let mut seen = HashMap::new();
        let started = NOW - 60_000;
        let e1 = entry(NOW - 1000, 1, 27, 7, "1");
        // First sight counts the whole count.
        let d = deltas(&mut seen, &[e1], 1, started, NOW);
        assert_eq!(
            d,
            vec![Delta {
                actor: 7,
                action: Action::VoiceDisconnect,
                n: 1,
                channel: Some(55)
            }]
        );
        // Growth counts only the difference; the same data twice counts once.
        let grown = [
            entry(NOW - 1000, 1, 27, 7, "6"),
            entry(NOW - 500, 2, 26, 7, "3"),
        ];
        let d = deltas(&mut seen, &grown, 1, started, NOW);
        assert_eq!(
            d.iter().map(|d| (d.action, d.n)).collect::<Vec<_>>(),
            [(Action::VoiceDisconnect, 5), (Action::VoiceMove, 3)]
        );
        assert!(deltas(&mut seen, &grown, 1, started, NOW).is_empty());
    }

    #[test]
    fn deltas_seed_old_entries_skip_the_bot_and_bad_counts() {
        let mut seen = HashMap::new();
        let started = NOW - 60_000;
        // Made before a restart: seeded, only later growth counts.
        let old = entry(NOW - 120_000, 1, 27, 7, "12");
        assert!(deltas(&mut seen, &[old], 1, started, NOW).is_empty());
        let old = entry(NOW - 120_000, 1, 27, 7, "14");
        assert_eq!(sum(&deltas(&mut seen, &[old], 1, started, NOW)), 2);
        // The bot's own disconnects, unparsable counts and expired entries never count.
        let others = [
            entry(NOW - 1000, 2, 27, 1, "9"),
            entry(NOW - 1000, 3, 27, 7, "lots"),
            entry(NOW - 1000, 4, 27, 7, "-3"),
            entry(NOW - MAX_AGE_MS, 5, 27, 7, "9"),
        ];
        assert!(deltas(&mut seen, &others, 1, NOW - MAX_AGE_MS - 1, NOW).is_empty());
        // A huge count is capped.
        let big = entry(NOW - 1000, 6, 26, 7, "4000000000");
        assert_eq!(sum(&deltas(&mut seen, &[big], 1, started, NOW)), MAX_COUNT);
        // State older than 15 minutes is dropped.
        deltas(&mut seen, &[], 1, started, NOW + MAX_AGE_MS);
        assert!(seen.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_ends_with_a_fetch_that_counts_all_of_it() {
        let state = Arc::new(Mutex::new(State::new(NOW - 60_000)));
        let discord = Arc::new(AtomicU32::new(0)); // the merged entry's count in Discord
        let (counted, fetches) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        state.lock().unwrap().watch(9, Instant::now());
        let mut tasks = vec![];
        // 15 disconnects in 2 s, each with its voice update, and nothing after them.
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(133)).await;
            discord.fetch_add(1, SeqCst);
            if state.lock().unwrap().request(9, Instant::now()) {
                let (state, discord) = (state.clone(), discord.clone());
                let (counted, fetches) = (counted.clone(), fetches.clone());
                tasks.push(tokio::spawn(async move {
                    let st = state.clone();
                    run(&state, 9, move || {
                        let (state, discord) = (st.clone(), discord.clone());
                        let (counted, fetches) = (counted.clone(), fetches.clone());
                        async move {
                            // Discord answers after 200 ms.
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            let c = discord.load(SeqCst).to_string();
                            let e = entry(NOW - 1000, 1, 27, 7, &c);
                            let mut s = state.lock().unwrap();
                            let started = s.started_ms;
                            let d = deltas(&mut s.seen, &[e], 1, started, NOW);
                            counted.fetch_add(sum(&d), SeqCst);
                            fetches.fetch_add(1, SeqCst);
                        }
                    })
                    .await
                }));
            }
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(counted.load(SeqCst), 15);
        // One claim, fetches 3 s apart: at 0.13 s, 3.13 s (trailing) and no more.
        assert_eq!(fetches.load(SeqCst), 2);
        let s = state.lock().unwrap();
        assert!(!s.guilds[&9].running && !s.guilds[&9].pending);
    }

    #[test]
    fn only_watched_guilds_fetch_and_one_claim_at_a_time() {
        let mut s = State::new(0);
        let now = Instant::now();
        assert!(!s.request(9, now));
        s.watch(9, now);
        assert!(s.request(9, now));
        assert!(!s.request(9, now));
        assert!(s.guilds[&9].pending);
        assert!(!s.request(9, now + WATCH + Duration::from_secs(1)));
    }
}

//! Reply-shaping helpers driven by `MiscCfg`: compact text, DMs, delayed deletes, message purges.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;
use tommybot_core::jobs;
use tommybot_logging::embed::{duration, Log, ARROW, BLUE, GREEN, NO, YES};
use tommybot_logging::emoji::{DETAILS, MODERATOR, PANIC, REASON, UNSET};
use tommybot_logging::LogKind;
use twilight_model::channel::message::{Component, Embed};
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
    Id,
};

use crate::app::{now_secs, App};
use crate::http_err::{best_effort, gone};
use crate::prefix::Flags;

/// Compact replies are one line; the full form adds moderator and reason.
pub fn case_text(
    compact: bool,
    (case_no, kind, target): (i64, &str, u64),
    reason: Option<&str>,
    actor: u64,
) -> String {
    let mut s = format!("Case #{case_no}: {kind} <@{target}>");
    if !compact {
        s += format!("\nModerator: <@{actor}>").as_str();
        if let Some(r) = reason {
            s += format!("\nReason: {r}").as_str();
        }
    }
    if reason.is_none() {
        s += add_reason_hint(case_no).as_str();
    }
    s
}

/// Whether the member was told about the action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DmOutcome {
    /// No DM was asked for (server toggle off and no `?dm`).
    #[default]
    Off,
    Sent,
    /// Closed DMs, an error, or no answer within [`DM_WAIT`].
    Failed,
}

/// One target's outcome. `case` is set when the action happened; `text` is the plain reply
/// (`Case #N: ...` on success, the refusal or error otherwise).
#[derive(Debug, Default)]
pub struct Applied {
    pub target: Option<u64>,
    pub case: Option<i32>,
    pub text: String,
    pub dm: DmOutcome,
}

impl From<String> for Applied {
    fn from(text: String) -> Self {
        Self {
            text,
            ..Default::default()
        }
    }
}

/// One reply for all targets. Compact mode turns several targets into count lines; failures stay listed.
pub fn summary(
    kind: &str,
    compact: bool,
    done: &[Applied],
    reason: Option<&str>,
    actor: u64,
) -> String {
    let texts = || done.iter().map(|a| a.text.as_str()).collect::<Vec<_>>();
    let mut s = if compact && done.len() > 1 {
        let (ok, failed): (Vec<_>, Vec<_>) = done.iter().partition(|a| a.case.is_some());
        let mut s = format!(
            "{YES} Successful {kind}s: `{}`\n{NO} Unsuccessful {kind}s: `{}`",
            ok.len(),
            failed.len()
        );
        if let Some(r) = reason {
            s += format!("\n{REASON} Reason: {r}").as_str();
        }
        s += format!("\n{MODERATOR} Moderator: <@{actor}>").as_str();
        failed.iter().fold(s, |s, f| s + "\n" + f.text.as_str())
    } else {
        texts().join("\n")
    };
    s.truncate(s.floor_char_boundary(2000));
    s
}

/// "Add Reason" pointer for actions run without a reason.
pub fn add_reason_hint(case_no: i64) -> String {
    format!("\nAdd Reason: `/cases modify case:{case_no} reason:`")
}

/// Title word, plural for the Successful/Unsuccessful headings, and the line shown when nothing failed.
fn labels(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "ban" => ("Ban", "bans", "All members were banned!"),
        "softban" => ("Softban", "softbans", "All members were softbanned!"),
        "kick" => ("Kick", "kicks", "All members were kicked!"),
        "warn" => ("Warn", "warns", "All members were warned!"),
        "unban" => ("Unban", "unbans", "All members were unbanned!"),
        "untimeout" => (
            "Untimeout",
            "timeout removals",
            "All timeouts were removed!",
        ),
        "quarantine" => ("Quarantine", "quarantines", "All members were quarantined!"),
        "unquarantine" => (
            "Unquarantine",
            "quarantine removals",
            "All members were released from quarantine!",
        ),
        "verify" => ("Verify", "verifications", "All members were verified!"),
        "sanitize" => ("Sanitize", "renames", "All members were sanitized!"),
        "purge" => ("Purge", "purges", "Nothing failed!"),
        "slowmode" => ("Slowmode", "changes", "Nothing failed!"),
        _ => ("Timeout", "timeouts", "All members were timed out!"),
    }
}

/// `✅` when every DM that was tried arrived, `❎` when none did (or none was tried), counts when mixed.
fn dm_flag(done: &[Applied]) -> String {
    let sent = done.iter().filter(|a| a.dm == DmOutcome::Sent).count();
    let failed = done.iter().filter(|a| a.dm == DmOutcome::Failed).count();
    match (sent, failed) {
        (0, _) => UNSET.to_string(),
        (_, 0) => YES.to_string(),
        (s, f) => format!("{YES} {s} | {UNSET} {f}"),
    }
}

/// The Details lines of a result: the options that apply to `kind`, each ✅ when used.
pub fn details(
    kind: &str,
    flags: Flags,
    duration_s: Option<i64>,
    done: &[Applied],
) -> Vec<(&'static str, String)> {
    let on = |b: bool| if b { YES } else { UNSET }.to_string();
    let dur = || match duration_s {
        Some(d) => format!("`{}` {YES}", duration(d)),
        None => UNSET.to_string(),
    };
    let dm = ("DM-Members", dm_flag(done));
    match kind {
        "ban" | "softban" => vec![
            // A softban unbans at once, so a typed duration does nothing.
            (
                "Duration",
                if kind == "softban" { on(false) } else { dur() },
            ),
            ("Soft Ban", on(kind == "softban")),
            ("Hack Ban", on(flags.hack)),
            dm,
        ],
        "kick" => vec![
            ("No-Avatar", on(flags.no_avatar)),
            ("Voice-Channels", on(flags.voice)),
            dm,
        ],
        "timeout" => vec![("Duration", dur()), dm],
        "warn" | "quarantine" | "unquarantine" | "untimeout" | "unban" => vec![dm],
        _ => vec![],
    }
}

/// "Timeout result:" and its kin (Reason, Moderator, Details, Successful and Unsuccessful
/// blocks), plus a pointer to add a reason when none was given.
pub fn result_embeds(
    kind: &str,
    done: &[Applied],
    (flags, duration_s): (Flags, Option<i64>),
    reason: Option<&str>,
    actor: u64,
) -> Vec<Embed> {
    let (title, plural, all) = labels(kind);
    let mut result = Log::new(&format!("{title} result:"), BLUE)
        .text(&format!(
            "{REASON} **Reason:** {}",
            reason.unwrap_or("No reason.")
        ))
        .text(&format!("{MODERATOR} **Moderator:** <@{actor}>"));
    let lines = details(kind, flags, duration_s, done);
    if !lines.is_empty() {
        result = result.text(&format!("{DETAILS} **Details:**"));
        for (k, v) in lines {
            result = result.text(&format!("{ARROW} {k}: {v}"));
        }
    }
    result = result.text(&format!("{YES} **Successful {plural}**"));
    let (ok, failed): (Vec<_>, Vec<_>) = done.iter().partition(|a| a.case.is_some());
    for a in &ok {
        let first = a.text.lines().next().unwrap_or_default();
        result = match a.target {
            Some(id) => result.text(&format!("{ARROW} <@{id}> [`{id}`]")),
            // Purge and slowmode have no member: show what happened.
            None => result.text(&format!(
                "{ARROW} {}",
                first.split_once(": ").map_or(first, |c| c.1)
            )),
        };
        // Warn threshold notes; the embed already shows moderator and reason.
        for extra in a.text.lines().skip(1).filter(|l| {
            !["Moderator:", "Reason:", "Add Reason"]
                .iter()
                .any(|p| l.starts_with(p))
        }) {
            result = result.text(extra);
        }
    }
    result = result.text(&format!("{UNSET} **Unsuccessful {plural}**"));
    if failed.is_empty() {
        result = result.text(all);
    }
    for a in failed {
        result = result.text(&format!("{ARROW} {}", a.text));
    }
    let mut out = vec![result.build()];
    if reason.is_none() && !ok.is_empty() {
        let mut add = Log::new("Add Reason", GREEN);
        for n in ok.iter().filter_map(|a| a.case) {
            add = add.text(&format!("{ARROW} `/cases modify (Case: {n})`"));
        }
        out.push(add.build());
    }
    out
}

/// How long a punishment waits for its DM before it goes ahead anyway.
pub const DM_WAIT: Duration = Duration::from_secs(3);

/// Runs the DM and the action in order. Punishments (`dm_first`) DM first, so a kicked or banned
/// member can still be reached, and act whatever the DM did. Removals act first and only DM once
/// the action worked. A DM that takes longer than [`DM_WAIT`] counts as failed.
pub async fn dm_and_act<T>(
    dm_first: bool,
    dm: Option<impl Future<Output = bool>>,
    act: impl Future<Output = Result<T>>,
) -> (DmOutcome, Result<T>) {
    let send = async move {
        match dm {
            None => DmOutcome::Off,
            Some(f) => match tokio::time::timeout(DM_WAIT, f).await {
                Ok(true) => DmOutcome::Sent,
                _ => DmOutcome::Failed,
            },
        }
    };
    if dm_first {
        let told = send.await;
        (told, act.await)
    } else {
        let done = act.await;
        let told = match done {
            Ok(_) => send.await,
            Err(_) => DmOutcome::Off,
        };
        (told, done)
    }
}

/// A punishment DM goes out when the server's switch is on or the moderator added `?dm`.
pub fn wants_dm(forced: bool, misc: &tommybot_core::sections::MiscCfg) -> bool {
    forced || misc.always_dm
}

/// Punishments DM before they act; their removals act first.
pub fn dm_first(kind: &str) -> bool {
    !matches!(kind, "unquarantine" | "untimeout" | "unban")
}

/// "You have been banned in Server!" with Reason, Moderator (None hides staff) and Duration.
pub fn dm_embed(
    kind: &str,
    server: &str,
    reason: Option<&str>,
    moderator: Option<&str>,
    duration_s: Option<i64>,
) -> Embed {
    let title = match kind {
        "untimeout" => format!("Your timeout was removed in {server}!"),
        k => {
            let what = match k {
                "warn" => "warned",
                "timeout" => "timed out",
                "quarantine" => "quarantined",
                "kick" => "kicked",
                "softban" => "softbanned",
                "tempban" => "temporarily banned",
                "unquarantine" => "unquarantined",
                "unban" => "unbanned",
                _ => "banned",
            };
            format!("You have been {what} in {server}!")
        }
    };
    let mut e = Log::new(&title, BLUE).line("Reason", reason.unwrap_or("No reason."));
    if let Some(m) = moderator {
        e = e.line("Moderator", m);
    }
    if let Some(d) = duration_s.filter(|_| matches!(kind, "timeout" | "tempban")) {
        e = e.line("Duration", &format!("`{}`", duration(d)));
    }
    e.build()
}

/// The line a quarantine DM without a reason gets while tickets are on: tickets stay open to
/// quarantined members, so they can ask why (D27).
fn reach_staff(kind: &str, reason: Option<&str>, tickets: bool) -> Option<&'static str> {
    (kind == "quarantine" && reason.is_none() && tickets)
        .then_some("You can DM TommyBot to reach this server's staff.")
}

/// An automatic quarantine (anti-nuke, a rate limit, the Raider List, a rejoin) tells the member
/// how to reach staff, while tickets are on (D27). Queued as a job, so the DM never holds back the
/// quarantine or the revert after it; a raid wave queues none, so the bot stays off DM limits.
/// Callers skip join and verification quarantines, which get the verification prompt.
pub(crate) async fn queue_quarantine_dm(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) {
    let Ok(cfg) = app.cfg(gid.get()).await else {
        return;
    };
    let now = crate::automod::now_ms();
    let raid = app
        .raids
        .lock()
        .unwrap()
        .get(&gid.get())
        .is_some_and(|r| r.active(now))
        || app
            .voice_raids
            .lock()
            .unwrap()
            .get(&gid.get())
            .is_some_and(|r| r.active(now));
    if !cfg.tickets.enabled || raid {
        return;
    }
    let job = json!({ "user": user.get() });
    if let Err(e) = jobs::enqueue(&app.pool, gid.get() as i64, QUARANTINE_DM, job, now_secs()).await
    {
        tracing::warn!(?e, "queueing the quarantine DM failed");
    }
}

pub(crate) const QUARANTINE_DM: &str = "quarantine_dm";

/// The `quarantine_dm` job: only a member still in a real quarantine, and never a bot. A failed
/// DM is only logged, never retried.
pub(crate) async fn quarantine_dm_job(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(p["user"].as_u64().context("job payload has no user")?);
    if crate::antinuke::quarantine_kind(app, gid, user).await? != Some(false)
        || crate::antinuke::is_bot(app, user).await
    {
        return Ok(());
    }
    let server = crate::verify::guild_name(app, gid).await;
    let text = format!(
        "You have been quarantined in {server}. You can DM TommyBot to reach this server's staff."
    );
    if !dm(app, user, &text).await {
        tracing::warn!(user = user.get(), "quarantine DM failed");
    }
    Ok(())
}

/// What a punishment DM says, see [`dm_embed`]. `moderator` is None when staff is hidden.
pub struct Notice<'a> {
    pub kind: &'a str,
    pub reason: Option<&'a str>,
    pub duration_s: Option<i64>,
    pub moderator: Option<Id<UserMarker>>,
    /// The appeal line under ban and timeout DMs.
    pub appeal: Option<String>,
}

/// Sends the punishment DM; true when it arrived.
pub async fn tell(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>, n: Notice<'_>) -> bool {
    let server = crate::verify::guild_name(app, gid).await;
    let moderator = match n.moderator {
        Some(m) => Some(match app.http.user(m).await {
            Ok(r) => r.model().await.map_or(format!("<@{m}>"), |u| u.name),
            Err(_) => format!("<@{m}>"),
        }),
        None => None,
    };
    let mut e = dm_embed(
        n.kind,
        &server,
        n.reason,
        moderator.as_deref(),
        n.duration_s,
    );
    let tickets = app.cfg(gid.get()).await.is_ok_and(|c| c.tickets.enabled);
    if let Some(line) = reach_staff(n.kind, n.reason, tickets) {
        e.description = Some(format!("{}\n{line}", e.description.unwrap_or_default()));
    }
    send_dm(
        app,
        user,
        n.appeal.as_deref().unwrap_or_default(),
        &[e],
        &[],
    )
    .await
}

/// Best effort, at most [`DM_WAIT`]: closed DMs are normal, other failures are logged. True when sent.
pub async fn dm(app: &App, user: Id<UserMarker>, text: &str) -> bool {
    send_dm(app, user, text, &[], &[]).await
}

/// [`dm`] with button rows under the text.
pub async fn dm_buttons(app: &App, user: Id<UserMarker>, text: &str, rows: &[Component]) -> bool {
    send_dm(app, user, text, &[], rows).await
}

async fn send_dm(
    app: &App,
    user: Id<UserMarker>,
    text: &str,
    embeds: &[Embed],
    rows: &[Component],
) -> bool {
    let send = async {
        let ch = best_effort(app.http.create_private_channel(user).await, "open DM")?;
        let ch = ch.model().await.ok()?;
        best_effort(
            app.http
                .create_message(ch.id)
                .content(text)
                .embeds(embeds)
                .components(rows)
                .await,
            "send DM",
        )
    };
    match tokio::time::timeout(DM_WAIT, send).await {
        Ok(sent) => sent.is_some(),
        Err(_) => {
            // Rate limits during an attack can hold a DM this long; owner alerts must not vanish unseen.
            tracing::warn!(user = user.get(), "DM gave up after {DM_WAIT:?}");
            false
        }
    }
}

/// DMs the server owner that an attack started, so they hear about it even when nobody watches
/// the log channel. At most one DM per server per 10 minutes: a nuke that trips panic mode is one alert.
pub async fn alert_owner(app: &App, gid: Id<GuildMarker>, what: &str) {
    static LAST: Mutex<BTreeMap<u64, i64>> = Mutex::new(BTreeMap::new());
    if !alert_due(&mut LAST.lock().unwrap(), gid.get(), now_secs(), 600) {
        return;
    }
    let info = match app.guild_info(gid).await {
        Ok(i) => i,
        Err(e) => return tracing::warn!(?e, "owner alert: guild lookup failed"),
    };
    let logs = app
        .cfg(gid.get())
        .await
        .ok()
        .and_then(|c| c.logging.target(LogKind::Panic, None))
        .map_or(String::new(), |ch| {
            format!("\nDetails: https://discord.com/channels/{gid}/{ch}")
        });
    let text = format!("{PANIC} **{what}** on your server. TommyBot is handling it.{logs}");
    dm(app, info.owner, &text).await;
}

/// DMs the server owner right away, without [`alert_owner`]'s 10-minute limit.
pub async fn dm_owner(app: &App, gid: Id<GuildMarker>, text: &str) {
    match app.guild_info(gid).await {
        Ok(i) => {
            dm(app, i.owner, text).await;
        }
        Err(e) => tracing::warn!(?e, "owner DM: guild lookup failed"),
    }
}

/// Whether `key` (a guild or channel) is due another alert, at most one per `every` seconds.
pub(crate) fn alert_due(last: &mut BTreeMap<u64, i64>, key: u64, now: i64, every: i64) -> bool {
    if last.get(&key).is_some_and(|t| now - t < every) {
        return false;
    }
    last.insert(key, now);
    true
}

/// Queued as a job so it survives restarts and never blocks the handler.
pub async fn delete_later(
    app: &App,
    gid: Id<GuildMarker>,
    channel: Id<ChannelMarker>,
    msg: Id<MessageMarker>,
    secs: u32,
) -> Result<()> {
    let job = json!({"channel": channel.get(), "message": msg.get()});
    let at = now_secs() + i64::from(secs);
    jobs::enqueue(&app.pool, gid.get() as i64, "delete_message", job, at).await?;
    Ok(())
}

/// Scheduler arm for `delete_message`; the scheduler drops the job when the message is already gone.
pub async fn run_delete(app: &App, payload: &Value) -> Result<()> {
    let id = |k: &str| payload[k].as_u64().with_context(|| format!("no {k}"));
    app.http
        .delete_message(Id::new(id("channel")?), Id::new(id("message")?))
        .await?;
    Ok(())
}

/// Bulk delete `user`'s messages among the channel's last 100. Discord refuses messages older than 14 days.
pub async fn purge_user(app: &App, channel: Id<ChannelMarker>, user: Id<UserMarker>) -> Result<()> {
    let msgs = app.http.channel_messages(channel).limit(100).await?;
    let cutoff = now_secs() - 14 * 86_400 + 60;
    let ids: Vec<_> = msgs
        .models()
        .await?
        .into_iter()
        .filter(|m| m.author.id == user && m.timestamp.as_secs() > cutoff)
        .map(|m| m.id)
        .collect();
    delete_batch(app, channel, &ids).await
}

/// Deletes tracked (channel, message) pairs, one bulk delete per channel; too old ones are skipped.
pub async fn purge_ids(app: &App, msgs: &[(u64, u64)]) {
    for (c, ids) in batches(msgs, now_secs()) {
        if let Err(e) = delete_batch(app, Id::new(c), &ids).await {
            tracing::warn!(?e, channel = c, "purging tracked messages failed");
        }
    }
}

/// Groups messages younger than 14 days by channel, at most 100 per group (Discord's bulk delete cap).
fn batches(msgs: &[(u64, u64)], now: i64) -> Vec<(u64, Vec<Id<MessageMarker>>)> {
    let cutoff_ms = (now - 14 * 86_400 + 60) * 1000;
    let mut by_channel: HashMap<u64, Vec<Id<MessageMarker>>> = HashMap::new();
    for &(c, m) in msgs {
        // Snowflake timestamp: ms since the Discord epoch in the top bits.
        if ((m >> 22) + 1_420_070_400_000) as i64 > cutoff_ms {
            by_channel.entry(c).or_default().push(Id::new(m));
        }
    }
    by_channel
        .into_iter()
        .flat_map(|(c, ids)| {
            ids.chunks(100)
                .map(|ch| (c, ch.to_vec()))
                .collect::<Vec<_>>()
        })
        .collect()
}

async fn delete_batch(
    app: &App,
    channel: Id<ChannelMarker>,
    ids: &[Id<MessageMarker>],
) -> Result<()> {
    match ids {
        [] => {}
        // Already deleted (by the author or another moderator) is fine.
        [one] => match app.http.delete_message(channel, *one).await {
            Err(e) if !gone(&e) => return Err(e.into()),
            _ => {}
        },
        _ => {
            app.http.delete_messages(channel, ids).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_text_forms() {
        assert_eq!(
            case_text(true, (3, "ban", 5), Some("x"), 9),
            "Case #3: ban <@5>"
        );
        assert_eq!(
            case_text(false, (3, "ban", 5), Some("x"), 9),
            "Case #3: ban <@5>\nModerator: <@9>\nReason: x"
        );
        assert!(case_text(true, (3, "ban", 5), None, 9)
            .ends_with("\nAdd Reason: `/cases modify case:3 reason:`"));
    }

    fn ok(case: i32, target: u64, dm: DmOutcome) -> Applied {
        Applied {
            target: Some(target),
            case: Some(case),
            text: format!("Case #{case}: kick <@{target}>"),
            dm,
        }
    }

    fn refused(target: u64) -> Applied {
        Applied {
            target: Some(target),
            ..format!("Cannot kick <@{target}>: they are immune").into()
        }
    }

    fn desc(e: &[Embed]) -> String {
        e[0].description.clone().unwrap()
    }

    #[test]
    fn compact_summary_counts() {
        let t = [ok(1, 5, DmOutcome::Off), refused(6)];
        assert_eq!(
            summary("kick", true, &t, Some("spam"), 9),
            "✅ Successful kicks: `1`\n❌ Unsuccessful kicks: `1`\n📄 Reason: spam\n👤 Moderator: <@9>\nCannot kick <@6>: they are immune"
        );
        assert_eq!(
            summary("kick", false, &t, None, 9),
            "Case #1: kick <@5>\nCannot kick <@6>: they are immune"
        );
        assert_eq!(
            summary("kick", true, &t[..1], None, 9),
            "Case #1: kick <@5>"
        );
    }

    #[test]
    fn result_lists_successful_and_unsuccessful() {
        let t = [ok(4, 5, DmOutcome::Sent), refused(6)];
        let e = result_embeds("timeout", &t, (Flags::default(), Some(300)), None, 9);
        assert_eq!(e[0].title.as_deref(), Some("Timeout result:"));
        let d = desc(&e);
        let succ = d.find("Successful timeouts").unwrap();
        let fail = d.find("Unsuccessful timeouts").unwrap();
        assert!(d.contains("No reason.") && d.contains("Moderator:** <@9>"));
        assert!(d.find("Details:").unwrap() < succ && succ < fail);
        assert!(d.contains("Duration: `5m` ✅") && d.contains("DM-Members: ✅"));
        assert!(d[succ..fail].contains("<@5> [`5`]"));
        assert!(d[fail..].contains("Cannot kick <@6>"));
        assert!(!d.contains("All members"));
        // Add Reason lists only the cases that were made.
        assert_eq!(e.len(), 2);
        assert!(desc(&e[1..]).contains("(Case: 4)"));
        let one = result_embeds("timeout", &t, (Flags::default(), Some(300)), Some("x"), 9);
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn nothing_failed_says_so() {
        let t = [ok(1, 5, DmOutcome::Off)];
        for (kind, all) in [
            ("ban", "All members were banned!"),
            ("kick", "All members were kicked!"),
            ("warn", "All members were warned!"),
            ("timeout", "All members were timed out!"),
            ("quarantine", "All members were quarantined!"),
            ("softban", "All members were softbanned!"),
            ("unban", "All members were unbanned!"),
        ] {
            assert!(
                desc(&result_embeds(kind, &t, Default::default(), Some("r"), 9)).contains(all),
                "{kind}"
            );
        }
    }

    fn flags(
        kind: &str,
        f: Flags,
        dur: Option<i64>,
        done: &[Applied],
    ) -> Vec<(&'static str, String)> {
        details(kind, f, dur, done)
    }

    #[test]
    fn details_per_action() {
        let none = [ok(1, 5, DmOutcome::Off)];
        let names =
            |v: Vec<(&'static str, String)>| v.into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert_eq!(
            names(flags("ban", Flags::default(), None, &none)),
            ["Duration", "Soft Ban", "Hack Ban", "DM-Members"]
        );
        assert_eq!(
            names(flags("kick", Flags::default(), None, &none)),
            ["No-Avatar", "Voice-Channels", "DM-Members"]
        );
        assert_eq!(
            names(flags("timeout", Flags::default(), Some(60), &none)),
            ["Duration", "DM-Members"]
        );
        for k in ["warn", "quarantine", "unquarantine", "untimeout", "unban"] {
            assert_eq!(
                names(flags(k, Flags::default(), None, &none)),
                ["DM-Members"],
                "{k}"
            );
        }
        assert!(flags("purge", Flags::default(), None, &none).is_empty());
        // Each flag says whether it was used.
        let f = Flags {
            hack: true,
            no_avatar: true,
            ..Default::default()
        };
        assert_eq!(
            flags("softban", f, Some(86_400), &none),
            [
                ("Duration", "❎".to_string()),
                ("Soft Ban", "✅".into()),
                ("Hack Ban", "✅".into()),
                ("DM-Members", "❎".into())
            ]
        );
        assert_eq!(
            flags("kick", f, None, &none)[..2],
            [
                ("No-Avatar", "✅".to_string()),
                ("Voice-Channels", "❎".into())
            ]
        );
        assert_eq!(flags("ban", Flags::default(), None, &none)[0].1, "❎");
    }

    #[test]
    fn dm_members_tells_the_truth() {
        let dm = |d: &[DmOutcome]| {
            let done: Vec<_> = d.iter().map(|&o| ok(1, 5, o)).collect();
            dm_flag(&done)
        };
        assert_eq!(dm(&[DmOutcome::Sent, DmOutcome::Sent]), "✅");
        assert_eq!(dm(&[DmOutcome::Off]), "❎");
        assert_eq!(dm(&[DmOutcome::Failed]), "❎");
        assert_eq!(
            dm(&[DmOutcome::Sent, DmOutcome::Sent, DmOutcome::Failed]),
            "✅ 2 | ❎ 1"
        );
    }

    #[test]
    fn other_commands_get_result_embeds() {
        let t = [Applied {
            case: Some(5),
            text: "Case #5: Deleted 7 messages".into(),
            ..Default::default()
        }];
        let d = desc(&result_embeds(
            "purge",
            &t,
            Default::default(),
            Some("x"),
            9,
        ));
        assert!(d.contains("Successful purges") && d.contains("Deleted 7 messages"));
        assert!(!d.contains("Details:"));
        let t = [Applied {
            target: Some(5),
            case: Some(6),
            text: "Case #6: Renamed <@5> to `bob`".into(),
            ..Default::default()
        }];
        let d = desc(&result_embeds(
            "sanitize",
            &t,
            Default::default(),
            Some("x"),
            9,
        ));
        assert!(d.contains("<@5> [`5`]"));
    }

    /// Records the order of the DM and the action.
    async fn step(log: &Mutex<Vec<&'static str>>, what: &'static str) {
        log.lock().unwrap().push(what);
    }

    #[tokio::test]
    async fn punishments_dm_before_they_act() {
        for kind in [
            "warn",
            "timeout",
            "quarantine",
            "kick",
            "ban",
            "softban",
            "tempban",
        ] {
            assert!(dm_first(kind), "{kind}");
            let log = Mutex::new(vec![]);
            let dm = async {
                step(&log, "dm").await;
                true
            };
            let act = async {
                step(&log, "act").await;
                anyhow::Ok(())
            };
            let (told, r) = dm_and_act(dm_first(kind), Some(dm), act).await;
            assert!(r.is_ok());
            assert_eq!(
                (told, log.into_inner().unwrap()),
                (DmOutcome::Sent, vec!["dm", "act"])
            );
        }
        for kind in ["unquarantine", "untimeout", "unban"] {
            assert!(!dm_first(kind), "{kind}");
        }
    }

    #[tokio::test]
    async fn failed_dm_still_punishes() {
        let acted = Mutex::new(false);
        let act = async {
            *acted.lock().unwrap() = true;
            anyhow::Ok(())
        };
        let (told, r) = dm_and_act(true, Some(async { false }), act).await;
        assert!(r.is_ok() && *acted.lock().unwrap());
        assert_eq!(told, DmOutcome::Failed);
        let e = result_embeds("ban", &[ok(1, 5, told)], Default::default(), Some("r"), 9);
        assert!(desc(&e).contains("DM-Members: ❎"));
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_dm_gives_up_after_three_seconds() {
        let start = tokio::time::Instant::now();
        let act = async { anyhow::Ok(tokio::time::Instant::now()) };
        let (told, r) = dm_and_act(true, Some(std::future::pending::<bool>()), act).await;
        assert_eq!(told, DmOutcome::Failed);
        assert_eq!(r.unwrap() - start, DM_WAIT);
        assert_eq!(DM_WAIT, Duration::from_secs(3));
    }

    #[tokio::test]
    async fn removals_act_first_and_skip_the_dm_when_they_fail() {
        let log = Mutex::new(vec![]);
        let dm = async {
            step(&log, "dm").await;
            false
        };
        let act = async {
            step(&log, "act").await;
            anyhow::Ok(())
        };
        // An unban DM usually fails (no shared server): the unban still happened, the result shows ❎.
        let (told, r) = dm_and_act(dm_first("unban"), Some(dm), act).await;
        assert!(r.is_ok());
        assert_eq!(
            (told, log.into_inner().unwrap()),
            (DmOutcome::Failed, vec!["act", "dm"])
        );
        let failed = async { Err::<(), _>(anyhow::anyhow!("no ban")) };
        let (told, r) = dm_and_act(false, Some(async { true }), failed).await;
        assert!(r.is_err());
        assert_eq!(told, DmOutcome::Off);
    }

    #[tokio::test]
    async fn toggle_off_sends_nothing() {
        let none: Option<std::future::Ready<bool>> = None;
        for first in [true, false] {
            let (told, _) = dm_and_act(first, none.clone(), async { anyhow::Ok(()) }).await;
            assert_eq!(told, DmOutcome::Off);
        }
    }

    #[test]
    fn one_switch_for_every_dm() {
        let mut misc = tommybot_core::sections::MiscCfg::default();
        assert!(misc.always_dm, "on by default");
        assert!(wants_dm(false, &misc));
        misc.always_dm = false;
        assert!(!wants_dm(false, &misc));
        assert!(wants_dm(true, &misc), "?dm still sends one");
    }

    #[test]
    fn a_quarantine_dm_without_a_reason_says_how_to_reach_staff() {
        assert!(reach_staff("quarantine", None, true).is_some());
        assert_eq!(reach_staff("quarantine", Some("raid"), true), None);
        assert_eq!(reach_staff("quarantine", None, false), None, "tickets off");
        assert_eq!(reach_staff("ban", None, true), None);
    }

    #[test]
    fn dm_embeds() {
        let e = dm_embed("warn", "WellBeing", None, Some("mod"), None);
        assert_eq!(
            e.title.as_deref(),
            Some("You have been warned in WellBeing!")
        );
        let d = e.description.unwrap();
        assert!(d.contains("**Reason:** No reason.") && d.contains("**Moderator:** mod"));
        assert!(!d.contains("Duration"));
        let e = dm_embed("timeout", "S", Some("spam"), None, Some(300));
        assert_eq!(e.title.as_deref(), Some("You have been timed out in S!"));
        let d = e.description.unwrap();
        assert!(d.contains("**Reason:** spam") && d.contains("**Duration:** `5m`"));
        assert!(!d.contains("Moderator"));
        for (k, t) in [
            ("quarantine", "You have been quarantined in S!"),
            ("kick", "You have been kicked in S!"),
            ("ban", "You have been banned in S!"),
            ("softban", "You have been softbanned in S!"),
            ("tempban", "You have been temporarily banned in S!"),
            ("unquarantine", "You have been unquarantined in S!"),
            ("untimeout", "Your timeout was removed in S!"),
            ("unban", "You have been unbanned in S!"),
        ] {
            assert_eq!(dm_embed(k, "S", None, None, None).title.as_deref(), Some(t));
        }
        // A ban shows no duration line, a tempban does.
        assert!(!dm_embed("ban", "S", None, None, Some(60))
            .description
            .unwrap()
            .contains("Duration"));
        assert!(dm_embed("tempban", "S", None, None, Some(60))
            .description
            .unwrap()
            .contains("`1m`"));
    }

    #[test]
    fn batches_cap_at_100_and_skip_old() {
        // A snowflake made "now" in ms since the Discord epoch.
        let now = 1_800_000_000;
        let fresh = ((now as u64 * 1000) - 1_420_070_400_000) << 22;
        let old = ((now as u64 - 15 * 86_400) * 1000 - 1_420_070_400_000) << 22;
        let mut msgs: Vec<(u64, u64)> = (0..150).map(|i| (1, fresh + i)).collect();
        msgs.push((1, old));
        msgs.push((2, fresh));
        let mut b = batches(&msgs, now);
        b.sort_by_key(|(c, ids)| (*c, std::cmp::Reverse(ids.len())));
        let sizes: Vec<_> = b.iter().map(|(c, ids)| (*c, ids.len())).collect();
        assert_eq!(sizes, [(1, 100), (1, 50), (2, 1)]);
    }

    #[test]
    fn owner_alert_waits_ten_minutes_per_server() {
        let mut last = BTreeMap::new();
        assert!(alert_due(&mut last, 1, 1000, 600));
        assert!(!alert_due(&mut last, 1, 1599, 600));
        assert!(alert_due(&mut last, 2, 1599, 600));
        assert!(alert_due(&mut last, 1, 1600, 600));
    }
}

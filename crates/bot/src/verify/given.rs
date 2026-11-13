//! A verification role given by another bot (D58): while TommyBot's verification is on, the role
//! is taken back and nobody is punished, as the quarantine hold does for a join quarantine (#170).
//! Switching verification off lets another bot verify instead. A role counts when it opens what
//! a verification role opens and @everyone lacks: View Channel, or Send Messages where @everyone
//! only reads, by base permission or channel overwrite, or Administrator. A bot's member overwrite
//! that opens such a channel is removed too. Grants made while TommyBot was down are found at
//! GuildCreate from the audit log ([`catch_up`]).
//! Not covered: a channel opened only by two roles together (each is judged alone), a member
//! overwrite given while TommyBot was down, and grants older than the newest 500 role changes in
//! the audit log.

use crate::{app::App, app::GuildCfg, events::send_log};
use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tommybot_logging::{
    embed::{NO, ORANGE},
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    channel::Channel,
    guild::{
        audit_log::{AuditLogChange, AuditLogEntry, AuditLogEventType as A},
        Permissions,
    },
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

/// Roles another bot gave a member, and TommyBot's own verifications, by (guild, member).
type Recent = Mutex<HashMap<(u64, u64), (Vec<Id<RoleMarker>>, Instant)>>;
static BOT_GIVEN: LazyLock<Recent> = LazyLock::new(Default::default);
static VERIFIED: LazyLock<Recent> = LazyLock::new(Default::default);
/// Members found verified already, so a reaction-role click costs no member read again.
static SEEN_VERIFIED: LazyLock<Recent> = LazyLock::new(Default::default);
type Locks = Mutex<HashMap<(u64, u64), Arc<tokio::sync::Mutex<()>>>>;
static MEMBER_LOCKS: LazyLock<Locks> = LazyLock::new(Default::default);
static GATES: LazyLock<Mutex<HashMap<u64, Arc<Gate>>>> = LazyLock::new(Default::default);
/// Whether a user is a bot; it never changes, so it is kept.
static BOTS: LazyLock<Mutex<HashMap<u64, bool>>> = LazyLock::new(Default::default);

/// How long a grant counts as recent, and how long a handler waits for a bot's other grants.
const RECENT: Duration = Duration::from_secs(60);
const SETTLE: Duration = Duration::from_millis(1500);
const SEEN_FOR: Duration = Duration::from_secs(600);
const GATE_FOR: Duration = Duration::from_secs(60);
const VIEW: u64 = Permissions::VIEW_CHANNEL.bits();
const SEND: u64 = Permissions::SEND_MESSAGES.bits();
const ADMIN: u64 = Permissions::ADMINISTRATOR.bits();

/// What verification opens in a guild: `gated` holds, per channel, the View or Send bits a
/// verification role gives there and @everyone lacks; `roles` the roles that give one of them
/// (verification roles and Administrator included).
#[derive(Debug, Default)]
pub(crate) struct Gate {
    at: Option<Instant>,
    verify: Vec<Id<RoleMarker>>,
    known: HashSet<Id<RoleMarker>>,
    pub(crate) roles: HashSet<Id<RoleMarker>>,
    pub(crate) gated: HashMap<Id<ChannelMarker>, u64>,
}

/// Builds [`Gate`] from unlocked role permissions and the channels' overwrites. Each role is
/// judged alone, in Discord's order: base, then @everyone's overwrite, then the role's own.
pub(crate) fn gate_of(
    gid: Id<GuildMarker>,
    verify: &[Id<RoleMarker>],
    perms: &HashMap<Id<RoleMarker>, u64>,
    channels: &[Channel],
) -> Gate {
    let everyone: Id<RoleMarker> = gid.cast();
    let base = |r: Id<RoleMarker>| perms.get(&r).copied().unwrap_or(0);
    let ow = |c: &Channel, r: Id<RoleMarker>| {
        c.permission_overwrites
            .iter()
            .flatten()
            .find(|o| o.id == r.cast())
            .map_or((0, 0), |o| (o.allow.bits(), o.deny.bits()))
    };
    let mut g = Gate {
        at: Some(Instant::now()),
        verify: verify.to_vec(),
        known: perms.keys().copied().collect(),
        ..Default::default()
    };
    for c in channels {
        let (e_allow, e_deny) = ow(c, everyone);
        let has = |r: Id<RoleMarker>, x: u64| {
            let (allow, deny) = ow(c, r);
            allow & x != 0 || (base(r) & x != 0 && (e_deny | deny) & x == 0)
        };
        let all = |x: u64| (base(everyone) & x != 0 && e_deny & x == 0) || e_allow & x != 0;
        let mut bits = 0;
        if !all(VIEW) && verify.iter().any(|v| has(*v, VIEW)) {
            bits |= VIEW;
        }
        // Send counts only where @everyone already reads: elsewhere View covers it.
        if all(VIEW) && !all(SEND) && verify.iter().any(|v| has(*v, SEND)) {
            bits |= SEND;
        }
        if bits != 0 {
            g.gated.insert(c.id, bits);
            g.roles.extend(
                perms
                    .keys()
                    .filter(|r| **r != everyone && (has(**r, bits & VIEW) || has(**r, bits & SEND))),
            );
        }
    }
    g.roles.extend(verify);
    g.roles.extend(perms.iter().filter(|(r, p)| **r != everyone && *p & ADMIN != 0).map(|(r, _)| *r));
    g
}

/// The guild's [`Gate`], rebuilt after a minute, on a config change, or for a role it lacks.
async fn gate(
    app: &App,
    gid: Id<GuildMarker>,
    verify: &[Id<RoleMarker>],
    added: &[Id<RoleMarker>],
) -> Result<Arc<Gate>> {
    if let Some(g) = GATES.lock().unwrap_or_else(|p| p.into_inner()).get(&gid.get()) {
        let fresh = g.at.is_some_and(|at| at.elapsed() < GATE_FOR);
        if fresh && g.verify == verify && added.iter().all(|r| g.known.contains(r)) {
            return Ok(g.clone());
        }
    }
    // Read fresh: a bot may give a role created a moment ago.
    let live = app.http.roles(gid).await?.models().await?;
    let live = live.iter().map(|r| (r.id, r.permissions.bits())).collect();
    let perms = crate::lockdown::unlocked(app, gid, live).await?;
    let channels = app.http.guild_channels(gid).await?.models().await?;
    let g = Arc::new(gate_of(gid, verify, &perms, &channels));
    GATES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(gid.get(), g.clone());
    Ok(g)
}

fn note(map: &Recent, gid: Id<GuildMarker>, user: Id<UserMarker>, roles: &[Id<RoleMarker>]) {
    let mut all = map.lock().unwrap_or_else(|p| p.into_inner());
    all.retain(|_, (_, at)| at.elapsed() < SEEN_FOR);
    let e = all
        .entry((gid.get(), user.get()))
        .or_insert_with(|| (vec![], Instant::now()));
    e.0.extend_from_slice(roles);
    e.1 = Instant::now();
}

fn recent(
    map: &Recent,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    within: Duration,
) -> Option<Vec<Id<RoleMarker>>> {
    let all = map.lock().unwrap_or_else(|p| p.into_inner());
    all.get(&(gid.get(), user.get()))
        .filter(|(_, at)| at.elapsed() < within)
        .map(|(r, _)| r.clone())
}

/// TommyBot verified this member: a bot's grant around the same time takes nothing away.
pub(crate) fn note_verified(gid: Id<GuildMarker>, user: Id<UserMarker>) {
    note(&VERIFIED, gid, user, &[]);
}

fn member_lock(gid: Id<GuildMarker>, user: Id<UserMarker>) -> Arc<tokio::sync::Mutex<()>> {
    let mut all = MEMBER_LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    all.retain(|_, l| Arc::strong_count(l) > 1);
    all.entry((gid.get(), user.get())).or_default().clone()
}

/// A bot account, cached. A failed lookup is tried once more after 2 s.
async fn is_bot(app: &App, user: Id<UserMarker>) -> Result<bool> {
    if let Some(b) = BOTS.lock().unwrap_or_else(|p| p.into_inner()).get(&user.get()) {
        return Ok(*b);
    }
    let get = || async { anyhow::Ok(app.http.user(user).await?.model().await?.bot) };
    let b = match get().await {
        Ok(b) => b,
        Err(_) => {
            tokio::time::sleep(Duration::from_secs(2)).await;
            get().await?
        }
    };
    let mut all = BOTS.lock().unwrap_or_else(|p| p.into_inner());
    if all.len() > 10_000 {
        all.clear();
    }
    all.insert(user.get(), b);
    Ok(b)
}

fn roles_added(e: &AuditLogEntry) -> Vec<Id<RoleMarker>> {
    e.changes
        .iter()
        .filter_map(|c| match c {
            AuditLogChange::RoleAdded { new, .. } => Some(new),
            _ => None,
        })
        .flatten()
        .map(|r| r.id)
        .collect()
}

pub async fn on_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
    match e.action_type {
        A::MemberRoleUpdate => on_roles(app, e).await,
        A::ChannelOverwriteCreate | A::ChannelOverwriteUpdate => on_overwrite(app, e).await,
        _ => Ok(()),
    }
}

async fn on_roles(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (Some(gid), Some(actor), Some(target)) = (e.guild_id, e.user_id, e.target_id) else {
        return Ok(());
    };
    let added = roles_added(e);
    if actor == app.me || added.is_empty() {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let v = &cfg.verification;
    if !v.enabled || v.roles.is_empty() {
        return Ok(());
    }
    let target: Id<UserMarker> = target.cast();
    // Cheap filters first: no REST call for a role that opens nothing verification opens, or
    // for a member found verified already.
    let gate = gate(app, gid, &v.roles, &added).await?;
    if !added.iter().any(|r| gate.roles.contains(r))
        || recent(&SEEN_VERIFIED, gid, target, SEEN_FOR).is_some()
        || hold_takes_it(app, &cfg, gid, target, actor).await?
    {
        return Ok(());
    }
    match is_bot(app, actor).await {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(e) => return failed(app, gid, target, actor, &e).await,
    }
    note(&BOT_GIVEN, gid, target, &added);
    // A bot may give two verification roles in two calls: wait for both, then judge them one
    // member at a time, so neither counts as proof that the member was verified already.
    tokio::time::sleep(SETTLE).await;
    let lock = member_lock(gid, target);
    let _one = lock.lock().await;
    let bot_given = recent(&BOT_GIVEN, gid, target, RECENT).unwrap_or_default();
    let taken = match take_back(app, &cfg, &gate, gid, target, &added, &bot_given).await {
        Ok(t) => Ok(t),
        Err(e) => {
            tracing::warn!(?e, "taking back a verification role failed, trying again");
            tokio::time::sleep(Duration::from_secs(2)).await;
            take_back(app, &cfg, &gate, gid, target, &added, &bot_given).await
        }
    };
    match taken {
        Ok(t) if t.is_empty() => Ok(()),
        Ok(_) => taken_log(app, gid, target, actor, "A verification role given by another bot was taken back").await,
        Err(e) => failed(app, gid, target, actor, &e).await,
    }
}

/// A bot's member overwrite opening a gated channel to an unverified member is removed.
async fn on_overwrite(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (Some(gid), Some(actor), Some(channel), Some(o)) =
        (e.guild_id, e.user_id, e.target_id, e.options.as_ref())
    else {
        return Ok(());
    };
    let (Some("1"), Some(member)) = (o.kind.as_deref(), o.id) else {
        return Ok(());
    };
    let allow = e
        .changes
        .iter()
        .find_map(|c| match c {
            AuditLogChange::Allow { new: Some(p), .. } => Some(p.bits()),
            _ => None,
        })
        .unwrap_or(0);
    if actor == app.me || allow & (VIEW | SEND) == 0 {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let v = &cfg.verification;
    if !v.enabled || v.roles.is_empty() {
        return Ok(());
    }
    let (channel, target): (Id<ChannelMarker>, Id<UserMarker>) = (channel.cast(), member.cast());
    let gate = gate(app, gid, &v.roles, &[]).await?;
    if gate.gated.get(&channel).is_none_or(|g| g & allow == 0)
        || recent(&SEEN_VERIFIED, gid, target, SEEN_FOR).is_some()
    {
        return Ok(());
    }
    match is_bot(app, actor).await {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(e) => return failed(app, gid, target, actor, &e).await,
    }
    let lock = member_lock(gid, target);
    let _one = lock.lock().await;
    let remove = || async {
        if recent(&VERIFIED, gid, target, RECENT).is_some() {
            return anyhow::Ok(false);
        }
        let Some(m) = crate::http_err::member(&app.http, gid, target).await? else {
            return Ok(false);
        };
        if m.roles.iter().any(|r| v.roles.contains(r)) {
            note(&SEEN_VERIFIED, gid, target, &[]);
            return Ok(false);
        }
        app.http
            .delete_channel_permission(channel)
            .member(target)
            .reason("Channel access given by another bot before verification")
            .await?;
        Ok(true)
    };
    let done = match remove().await {
        Ok(d) => Ok(d),
        Err(_) => {
            tokio::time::sleep(Duration::from_secs(2)).await;
            remove().await
        }
    };
    match done {
        Ok(false) => Ok(()),
        Ok(true) => taken_log(app, gid, target, actor, "A channel permission given by another bot was taken back").await,
        Err(e) => failed(app, gid, target, actor, &e).await,
    }
}

/// The quarantine hold takes back every role a held member is given, except from a Quarantine
/// Whitelist actor. It acts on a join quarantine always and on any other only with Quarantine
/// Hold on; everything else is judged here.
async fn hold_takes_it(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    actor: Id<UserMarker>,
) -> Result<bool> {
    if !cfg.antinuke.enabled {
        return Ok(false);
    }
    let Some(on_join) = crate::antinuke::quarantine_kind(app, gid, target).await? else {
        return Ok(false);
    };
    if !on_join && !cfg.antinuke.quarantine_hold {
        return Ok(false);
    }
    let listed = cfg.antinuke.quarantine_whitelist.contains(&actor)
        || app.whitelisted(gid, actor, "quarantine").await?;
    Ok(!listed)
}

/// Removes what the bot gave an unverified member that verification would give: a role in
/// [`Gate::roles`]. Returns what was taken.
async fn take_back(
    app: &App,
    cfg: &GuildCfg,
    gate: &Gate,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    added: &[Id<RoleMarker>],
    bot_given: &[Id<RoleMarker>],
) -> Result<Vec<Id<RoleMarker>>> {
    let v = &cfg.verification;
    if recent(&VERIFIED, gid, target, RECENT).is_some() {
        return Ok(vec![]);
    }
    let Some(member) = crate::http_err::member(&app.http, gid, target).await? else {
        return Ok(vec![]);
    };
    // Verified already: holds a verification role no bot gave just now.
    if member
        .roles
        .iter()
        .any(|r| v.roles.contains(r) && !bot_given.contains(r))
    {
        note(&SEEN_VERIFIED, gid, target, &[]);
        return Ok(vec![]);
    }
    let taken: Vec<Id<RoleMarker>> = added
        .iter()
        .copied()
        .filter(|r| member.roles.contains(r) && gate.roles.contains(r))
        .collect();
    remove_roles(app, gid, target, &taken).await?;
    Ok(taken)
}

async fn remove_roles(
    app: &App,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    roles: &[Id<RoleMarker>],
) -> Result<()> {
    for r in roles {
        app.http
            .remove_guild_member_role(gid, target, *r)
            .reason("Verification role given by another bot")
            .await?;
    }
    Ok(())
}

/// Grants made while TommyBot was down (GuildCreate): the newest add of each role in the audit
/// log says who gave it. A member whose verification roles all came from another bot loses the
/// roles that bot gave in [`Gate::roles`]; a role given by TommyBot, by staff or before the log
/// counts as verified.
pub async fn catch_up(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let v = &cfg.verification;
    if !v.enabled || v.roles.is_empty() {
        return Ok(());
    }
    let gate = gate(app, gid, &v.roles, &[]).await?;
    // (member, role) -> who added it last. Entries come newest first.
    let mut by: HashMap<(Id<UserMarker>, Id<RoleMarker>), Id<UserMarker>> = HashMap::new();
    let mut before = None;
    // shortcut: reads the newest 500 role changes; read further if outages get long and busy.
    for _ in 0..5 {
        let req = app.http.audit_log(gid).action_type(A::MemberRoleUpdate).limit(100);
        let req = match before {
            Some(b) => req.before(b),
            None => req,
        };
        let log = req.await?.model().await?;
        for e in &log.entries {
            let (Some(actor), Some(t)) = (e.user_id, e.target_id) else {
                continue;
            };
            for r in roles_added(e).into_iter().filter(|r| gate.roles.contains(r)) {
                by.entry((t.cast(), r)).or_insert(actor);
            }
        }
        match log.entries.last() {
            Some(e) if log.entries.len() == 100 => before = Some(e.id.get()),
            _ => break,
        }
    }
    let mut bots = HashSet::new();
    for actor in by.values().copied().collect::<HashSet<_>>() {
        if actor != app.me && is_bot(app, actor).await? {
            bots.insert(actor);
        }
    }
    let members: HashSet<Id<UserMarker>> = by
        .iter()
        .filter(|(_, a)| bots.contains(*a))
        .map(|((u, _), _)| *u)
        .collect();
    for target in members {
        let from_bot = |r: &Id<RoleMarker>| by.get(&(target, *r)).is_some_and(|a| bots.contains(a));
        let lock = member_lock(gid, target);
        let _one = lock.lock().await;
        if recent(&VERIFIED, gid, target, RECENT).is_some() {
            continue;
        }
        let Some(member) = crate::http_err::member(&app.http, gid, target).await? else {
            continue;
        };
        let actor = by
            .iter()
            .find(|((u, _), a)| *u == target && bots.contains(*a))
            .map(|(_, a)| *a)
            .unwrap_or(app.me);
        if member.roles.iter().any(|r| v.roles.contains(r) && !from_bot(r))
            || hold_takes_it(app, &cfg, gid, target, actor).await?
        {
            continue;
        }
        let taken: Vec<_> = member
            .roles
            .iter()
            .copied()
            .filter(|r| gate.roles.contains(r) && from_bot(r))
            .collect();
        if taken.is_empty() {
            continue;
        }
        match remove_roles(app, gid, target, &taken).await {
            Ok(()) => taken_log(app, gid, target, actor, "A verification role given by another bot while TommyBot was offline was taken back").await?,
            Err(e) => failed(app, gid, target, actor, &e).await?,
        }
    }
    Ok(())
}

async fn taken_log(
    app: &App,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    actor: Id<UserMarker>,
    what: &str,
) -> Result<()> {
    let log = Log::new("Verification", ORANGE)
        .user("Member", target.get())
        .user("Bot", actor.get())
        .text(&format!("{NO} **{what}**"))
        .line(
            "Why",
            "TommyBot's verification is on, so members verify through it. Turn verification off to let another bot verify.",
        );
    if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
        tracing::warn!(?e, "verification role log failed");
    }
    Ok(())
}

/// Lookup or removal failed twice: staff hear that the access may have stayed (D12b rule).
async fn failed(
    app: &App,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    actor: Id<UserMarker>,
    e: &anyhow::Error,
) -> Result<()> {
    tracing::warn!(?e, "taking back a verification role failed twice");
    let log = Log::new("Verification", ORANGE)
        .user("Member", target.get())
        .user("Bot", actor.get())
        .text(&format!(
            "{NO} **Could not take back a verification role given by another bot**"
        ))
        .line("Error", &crate::http_err::why(e))
        .line("Effect", "The member may skip verification. Check their roles.");
    if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
        tracing::warn!(?e, "verification role failure log failed");
    }
    Ok(())
}

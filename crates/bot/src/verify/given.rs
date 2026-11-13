//! A verification role given by another bot (D58), or by a human without a verify or
//! quarantine permit who is not Owner, EO or TA (Thomas, 13:53): while TommyBot's verification is on, the role
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
/// Per guild: the cached [`Gate`] with how long it stays fresh, and a generation that a hard
/// forget bumps, so a gate read while a role or channel changed is never stored.
#[derive(Default)]
struct Slot {
    gate: Option<(Arc<Gate>, Duration)>,
    /// (hard forgets, softens) so far.
    gen: (u64, u64),
}
static GATES: LazyLock<Mutex<HashMap<u64, Slot>>> = LazyLock::new(Default::default);
/// Whether a user is a bot; it never changes, so it is kept.
static BOTS: LazyLock<Mutex<HashMap<u64, bool>>> = LazyLock::new(Default::default);

/// How long a grant counts as recent, and how long a handler waits for a bot's other grants.
const RECENT: Duration = Duration::from_secs(60);
const SETTLE: Duration = Duration::from_millis(1500);
const SEEN_FOR: Duration = Duration::from_secs(600);
const GATE_FOR: Duration = Duration::from_secs(60);
/// TommyBot's own role and overwrite edits (a lock applying to every channel) rebuild the gate
/// at most this often.
const GATE_COALESCE: Duration = Duration::from_secs(3);
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
    let gen = {
        let all = GATES.lock().unwrap_or_else(|p| p.into_inner());
        let slot = all.get(&gid.get());
        if let Some((g, fresh_for)) = slot.and_then(|s| s.gate.as_ref()) {
            let fresh = g.at.is_some_and(|at| at.elapsed() < *fresh_for);
            if fresh && g.verify == verify && added.iter().all(|r| g.known.contains(r)) {
                return Ok(g.clone());
            }
        }
        slot.map_or((0, 0), |s| s.gen)
    };
    // Read fresh: a bot may give a role created a moment ago.
    let live = app.http.roles(gid).await?.models().await?;
    let live = live.iter().map(|r| (r.id, r.permissions.bits())).collect();
    let perms = crate::lockdown::unlocked(app, gid, live).await?;
    let mut channels = app.http.guild_channels(gid).await?.models().await?;
    // A channel lock's denies and panic's Send exemption are not what verification opens:
    // judge the overwrites they saved.
    crate::lockdown::unlocked_channels(app, gid, &mut channels).await?;
    crate::panic::unexempted(app, gid, &mut channels).await?;
    let g = Arc::new(gate_of(gid, verify, &perms, &channels));
    store_gate(gid, gen, g.clone());
    Ok(g)
}

/// Caches `g` unless a hard forget came while it was read (S R4-1); a soften during the read
/// stores it with the short life (R5). True when stored.
fn store_gate(gid: Id<GuildMarker>, gen: (u64, u64), g: Arc<Gate>) -> bool {
    let mut all = GATES.lock().unwrap_or_else(|p| p.into_inner());
    let slot = all.entry(gid.get()).or_default();
    if slot.gen.0 != gen.0 {
        return false;
    }
    let life = if slot.gen.1 == gen.1 { GATE_FOR } else { GATE_COALESCE };
    slot.gate = Some((g, life));
    true
}

/// Drops the guild's [`Gate`] at once: someone else changed a role or channel (its audit event).
pub(crate) fn forget_gate(gid: Id<GuildMarker>) {
    let mut all = GATES.lock().unwrap_or_else(|p| p.into_inner());
    let slot = all.entry(gid.get()).or_default();
    slot.gate = None;
    slot.gen.0 += 1;
}

/// Roles and channels TommyBot is editing itself (a lock, an unlock, panic), by (guild, id),
/// marked just before each edit.
static OWN_EDITS: LazyLock<Mutex<HashMap<(u64, u64), Instant>>> = LazyLock::new(Default::default);
const OWN_FOR: Duration = Duration::from_secs(5);

/// TommyBot is about to edit this role or channel itself: the gateway event it causes only
/// softens the gate. Scoped to the object, so nobody can open the window for their own edits.
pub(crate) fn mark_own(gid: Id<GuildMarker>, object: u64) {
    let mut all = OWN_EDITS.lock().unwrap_or_else(|p| p.into_inner());
    all.retain(|_, at| at.elapsed() < OWN_FOR);
    all.insert((gid.get(), object), Instant::now());
}

/// Tests: the own-edit marks of `gid` run out now.
#[cfg(test)]
pub(crate) fn age_own_edits(gid: Id<GuildMarker>) {
    let mut all = OWN_EDITS.lock().unwrap_or_else(|p| p.into_inner());
    let old = Instant::now().checked_sub(OWN_FOR * 2).unwrap_or_else(Instant::now);
    all.iter_mut().filter(|((g, _), _)| *g == gid.get()).for_each(|(_, at)| *at = old);
}

/// A gateway role or channel event (from `invalidate_guild`), which names no actor: a hard
/// forget, unless it is about `object` and TommyBot marked that object just now (S R5-1, R6-1).
pub(crate) fn invalidated(gid: Id<GuildMarker>, object: Option<u64>) {
    let own = object.is_some_and(|o| {
        OWN_EDITS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(gid.get(), o))
            .is_some_and(|at| at.elapsed() < OWN_FOR)
    });
    if own {
        soften_gate(gid);
    } else {
        forget_gate(gid);
    }
}

/// Lets the guild's [`Gate`] live at most [`GATE_COALESCE`] more: TommyBot's own edit. A read
/// that crosses it is stored with that short life too.
pub(crate) fn soften_gate(gid: Id<GuildMarker>) {
    let mut all = GATES.lock().unwrap_or_else(|p| p.into_inner());
    let slot = all.entry(gid.get()).or_default();
    slot.gen.1 += 1;
    if let Some((g, fresh_for)) = slot.gate.as_mut() {
        let left = g.at.map_or(Duration::ZERO, |at| at.elapsed()) + GATE_COALESCE;
        *fresh_for = (*fresh_for).min(left);
    }
}

fn forget(map: &Recent, gid: Id<GuildMarker>, user: Id<UserMarker>) {
    map.lock().unwrap_or_else(|p| p.into_inner()).remove(&(gid.get(), user.get()));
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
    roles_changed(e, true)
}

/// The roles an entry added (`added`) or removed.
fn roles_changed(e: &AuditLogEntry, added: bool) -> Vec<Id<RoleMarker>> {
    e.changes
        .iter()
        .filter_map(|c| match c {
            AuditLogChange::RoleAdded { new, .. } if added => Some(new),
            AuditLogChange::RoleRemoved { new, .. } if !added => Some(new),
            _ => None,
        })
        .flatten()
        .map(|r| r.id)
        .collect()
}

pub async fn on_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
    let Some(gid) = e.guild_id else {
        return Ok(());
    };
    match e.action_type {
        A::MemberRoleUpdate => on_roles(app, e).await,
        A::ChannelOverwriteCreate | A::ChannelOverwriteUpdate => {
            changed(app, gid, e);
            on_overwrite(app, e).await
        }
        A::RoleCreate | A::RoleUpdate | A::ChannelCreate | A::ChannelUpdate | A::ChannelOverwriteDelete => {
            changed(app, gid, e);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// A role or channel changed. Another actor's edit drops the gate at once. TommyBot's own: a
/// channel update is its slowmode or a rename (nothing a gate reads, so kept), and a role or
/// overwrite edit (a lock) only shortens the gate's life, so a lock over many channels rebuilds
/// it at most every few seconds.
fn changed(app: &App, gid: Id<GuildMarker>, e: &AuditLogEntry) {
    if !matches!(
        e.action_type,
        A::RoleCreate
            | A::RoleUpdate
            | A::ChannelCreate
            | A::ChannelUpdate
            | A::ChannelOverwriteCreate
            | A::ChannelOverwriteUpdate
            | A::ChannelOverwriteDelete
    ) {
        return;
    }
    match (e.user_id == Some(app.me), e.action_type) {
        (false, _) => forget_gate(gid),
        (true, A::ChannelUpdate) => {}
        (true, _) => soften_gate(gid),
    }
}

/// Runs first in the audit event arm, before anti-nuke, Ban Sync and the Raider List.
pub(crate) fn audit_changed(app: &App, e: &AuditLogEntry) {
    if let Some(gid) = e.guild_id {
        changed(app, gid, e);
    }
}

async fn on_roles(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (Some(gid), Some(actor), Some(target)) = (e.guild_id, e.user_id, e.target_id) else {
        return Ok(());
    };
    let target: Id<UserMarker> = target.cast();
    let added = roles_added(e);
    let removed = roles_changed(e, false);
    if !removed.is_empty() {
        // An un-verify: the member no longer counts as seen verified.
        let cfg = app.cfg(gid.get()).await?;
        if removed.iter().any(|r| cfg.verification.roles.contains(r)) {
            forget(&SEEN_VERIFIED, gid, target);
        }
    }
    if actor == app.me || added.is_empty() {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let v = &cfg.verification;
    if !v.enabled || v.roles.is_empty() {
        return Ok(());
    }
    // Cheap filters first: no REST call for a role that opens nothing verification opens, or
    // for a member found verified already.
    let gate = gate(app, gid, &v.roles, &added).await?;
    if !added.iter().any(|r| gate.roles.contains(r))
        || recent(&SEEN_VERIFIED, gid, target, SEEN_FOR).is_some()
        || hold_takes_it(app, &cfg, gid, target, actor).await?
    {
        return Ok(());
    }
    let bot = match is_bot(app, actor).await {
        Ok(b) => b,
        Err(e) => return failed(app, gid, target, actor, &e).await,
    };
    // A human may verify by hand only with a verify or quarantine permit, or as Owner, EO or TA
    // (Thomas, 13:53 and 13:26): Discord's Manage Roles alone exempts nobody. Like a bot, a
    // human without one cannot give an unverified member a role that opens what verification
    // opens; roles given to verified members are normal staff work (`take_back` leaves them).
    // A failed permit lookup takes the role back: the rule fails closed, and staff are told.
    let mut lookup_failed = None;
    if !bot {
        let level = async {
            let owner = app.guild_info(gid).await?.owner;
            app.level(gid, owner, actor).await
        };
        match level.await {
            Ok(l) if l.has("verify") || l.has("quarantine") => return Ok(()),
            Ok(_) => {}
            Err(e) => lookup_failed = Some(e),
        }
    }
    // Roles given by anyone who may not verify; the name stays from when only bots counted.
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
        Ok(_) if bot => taken_log(app, gid, target, actor, "A verification role given by another bot was taken back").await,
        Ok(_) => {
            let mut log = Log::new("Verification", ORANGE)
                .user("Member", target.get())
                .user("Given by", actor.get())
                .text(&format!("{NO} **A role that skips verification, given without a verify permit, was taken back**"))
                .line("Why", "Only TommyBot's verification, a verify or quarantine permit, or an owner or trusted admin can verify a member.");
            if let Some(e) = &lookup_failed {
                log = log.line(
                    "Note",
                    &format!("Their permits could not be read ({}), so the role was taken back to be safe.", crate::http_err::why(e)),
                );
            }
            if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
                tracing::warn!(?e, "verification role log failed");
            }
            Ok(())
        }
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
    // As for roles: a verification role the bot gives at the same moment proves nothing.
    tokio::time::sleep(SETTLE).await;
    let lock = member_lock(gid, target);
    let _one = lock.lock().await;
    let remove = || async {
        if recent(&VERIFIED, gid, target, RECENT).is_some() {
            return anyhow::Ok(false);
        }
        let Some(m) = crate::http_err::member(&app.http, gid, target).await? else {
            return Ok(false);
        };
        let bot_given = recent(&BOT_GIVEN, gid, target, RECENT).unwrap_or_default();
        if m.roles.iter().any(|r| v.roles.contains(r) && !bot_given.contains(r)) {
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

/// The job that runs [`catch_up`] again after a raid or lock, or after a failed audit read.
pub(crate) const CATCH_UP: &str = "verify_catch_up";
/// Audit pages of 100 that [`catch_up`] reads at most.
const CATCH_UP_PAGES: usize = 5;

/// Grants made while TommyBot was down (GuildCreate): the newest change of each (member, role)
/// in the audit log says who gave it. A member whose verification roles all came from another
/// bot loses the roles that bot gave in [`Gate::roles`]; a role given by TommyBot, by staff or
/// before the log counts as verified, and a pair whose newest change is a removal (TommyBot
/// taking it back live) costs no member read. Waits out a raid or lock like D17.
pub async fn catch_up(app: &App, gid: Id<GuildMarker>, job: Option<&serde_json::Value>) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let v = &cfg.verification;
    if !v.enabled || v.roles.is_empty() {
        return Ok(());
    }
    // One run per guild at a time: GuildCreate and a pending job can meet at a restart.
    let lock = {
        let mut all = CATCH_UP_LOCKS.lock().unwrap_or_else(|p| p.into_inner());
        all.retain(|_, l| Arc::strong_count(l) > 1);
        all.entry(gid.get()).or_default().clone()
    };
    let _one = lock.lock().await;
    let defer = crate::lockdown::Deferred::new(app, gid, CATCH_UP, job).await;
    if defer.waits(app, gid).await {
        return defer.later(app, gid, false).await;
    }
    let read = async {
        let gate = gate(app, gid, &v.roles, &[]).await?;
        let newest = newest_changes(app, gid, &gate).await?;
        anyhow::Ok((gate, newest))
    };
    let (gate, (by, capped)) = match read.await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(?e, "reading the audit log for missed verification roles failed, retrying in a job");
            return defer.later(app, gid, true).await;
        }
    };
    if let Some(oldest) = capped {
        cap_note(app, gid, oldest).await;
    }
    // Whose grants are taken back, looked up once per actor for this run: another bot, or a
    // human without a verify or quarantine permit who is not Owner, EO or TA (as live).
    let mut takers = Takers::new();
    let mut skipped = false;
    let actors: HashSet<_> = by.values().filter(|(a, add)| *add && *a != app.me).map(|(a, _)| *a).collect();
    let owner = app.guild_info(gid).await?.owner;
    for actor in actors {
        let taker = async {
            if is_bot(app, actor).await? {
                return anyhow::Ok(true);
            }
            let level = app.level(gid, owner, actor).await?;
            Ok(!level.has("verify") && !level.has("quarantine"))
        };
        match taker.await {
            Ok(true) => {
                takers.insert(actor);
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(?e, actor = actor.get(), "actor lookup failed, retrying their grants in a job");
                skipped = true;
            }
        }
    }
    let members: HashSet<Id<UserMarker>> = by
        .keys()
        .filter(|(u, r)| from_taker(&by, &takers, *u, r))
        .map(|(u, _)| *u)
        .collect();
    for target in members {
        if let Err(e) = catch_up_member(app, &cfg, &gate, gid, target, &by, &takers).await {
            tracing::warn!(?e, user = target.get(), "checking one member's verification roles failed");
            skipped = true;
        }
    }
    // A skipped actor or member is checked again by the next run.
    if skipped {
        defer.later(app, gid, true).await?;
    }
    Ok(())
}

type GuildLocks = Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>;
static CATCH_UP_LOCKS: LazyLock<GuildLocks> = LazyLock::new(Default::default);
/// Guilds whose first catch-up in this process ran: only that one follows a downtime.
static CAUGHT_UP: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(Default::default);

/// Only a first run after a known downtime whose oldest entry read is newer than its start.
fn cap_worth_telling(first: bool, last_up: i64, oldest_secs: i64) -> bool {
    first && last_up > 0 && oldest_secs > last_up
}

/// The cap was hit. Staff hear of it only when the oldest entry read is newer than the time
/// TommyBot was last up before this process (so grants from the downtime may be missing); a
/// busy server's 45-day history alone, or a later re-identify, goes to tracing.
async fn cap_note(app: &App, gid: Id<GuildMarker>, oldest: u64) {
    let first = CAUGHT_UP.lock().unwrap_or_else(|p| p.into_inner()).insert(gid.get());
    let oldest_secs = ((oldest >> 22) as i64 + 1_420_070_400_000) / 1000;
    let up = crate::metrics::LAST_UP.load(std::sync::atomic::Ordering::Relaxed);
    if !cap_worth_telling(first, up, oldest_secs) {
        tracing::info!(guild = gid.get(), oldest_secs, up, "verification catch-up read its 500-entry cap");
        return;
    }
    let log = Log::new("Verification", ORANGE).text(
        "Checked only the newest 500 role changes for verification roles given while TommyBot was offline; older grants from that time were not checked.",
    );
    if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
        tracing::warn!(?e, "catch-up cap log failed");
    }
}

type Newest = HashMap<(Id<UserMarker>, Id<RoleMarker>), (Id<UserMarker>, bool)>;

/// Actors whose grants [`catch_up`] takes back: other bots, and humans without a permit.
type Takers = HashSet<Id<UserMarker>>;

/// The newest change of this pair is a taker adding the role.
fn from_taker(by: &Newest, takers: &Takers, u: Id<UserMarker>, r: &Id<RoleMarker>) -> bool {
    by.get(&(u, *r)).is_some_and(|(a, add)| *add && takers.contains(a))
}

/// (member, role) -> (who changed it last, whether that was an add), for candidate roles, and
/// the oldest entry id read when the page cap was hit. Entries come newest first.
async fn newest_changes(app: &App, gid: Id<GuildMarker>, gate: &Gate) -> Result<(Newest, Option<u64>)> {
    let mut by = Newest::new();
    let mut before = None;
    // shortcut: reads the newest 500 role changes (logged when hit); read further if outages get long and busy.
    for _ in 0..CATCH_UP_PAGES {
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
            for add in [true, false] {
                for r in roles_changed(e, add).into_iter().filter(|r| gate.roles.contains(r)) {
                    by.entry((t.cast(), r)).or_insert((actor, add));
                }
            }
        }
        match log.entries.last() {
            Some(e) if log.entries.len() == 100 => before = Some(e.id.get()),
            _ => return Ok((by, None)),
        }
    }
    Ok((by, before))
}

async fn catch_up_member(
    app: &App,
    cfg: &GuildCfg,
    gate: &Gate,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    by: &Newest,
    takers: &Takers,
) -> Result<()> {
    let v = &cfg.verification;
    let lock = member_lock(gid, target);
    let _one = lock.lock().await;
    if recent(&VERIFIED, gid, target, RECENT).is_some() {
        return Ok(());
    }
    let Some(member) = crate::http_err::member(&app.http, gid, target).await? else {
        return Ok(());
    };
    let actor = by
        .iter()
        .find(|((u, _), (a, add))| *u == target && *add && takers.contains(a))
        .map(|(_, (a, _))| *a)
        .unwrap_or(app.me);
    if member.roles.iter().any(|r| v.roles.contains(r) && !from_taker(by, takers, target, r))
        || hold_takes_it(app, cfg, gid, target, actor).await?
    {
        return Ok(());
    }
    let taken: Vec<_> = member
        .roles
        .iter()
        .copied()
        .filter(|r| gate.roles.contains(r) && from_taker(by, takers, target, r))
        .collect();
    if taken.is_empty() {
        return Ok(());
    }
    match remove_roles(app, gid, target, &taken).await {
        Ok(()) => taken_log(app, gid, target, actor, "A verification role given while TommyBot was offline, by another bot or without a verify permit, was taken back").await,
        Err(e) => failed(app, gid, target, actor, &e).await,
    }
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

/// The cached gate's freshness, for tests.
#[cfg(test)]
pub(crate) fn cached_for(gid: Id<GuildMarker>) -> Option<Duration> {
    let all = GATES.lock().unwrap_or_else(|p| p.into_inner());
    all.get(&gid.get()).and_then(|s| s.gate.as_ref()).map(|(_, d)| *d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gate_read_across_a_forget_is_not_stored() {
        let gid = Id::new(4_242_424_242);
        let gen = GATES.lock().unwrap().get(&gid.get()).map_or((0, 0), |s| s.gen);
        forget_gate(gid);
        assert!(!store_gate(gid, gen, Arc::new(Gate::default())));
        assert_eq!(cached_for(gid), None);
        let now = || Arc::new(Gate { at: Some(Instant::now()), ..Default::default() });
        let gen = (gen.0 + 1, gen.1);
        assert!(store_gate(gid, gen, now()));
        assert_eq!(cached_for(gid), Some(GATE_FOR));
        // TommyBot's own edit only shortens its life, and a read across it gets the short life.
        soften_gate(gid);
        assert!(cached_for(gid).is_some_and(|d| d <= GATE_COALESCE + Duration::from_secs(1)));
        assert!(store_gate(gid, gen, now()));
        assert_eq!(cached_for(gid), Some(GATE_COALESCE));
    }

    #[test]
    fn the_cap_is_told_only_after_a_downtime_it_may_have_cut() {
        assert!(cap_worth_telling(true, 1_000, 2_000));
        assert!(!cap_worth_telling(true, 1_000, 900), "the 500 reach back before the downtime");
        assert!(!cap_worth_telling(false, 1_000, 2_000), "a later re-identify");
        assert!(!cap_worth_telling(true, 0, 2_000), "downtime unknown");
    }
}

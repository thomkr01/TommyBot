//! Discord-side config. `t!<module>` shows a numbered panel; `VALUE ?set N`, `N ?on` / `N ?off`
//! and `OBJ ?add N` / `OBJ ?remove N` change it. The numbers are public API (people memorise
//! `t!jg 3c`), so never renumber a row: add new ones after the last.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_automod::join::RaidCfg;
use tommybot_core::{duration, permits::Level};
use tommybot_logging::embed::{self, ARROW};
use tommybot_logging::emoji::YES;
use tommybot_logging::emoji::{Emoji, CLOSE, OFF, ON, PAGE_FIRST, PAGE_LAST, PAGE_NEXT, PAGE_PREV};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component},
        AllowedMentions, Embed, MessageFlags,
    },
    gateway::payload::incoming::MessageCreate,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{marker::GuildMarker, Id},
};

use crate::{
    app::{App, GuildCfg},
    targets::{pick, Pick},
};

#[derive(Clone, Copy)]
pub enum Kind {
    Bool,
    Int(i64, i64),
    /// Stored as the typed number times this (0.01 for percentages kept as fractions).
    Num(f64),
    Secs,
    Ms,
    /// Minimum account age in ms: a bare whole number is days, else a duration (`12h`), within `MIN_AGE_MS`.
    Age,
    Text,
    /// Typed word -> stored JSON.
    Choice(&'static [(&'static str, &'static str)]),
    /// Stored JSON for `?on` and `?off`; `?on` keeps a value that is already on.
    Switch(&'static str, &'static str),
    Role,
    Channel,
    Roles,
    Channels,
    /// Header for the lettered rows below it.
    Group,
    Dashboard,
    /// User ids kept in the permits tables, not the config document (Trusted Admins).
    Users,
}

pub struct Row {
    pub code: &'static str,
    pub name: &'static str,
    /// `section.field.sub` in the config document.
    pub path: &'static str,
    pub kind: Kind,
    pub get: fn(&GuildCfg) -> String,
}

pub struct Module {
    pub names: &'static [&'static str],
    pub title: &'static str,
    /// Top-level entries per page; 0 shows everything on one page.
    pub per_page: usize,
    pub footer: &'static str,
    pub rows: &'static [Row],
}

#[rustfmt::skip]
mod maps;
pub use maps::module;
#[cfg(test)]
use maps::*;

#[derive(Debug, PartialEq, Eq)]
pub enum Op {
    Show(usize),
    Set(String, String),
    Toggle(String, bool),
    Add(String, String),
    Remove(String, String),
}

const USAGE: &str =
    "Usage: `VALUE ?set N`, `N ?on`, `N ?off`, `OBJECT ?add N` or `OBJECT ?remove N`";

/// A lone sub-row letter typed apart from its number, or nothing.
fn letter(t: Option<&&str>) -> String {
    t.filter(|t| t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic()))
        .map_or(String::new(), |t| t.to_lowercase())
}

/// Everything after the module name.
pub fn parse_op(args: &str) -> Result<Op, &'static str> {
    let toks: Vec<&str> = args.split_whitespace().collect();
    let Some(at) = toks.iter().position(|t| t.starts_with('?')) else {
        return match toks.as_slice() {
            [] => Ok(Op::Show(0)),
            [p] => p
                .parse::<usize>()
                .map(|p| Op::Show(p.saturating_sub(1)))
                .map_err(|_| USAGE),
            _ => Err(USAGE),
        };
    };
    let flag = toks[at].to_lowercase();
    let value = toks[..at].join(" ");
    match flag.as_str() {
        "?on" | "?off" => {
            // `1 ?off a` names row 1a too.
            let code = format!(
                "{}{}",
                toks[..at].last().ok_or(USAGE)?.to_lowercase(),
                letter(toks.get(at + 1))
            );
            Ok(Op::Toggle(code, flag == "?on"))
        }
        "?set" | "?add" | "?remove" => {
            // `6 ?set 1 b` names row 1b too.
            let code = format!(
                "{}{}",
                toks.get(at + 1).ok_or(USAGE)?.to_lowercase(),
                letter(toks.get(at + 2))
            );
            if value.is_empty() {
                return Err(USAGE);
            }
            Ok(match flag.as_str() {
                "?set" => Op::Set(code, value),
                "?add" => Op::Add(code, value),
                _ => Op::Remove(code, value),
            })
        }
        _ => Err(USAGE),
    }
}

#[derive(Debug, PartialEq)]
pub enum Edit {
    Put(Value),
    Add(Vec<Value>),
    Remove(Vec<Value>),
}

fn literal(s: &str) -> Value {
    serde_json::from_str(s).expect("settings table literal")
}

/// Turns an op on one row into an edit. `ids` are the resolved roles or channels for object rows.
pub fn edit_for(row: &Row, op: &Op, ids: &[u64], current: Option<&Value>) -> Result<Edit, String> {
    let label = format!("[{}] {}", row.code.to_uppercase(), row.name);
    let ids_json = || ids.iter().map(|i| json!(i.to_string())).collect::<Vec<_>>();
    match (row.kind, op) {
        (Kind::Dashboard | Kind::Users, _) => {
            Err(format!("{label} can only be changed on the dashboard"))
        }
        (Kind::Group, _) => Err(format!(
            "{label} has sub-settings, use {}a, {}b...",
            row.code, row.code
        )),
        (Kind::Bool, Op::Toggle(_, on)) => Ok(Edit::Put(json!(on))),
        (Kind::Switch(on, off), Op::Toggle(_, true)) => Ok(Edit::Put(match current {
            Some(v) if !v.is_null() && *v != literal(off) => v.clone(),
            _ => literal(on),
        })),
        (Kind::Switch(_, off), Op::Toggle(_, false)) => Ok(Edit::Put(literal(off))),
        (Kind::Bool | Kind::Switch(..), _) => Err(format!(
            "{label} is a switch: use `{} ?on` or `{} ?off`",
            row.code, row.code
        )),
        (_, Op::Toggle(..)) => Err(format!(
            "{label} is not a switch: use `VALUE ?set {}`",
            row.code
        )),
        (Kind::Roles | Kind::Channels, Op::Set(..) | Op::Add(..)) => Ok(Edit::Add(ids_json())),
        (Kind::Roles | Kind::Channels, Op::Remove(..)) => Ok(Edit::Remove(ids_json())),
        (_, Op::Add(..) | Op::Remove(..)) => Err(format!(
            "{label} is not a list: use `VALUE ?set {}`",
            row.code
        )),
        (kind, Op::Set(_, v)) => {
            let bad = || format!("`{v}` is not a valid value for {label}");
            Ok(Edit::Put(match kind {
                Kind::Int(min, max) => {
                    let n: i64 = v.parse().map_err(|_| bad())?;
                    if !(min..=max).contains(&n) {
                        return Err(format!("{label} must be between {min} and {max}"));
                    }
                    json!(n)
                }
                Kind::Num(scale) => {
                    let n: f64 = v.trim_end_matches('%').parse().map_err(|_| bad())?;
                    if !n.is_finite() || n < 0.0 {
                        return Err(bad());
                    }
                    // Rounded, so `99` at scale 0.01 is 0.99 and not 0.9900000000000001.
                    let round = |x: f64| (x * 1e9).round() / 1e9;
                    let x = round(n * scale);
                    let (section, path) = row.path.split_once('.').unwrap_or_default();
                    if let Some((lo, hi)) = tommybot_core::limits::float_for_path(section, path) {
                        if !(lo..=hi).contains(&x) {
                            let (lo, hi) = (round(lo / scale), round(hi / scale));
                            return Err(format!("{label} must be between {lo} and {hi}"));
                        }
                    }
                    json!(x)
                }
                Kind::Secs | Kind::Ms => {
                    let secs = duration::parse(v).ok_or_else(|| {
                        format!(
                            "{}. Give a number with a unit, like `15d`, `12h` or `30m`.",
                            bad()
                        )
                    })?;
                    let n = if matches!(kind, Kind::Ms) {
                        secs.checked_mul(1000).ok_or_else(bad)?
                    } else {
                        secs
                    };
                    let (section, path) = row.path.split_once('.').unwrap_or_default();
                    if let Some((lo, hi)) = tommybot_core::limits::for_path(section, path) {
                        if !(lo..=hi).contains(&n) {
                            let unit = if matches!(kind, Kind::Ms) { 1000 } else { 1 };
                            let (lo, hi) = (embed::duration(lo / unit), embed::duration(hi / unit));
                            return Err(format!("{label} must be between {lo} and {hi}"));
                        }
                    }
                    json!(n)
                }
                Kind::Age => {
                    let (lo, hi) = tommybot_core::limits::MIN_AGE_MS;
                    let secs = match v.parse::<i64>() {
                        Ok(days) => days.checked_mul(86_400),
                        Err(_) if v.contains(['.', ',']) => {
                            return Err(format!("{}. Give a whole number.", bad()))
                        }
                        Err(_) => duration::parse(v),
                    };
                    let ms = secs.and_then(|s| s.checked_mul(1000)).ok_or_else(|| {
                        format!(
                            "{}. Give days, or a number with a unit, like `12h` or `30m`.",
                            bad()
                        )
                    })?;
                    if !(lo..=hi).contains(&ms) {
                        return Err(format!("{label} ranges from 5 seconds to 100 days"));
                    }
                    json!(ms)
                }
                Kind::Text if !v.is_empty() && v.len() <= 32 => json!(v),
                Kind::Choice(opts) => {
                    let v = v.to_lowercase();
                    let found = opts.iter().find(|(k, _)| *k == v);
                    let Some((_, lit)) = found else {
                        let names: Vec<_> = opts.iter().map(|(k, _)| *k).collect();
                        return Err(format!("{label} takes one of: {}", names.join(", ")));
                    };
                    literal(lit)
                }
                Kind::Role | Kind::Channel => match ids {
                    [one] => json!(one.to_string()),
                    _ => return Err(format!("{label} takes exactly one")),
                },
                _ => return Err(bad()),
            }))
        }
        (_, Op::Show(_)) => Err(USAGE.into()),
    }
}

/// User ids in `<@1> 2` form; anything else is ignored.
fn user_ids(v: &str) -> Vec<i64> {
    v.split_whitespace()
        .filter_map(|t| t.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok())
        .collect()
}

/// `statics 10 ?add/?remove USER`: writes the `trusted_admins` table the dashboard permits page uses.
async fn edit_trusted_admins(
    app: &App,
    gid: Id<GuildMarker>,
    (level_user, level): (u64, &Level),
    op: &Op,
) -> Result<String> {
    let (Op::Set(_, v) | Op::Add(_, v) | Op::Remove(_, v)) = op else {
        return Ok("[10] Trusted Admins: use `USER ?add 10` or `USER ?remove 10`".into());
    };
    if !matches!(level, Level::Owner | Level::ExtraOwner) {
        return Ok("Only the owner and Extra Owners can change Trusted Admins".into());
    }
    // Trusted Admins bypass every system, so adding one needs a fresh 2FA like every protected
    // change. Removing does not: it is how a hacked TA is cut off fast.
    if !matches!(op, Op::Remove(..)) && !fresh_2fa(app, level_user).await? {
        return Ok(needs_2fa(gid.get()));
    }
    let ids = user_ids(v);
    if ids.is_empty() {
        return Ok("Give at least one user mention or ID".into());
    }
    let sql = if matches!(op, Op::Remove(..)) {
        "delete from trusted_admins where guild_id = $1 and user_id = any($2)"
    } else {
        "insert into trusted_admins (guild_id, user_id) select $1, unnest($2::bigint[]) on conflict do nothing"
    };
    // A config version of its own, so a rollback or a stale Permits page save cannot undo it unseen.
    let (mut tx, version, mut doc) = locked(app, gid).await?;
    sqlx::query(sql)
        .bind(gid.get() as i64)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    append(app, tx, gid, version, &mut doc, level_user).await?;
    let now: Vec<i64> = sqlx::query_scalar(
        "select user_id from trusted_admins where guild_id = $1 order by user_id",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    let list: Vec<_> = now.iter().map(|u| format!("<@{u}>")).collect();
    Ok(format!(
        "{YES} [10] Trusted Admins: {}",
        if list.is_empty() {
            "none".into()
        } else {
            list.join(" ")
        }
    ))
}

fn id_of(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

/// A numeric key indexes into a list (`misc.warn_actions.0.points`).
pub fn get_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(doc, |v, k| {
        v.get(k).or_else(|| v.get(k.parse::<usize>().ok()?))
    })
}

/// Writes `e` at `path`, creating objects on the way. Lists compare ids, so `"5"` and `5` are one entry.
pub fn apply(doc: &mut Value, path: &str, e: Edit) {
    // Join raid has no serde defaults for its required fields, so a first write starts from ours.
    if path.starts_with("joinraid.") && doc.get("joinraid").is_none_or(Value::is_null) {
        doc["joinraid"] = serde_json::to_value(RaidCfg::default()).expect("raid defaults");
    }
    // A new first warn action needs both fields: timeout at 3 warns until the user changes them.
    if path.starts_with("misc.warn_actions.0.") && get_path(doc, "misc.warn_actions.0").is_none() {
        doc["misc"]["warn_actions"] = json!([{"points": 3, "action": "timeout"}]);
    }
    let mut v = doc;
    for k in path.split('.') {
        let len = v.as_array().map_or(0, Vec::len);
        if let Some(i) = k.parse::<usize>().ok().filter(|&i| i < len) {
            v = &mut v[i];
            continue;
        }
        if !v.is_object() {
            *v = json!({});
        }
        v = v.as_object_mut().unwrap().entry(k).or_insert(Value::Null);
    }
    match e {
        Edit::Put(x) => *v = x,
        Edit::Add(xs) => {
            if !v.is_array() {
                *v = json!([]);
            }
            let list = v.as_array_mut().unwrap();
            for x in xs {
                if !list.iter().any(|y| id_of(y) == id_of(&x)) {
                    list.push(x);
                }
            }
        }
        Edit::Remove(xs) => {
            if let Some(list) = v.as_array_mut() {
                list.retain(|y| !xs.iter().any(|x| id_of(x) == id_of(y)));
            }
        }
    }
}

/// Trusted Admins may not change Miscellaneous or Anti-Nuke; everyone else needs Dashboard Edit.
pub fn may_edit(level: &Level, section: &str) -> bool {
    match section {
        "misc" | "antinuke" | "panic" | "backups" => {
            matches!(level, Level::Owner | Level::ExtraOwner)
        }
        "whitelist" => level.immune(),
        _ => level.has("dashboard_edit"),
    }
}

const GREY: u32 = 0x2B2D31;

/// Whether `user` confirmed their 2FA code on the dashboard in the last few minutes (its step-up).
pub(crate) async fn fresh_2fa(app: &App, user: u64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM dash_session WHERE user_id = $1 AND step_up_until > now() AND expires_at > now())",
    )
    .bind(user as i64)
    .fetch_one(&app.pool)
    .await?)
}

pub(crate) fn needs_2fa(gid: u64) -> String {
    format!(
        "This change needs 2FA. Open the {}, choose Confirm 2FA in the account menu, then try again within 10 minutes.",
        dashboard_link(gid)
    )
}

pub(crate) fn dashboard_link(gid: u64) -> String {
    match std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| !u.is_empty())
    {
        Some(u) => format!("[Dashboard]({}/#/{gid})", u.trim_end_matches('/')),
        None => "Dashboard".into(),
    }
}

/// Fields as (name, value): top-level rows start one, lettered rows add a `▸ [A] Name: value` line to it.
fn entries(m: &Module, cfg: &GuildCfg, dash: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for row in m.rows {
        let val = match row.kind {
            Kind::Dashboard | Kind::Users => dash.to_string(),
            Kind::Role | Kind::Channel | Kind::Roles | Kind::Channels => (row.get)(cfg),
            _ => format!("*`{}`*", (row.get)(cfg)),
        };
        let letter = row.code.trim_start_matches(|c: char| c.is_ascii_digit());
        match (letter.is_empty(), out.last_mut()) {
            (false, Some((_, lines))) => {
                if !lines.is_empty() {
                    lines.push('\n');
                }
                lines.push_str(&format!(
                    "{ARROW} [{}] {}: {val}",
                    letter.to_uppercase(),
                    row.name
                ));
            }
            _ => {
                let lines = match row.kind {
                    Kind::Group => String::new(),
                    _ => format!("{ARROW} {val}"),
                };
                // The panel shows a module's on/off state as a toggle next to the Status title.
                let toggle = match (row.kind, row.name) {
                    (Kind::Bool, "Status") if (row.get)(cfg) == "Enabled" => format!(" {ON}"),
                    (Kind::Bool, "Status") => format!(" {OFF}"),
                    _ => String::new(),
                };
                out.push((format!("[{}] {}:{toggle}", row.code, row.name), lines));
            }
        }
    }
    out
}

/// One page of a module's panel and the page count.
pub fn panel(m: &Module, cfg: &GuildCfg, gid: u64, page: usize) -> (Embed, usize) {
    let all = entries(m, cfg, &dashboard_link(gid));
    let per = if m.per_page == 0 {
        all.len().max(1)
    } else {
        m.per_page
    };
    let pages = all.len().div_ceil(per).max(1);
    let page = page.min(pages - 1);
    let mut e = embed::Log::new(m.title, GREY);
    for (name, value) in all.into_iter().skip(page * per).take(per) {
        e = e.inline(&name, &value);
    }
    // Footers name commands with the default prefix; show the server's own.
    let f = m.footer.replace("t!", &cfg.misc.prefix);
    let footer = match (pages, f.as_str()) {
        (1, f) => f.to_string(),
        (n, "") => format!("Page: {}/{n}", page + 1),
        (n, f) => format!("{f} · Page: {}/{n}", page + 1),
    };
    if !footer.is_empty() {
        e = e.footer(&footer);
    }
    (e.build(), pages)
}

const PAGE: &str = "pg:";

fn button(icon: Emoji, id: String, style: ButtonStyle, disabled: bool) -> Component {
    Component::Button(icon.on(Button {
        id: None,
        custom_id: Some(id),
        disabled,
        emoji: None,
        label: Some(icon.fallback.into()),
        style,
        url: None,
        sku_id: None,
    }))
}

/// `≪ ‹ › ≫ ✖` row, drawn with the custom page emojis. The custom id carries the invoker, the panel and the target page, so no
/// state is kept; the last field only keeps ids unique when two buttons lead to the same page.
pub fn page_row(invoker: u64, kind: &str, page: usize, pages: usize) -> Component {
    let id = |to: usize, slot: &str| format!("{PAGE}{invoker}:{kind}:{to}:{slot}");
    let last = pages - 1;
    let grey = ButtonStyle::Secondary;
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            button(PAGE_FIRST, id(0, "f"), grey, page == 0),
            button(PAGE_PREV, id(page.saturating_sub(1), "p"), grey, page == 0),
            button(PAGE_NEXT, id((page + 1).min(last), "n"), grey, page >= last),
            button(PAGE_LAST, id(last, "l"), grey, page >= last),
            button(
                CLOSE,
                format!("{PAGE}{invoker}:x"),
                ButtonStyle::Danger,
                false,
            ),
        ],
    })
}

/// Turns a typed object list into ids: mentions and ids as given, names (comma separated when they
/// have spaces) by exact or unique prefix match.
async fn resolve(
    app: &App,
    gid: Id<GuildMarker>,
    roles: bool,
    value: &str,
) -> Result<Result<Vec<u64>, String>> {
    let mut ids = vec![];
    let mut words = vec![];
    for tok in value.split_whitespace() {
        let digits = tok.trim_matches(|c: char| !c.is_ascii_digit());
        match digits.parse::<u64>() {
            Ok(id) if tok.starts_with('<') || tok == digits => ids.push(id),
            _ => words.push(tok),
        }
    }
    let joined = words.join(" ");
    let names: Vec<&str> = if joined.contains(',') {
        joined
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .collect()
    } else {
        words
    };
    if names.is_empty() {
        return Ok(Ok(ids));
    }
    let all: Vec<(u64, String)> = if roles {
        app.http
            .roles(gid)
            .await?
            .models()
            .await?
            .into_iter()
            .map(|r| (r.id.get(), r.name))
            .collect()
    } else {
        app.http
            .guild_channels(gid)
            .await?
            .models()
            .await?
            .into_iter()
            .map(|c| (c.id.get(), c.name.unwrap_or_default()))
            .collect()
    };
    for n in names {
        let q = n.trim_start_matches(['@', '#']).to_lowercase();
        let cands: Vec<(u64, Vec<String>)> = all
            .iter()
            .filter(|(_, name)| name.to_lowercase().starts_with(&q))
            .map(|(id, name)| (*id, vec![name.clone()]))
            .collect();
        match pick(&q, &cands) {
            Pick::One(id) => ids.push(id),
            Pick::None => return Ok(Err(format!("Nothing found for `{n}`"))),
            Pick::Ambiguous => return Ok(Err(format!("`{n}` is ambiguous, use a mention or id"))),
        }
    }
    Ok(Ok(ids))
}

/// Appends a config version the way the dashboard does (same lock, same NOTIFY). Ok(Err) is a refused value.
pub async fn save(
    app: &App,
    gid: Id<GuildMarker>,
    actor: u64,
    sections: &[&str],
    f: impl FnOnce(&mut Value) -> Result<(), String>,
) -> Result<Result<Value, String>> {
    let (tx, version, mut doc) = locked(app, gid).await?;
    if let Err(e) = f(&mut doc) {
        return Ok(Err(e));
    }
    for s in sections.iter().filter(|s| !doc[**s].is_null()) {
        if let Err(e) = crate::app::check_section(s, &doc[*s]) {
            return Ok(Err(format!("Not saved: {e}")));
        }
    }
    let touches = sections
        .iter()
        .any(|s| matches!(*s, "verification" | "lockdown"));
    if let Some(e) = tommybot_automod::join::web_mode_error(&doc).filter(|_| touches) {
        return Ok(Err(format!("Not saved: {e}")));
    }
    append(app, tx, gid, version, &mut doc, actor).await?;
    Ok(Ok(doc))
}

/// A transaction holding the guild's config lock, with the newest version and its config.
pub(crate) async fn locked(
    app: &App,
    gid: Id<GuildMarker>,
) -> Result<(sqlx::Transaction<'static, sqlx::Postgres>, i32, Value)> {
    let guild = gid.get() as i64;
    let mut tx = app.pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(guild)
        .execute(&mut *tx)
        .await?;
    let (version, doc) = sqlx::query_as(
        "select version, config from guild_config where guild_id = $1 order by version desc limit 1",
    )
    .bind(guild)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or((0, json!({})));
    Ok((tx, version, doc))
}

/// Saves `doc` as version `version + 1` with the live permits as its `permits` copy, then commits.
pub(crate) async fn append(
    app: &App,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    gid: Id<GuildMarker>,
    version: i32,
    doc: &mut Value,
    actor: u64,
) -> Result<()> {
    let guild = gid.get() as i64;
    doc["permits"] = tommybot_core::permits::snapshot(&mut tx, guild).await?;
    sqlx::query(
        "insert into guild_config (guild_id, version, config, changed_by) values ($1,$2,$3,$4)",
    )
    .bind(guild)
    .bind(version + 1)
    .bind(&*doc)
    .bind(actor as i64)
    .execute(&mut *tx)
    .await?;
    sqlx::query("select pg_notify('config_changed', $1)")
        .bind(format!("{guild}:{}", version + 1))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    app.forget_cfg(gid.get());
    Ok(())
}

/// One reply; mentions never ping.
pub(crate) async fn send(
    app: &App,
    msg: &MessageCreate,
    text: &str,
    embed: Option<Embed>,
    rows: Vec<Component>,
) -> Result<()> {
    let embeds: Vec<Embed> = embed.into_iter().collect();
    app.http
        .create_message(msg.channel_id)
        .content(text)
        .embeds(&embeds)
        .components(&rows)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

/// The reply to a code the module has no row for, naming the cards or letters that exist.
fn no_such_row(m: &Module, code: &str) -> String {
    let card = code.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let letters: Vec<&str> = m
        .rows
        .iter()
        .filter_map(|r| r.code.strip_prefix(card))
        .filter(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphabetic()))
        .collect();
    if let (Some(first), Some(last)) = (letters.first(), letters.last()) {
        return format!("Card {card} has options {first} to {last}");
    }
    let last = m
        .rows
        .iter()
        .filter_map(|r| r.code.parse::<u32>().ok())
        .max()
        .unwrap_or(1);
    format!("`{code}` is not an option. Options available are [1 to {last}].")
}

/// Handles `t!<module> ...`; false when `rest` is not a settings command.
pub async fn try_run(app: &App, msg: &MessageCreate, rest: &str) -> Result<bool> {
    let Some(gid) = msg.guild_id else {
        return Ok(false);
    };
    let (word, args) = rest
        .trim_start()
        .split_once(char::is_whitespace)
        .unwrap_or((rest.trim(), ""));
    let word = word.to_lowercase();
    if word == "whitelist" {
        crate::whitelist::run(app, msg, args).await?;
        return Ok(true);
    }
    let Some(m) = module(&word) else {
        return Ok(false);
    };
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, msg.author.id).await?;
    if level == Level::None {
        return Ok(true);
    }
    if word == "setup" && matches!(args.trim(), "run" | "auto") {
        if level.immune() {
            crate::setup::run(app, msg, gid).await?;
        } else {
            send(
                app,
                msg,
                "Only the owner, Extra Owners and Trusted Admins can run setup",
                None,
                vec![],
            )
            .await?;
        }
        return Ok(true);
    }
    let op = match parse_op(args) {
        Ok(op) => op,
        Err(e) => {
            send(app, msg, e, None, vec![]).await?;
            return Ok(true);
        }
    };
    let code = match &op {
        Op::Show(page) => {
            let cfg = app.cfg(gid.get()).await?;
            let (mut embed, pages) = panel(m, &cfg, gid.get(), *page);
            if m.names[0] == "bansync" && level.immune() {
                let extra = crate::bansync::panel_text(app, gid).await?;
                let d = embed.description.get_or_insert_with(String::new);
                d.push_str(&format!("\n\n{extra}"));
            }
            let row = (pages > 1).then(|| {
                page_row(
                    msg.author.id.get(),
                    m.names[0],
                    (*page).min(pages - 1),
                    pages,
                )
            });
            send(app, msg, "", Some(embed), row.into_iter().collect()).await?;
            return Ok(true);
        }
        Op::Set(c, _) | Op::Toggle(c, _) | Op::Add(c, _) | Op::Remove(c, _) => c.clone(),
    };
    // A switch typed on a group (`an 5 ?off`) means its first letter (`5a`).
    let code = maps::alias(m, &code);
    let group_switch = format!("{code}a");
    let found = m.rows.iter().find(|r| r.code == code);
    let found = match (found, &op) {
        (Some(r), Op::Toggle(..)) if matches!(r.kind, Kind::Group) => {
            m.rows.iter().find(|r| r.code == group_switch)
        }
        (f, _) => f,
    };
    let Some(row) = found else {
        send(app, msg, &no_such_row(m, code), None, vec![]).await?;
        return Ok(true);
    };
    if matches!(row.kind, Kind::Users) {
        let text = edit_trusted_admins(app, gid, (msg.author.id.get(), &level), &op).await?;
        send(app, msg, &text, None, vec![]).await?;
        return Ok(true);
    }
    let section = row.path.split('.').next().unwrap_or_default();
    if !row.path.is_empty() && !may_edit(&level, section) {
        send(app, msg, "You may not change this setting", None, vec![]).await?;
        return Ok(true);
    }
    if tommybot_core::sections::SENSITIVE.contains(&section) {
        let text = format!(
            "This setting needs 2FA, so change it on the {}",
            dashboard_link(gid.get())
        );
        send(app, msg, &text, None, vec![]).await?;
        return Ok(true);
    }
    let ids = match (row.kind, &op) {
        (
            Kind::Role | Kind::Roles | Kind::Channel | Kind::Channels,
            Op::Set(_, v) | Op::Add(_, v) | Op::Remove(_, v),
        ) => match resolve(app, gid, matches!(row.kind, Kind::Role | Kind::Roles), v).await? {
            Ok(ids) => ids,
            Err(e) => {
                send(app, msg, &e, None, vec![]).await?;
                return Ok(true);
            }
        },
        _ => vec![],
    };
    // Anti-nuke guards the main roles while a monitor is on, so they need its bar then (D67).
    if row.path == "lockdown.main_roles" {
        let an = &app.cfg(gid.get()).await?.antinuke;
        let text = if !(an.monitor_public_roles || an.monitor_channel_perms) {
            None
        } else if !matches!(level, Level::Owner | Level::ExtraOwner) {
            Some("Anti-nuke monitors the main roles, so only the owner and Extra Owners can change them".into())
        } else if !fresh_2fa(app, msg.author.id.get()).await? {
            Some(needs_2fa(gid.get()))
        } else {
            None
        };
        if let Some(text) = text {
            send(app, msg, &text, None, vec![]).await?;
            return Ok(true);
        }
    }
    // The dashboard's rule too: a main role is never @everyone, managed or above the bot.
    if row.path == "lockdown.main_roles" && !matches!(op, Op::Remove(..)) {
        let info = app.guild_info(gid).await?;
        let bad = ids
            .iter()
            .find_map(|&id| Some((id, info.unfit(gid, Id::new_checked(id)?)?)));
        if let Some((id, why)) = bad {
            let text = format!("<@&{id}> cannot be a main role: {why}");
            send(app, msg, &text, None, vec![]).await?;
            return Ok(true);
        }
    }
    let sections = [section];
    let saved = save(app, gid, msg.author.id.get(), &sections, |doc| {
        let e = edit_for(row, &op, &ids, get_path(doc, row.path))?;
        apply(doc, row.path, e);
        Ok(())
    });
    let text = match saved.await? {
        Ok(doc) => {
            let cfg = GuildCfg::parse(&doc);
            format!(
                "{YES} [{}] {}: {}",
                row.code.to_uppercase(),
                row.name,
                (row.get)(&cfg)
            )
        }
        Err(e) => e,
    };
    send(app, msg, &text, None, vec![]).await?;
    Ok(true)
}

/// The page buttons. Returns false for other interactions.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix(PAGE) else {
        return Ok(false);
    };
    let mut it = rest.split(':');
    let invoker = it.next().and_then(|s| s.parse::<u64>().ok());
    let client = app.http.interaction(app.me.cast());
    if invoker != i.author_id().map(|a| a.get()) {
        let r = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(InteractionResponseData {
                content: Some("Only the person who opened this panel can use it.".into()),
                flags: Some(MessageFlags::EPHEMERAL),
                ..Default::default()
            }),
        };
        client.create_response(i.id, &i.token, &r).await?;
        return Ok(true);
    }
    let (Some(kind), Some(gid)) = (it.next(), i.guild_id) else {
        return Ok(true);
    };
    if kind == "x" {
        if let Some(m) = &i.message {
            crate::http_err::best_effort(
                app.http.delete_message(m.channel_id, m.id).await,
                "close panel",
            );
        }
        let r = InteractionResponse {
            kind: InteractionResponseType::DeferredUpdateMessage,
            data: None,
        };
        crate::http_err::best_effort(
            client.create_response(i.id, &i.token, &r).await,
            "ack panel close",
        );
        return Ok(true);
    }
    let page: usize = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let cfg = app.cfg(gid.get()).await?;
    let (embed, pages) = match module(kind) {
        Some(m) => panel(m, &cfg, gid.get(), page),
        None if kind == "cmds" => {
            let owner = app.guild_info(gid).await?.owner;
            let level = app.level(gid, owner, Id::new(invoker.unwrap_or(1))).await?;
            crate::general::commands_page(&level, &cfg.misc.prefix, page)
        }
        None if kind == "about" => crate::general::about_page(page),
        None if kind.starts_with("cases.") => {
            let user = kind["cases.".len()..].parse().unwrap_or_default();
            crate::modview::cases_page(app, gid.get() as i64, user, page).await?
        }
        None if kind == "ginfo" => crate::info::server_page(app, gid, page).await?,
        None if kind == "tkblock" => crate::tickets::blocked_page(&cfg.tickets, page),
        None if kind == "tksnip" => crate::tickets::snippets_page(&cfg.tickets, page),
        None => return Ok(true),
    };
    let r = InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            embeds: Some(vec![embed]),
            components: Some(vec![page_row(
                invoker.unwrap_or_default(),
                kind,
                page.min(pages - 1),
                pages,
            )]),
            ..Default::default()
        }),
    };
    client.create_response(i.id, &i.token, &r).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(m: &'static Module, code: &str) -> &'static Row {
        m.rows.iter().find(|r| r.code == code).unwrap()
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres. Removing a TA from Discord is a
    /// config version holding the live list, so a rollback to the version before shows it.
    #[tokio::test]
    async fn trusted_admin_removal_is_a_config_version() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let app = App::new(
            twilight_http::Client::new(String::new()),
            twilight_http::Client::new(String::new()),
            pool,
            tommybot_automod::discord_api::Api::new(""),
            Id::new(1),
        );
        let g = 9_830_000_000 + std::process::id() as i64;
        let gid = Id::new(g as u64);
        sqlx::query("insert into trusted_admins (guild_id, user_id) values ($1, 55)")
            .bind(g)
            .execute(&app.pool)
            .await
            .unwrap();
        // A stale copy, as left by the unversioned removal before this fix.
        sqlx::query("insert into guild_config (guild_id, version, config) values ($1, 1, $2)")
            .bind(g)
            .bind(json!({"misc": {"prefix": "x!"}, "permits": {"eo": [], "ta": ["55", "66"], "custom": []}}))
            .execute(&app.pool)
            .await
            .unwrap();
        let op = Op::Remove("10".into(), "<@55>".into());
        let r = edit_trusted_admins(&app, gid, (7, &Level::Owner), &op)
            .await
            .unwrap();
        assert!(r.starts_with(&YES.to_string()), "{r}");
        let (v, by, c): (i32, i64, Value) = sqlx::query_as(
            "select version, changed_by, config from guild_config where guild_id = $1 order by version desc limit 1",
        )
        .bind(g)
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!((v, by), (2, 7));
        assert_eq!(c["permits"], json!({"eo": [], "ta": [], "custom": []}));
        assert_eq!(c["misc"]["prefix"], "x!", "the rest of the config is kept");
        for t in ["guild_config", "trusted_admins"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "delete from {t} where guild_id = $1"
            )))
            .bind(g)
            .execute(&app.pool)
            .await
            .unwrap();
        }
    }

    #[test]
    fn prefix_edits_the_first_warn_action() {
        let mut doc = json!({});
        apply(&mut doc, "misc.warn_actions.0.points", Edit::Put(json!(5)));
        assert_eq!(
            doc["misc"]["warn_actions"],
            json!([{"points": 5, "action": "timeout"}])
        );
        doc["misc"]["warn_actions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"points": 9, "action": "ban"}));
        apply(
            &mut doc,
            "misc.warn_actions.0.action",
            Edit::Put(json!("kick")),
        );
        assert_eq!(doc["misc"]["warn_actions"][0]["action"], "kick");
        assert_eq!(doc["misc"]["warn_actions"][1]["points"], 9);
        assert_eq!(
            get_path(&doc, "misc.warn_actions.1.action"),
            Some(&json!("ban"))
        );
        let c = GuildCfg::parse(&doc);
        assert_eq!(c.misc.warn_actions.len(), 2);
    }

    #[test]
    fn reads_user_ids() {
        assert_eq!(user_ids("<@12> 34 junk"), vec![12, 34]);
    }

    #[test]
    fn parses_prefix_syntax() {
        assert_eq!(parse_op(""), Ok(Op::Show(0)));
        assert_eq!(parse_op("2"), Ok(Op::Show(1)));
        assert_eq!(parse_op("3c ?on"), Ok(Op::Toggle("3c".into(), true)));
        assert_eq!(parse_op("1 ?OFF"), Ok(Op::Toggle("1".into(), false)));
        assert_eq!(
            parse_op("15 ?set 3C"),
            Ok(Op::Set("3c".into(), "15".into()))
        );
        assert_eq!(
            parse_op("Muted Role, @x ?add 5"),
            Ok(Op::Add("5".into(), "Muted Role, @x".into()))
        );
        assert_eq!(
            parse_op("<#1> ?remove 8"),
            Ok(Op::Remove("8".into(), "<#1>".into()))
        );
        assert!(parse_op("?set 3").is_err());
        assert_eq!(parse_op("1 ?off a"), Ok(Op::Toggle("1a".into(), false)));
        assert_eq!(parse_op("6 ?set 1 B"), Ok(Op::Set("1b".into(), "6".into())));
        assert!(parse_op("?on").is_err());
        assert!(parse_op("x ?what 3").is_err());
    }

    #[test]
    fn codes_are_unique_and_lettered_rows_have_a_parent() {
        for m in MODULES {
            let mut seen = std::collections::HashSet::new();
            for r in m.rows {
                assert!(seen.insert(r.code), "{} repeats {}", m.title, r.code);
                let lettered = r.code.ends_with(|c: char| c.is_ascii_alphabetic());
                assert!(
                    !lettered
                        || m.rows
                            .iter()
                            .any(|g| r.code.starts_with(g.code) && g.code.len() < r.code.len()),
                    "{} {} has no parent",
                    m.title,
                    r.code
                );
            }
        }
    }

    #[test]
    fn every_writable_row_saves_a_valid_section() {
        for m in MODULES {
            for r in m.rows {
                let op = match r.kind {
                    Kind::Group | Kind::Dashboard | Kind::Users => continue,
                    Kind::Bool | Kind::Switch(..) => Op::Toggle(r.code.into(), true),
                    Kind::Int(min, _) => Op::Set(r.code.into(), min.to_string()),
                    Kind::Num(_) => Op::Set(r.code.into(), "50".into()),
                    // 1m fits every ranged duration row (Voice Raid: span up to 5m, active from 1m).
                    Kind::Secs | Kind::Ms | Kind::Age => Op::Set(r.code.into(), "1m".into()),
                    Kind::Text => Op::Set(r.code.into(), "t!".into()),
                    Kind::Choice(o) => Op::Set(r.code.into(), o[o.len() - 1].0.into()),
                    Kind::Role | Kind::Channel | Kind::Roles | Kind::Channels => {
                        Op::Set(r.code.into(), "<#5>".into())
                    }
                };
                let mut doc = json!({});
                let e = edit_for(r, &op, &[5], None).unwrap_or_else(|e| panic!("{e}"));
                apply(&mut doc, r.path, e);
                let section = r.path.split('.').next().unwrap();
                crate::app::check_section(section, &doc[section])
                    .unwrap_or_else(|e| panic!("{} {}: {e}", m.title, r.code));
                // The panel getter reads the result back without panicking.
                (r.get)(&GuildCfg::parse(&doc));
            }
        }
    }

    #[test]
    fn ranged_durations_are_refused_outside_their_limits() {
        let span = row(&JOINRAID, "10c");
        let set = |v: &str| edit_for(span, &Op::Set("10c".into(), v.into()), &[5], None);
        assert!(set("5m").is_ok());
        assert_eq!(
            set("10m").err().as_deref(),
            Some("[10C] Join History must be between 1s and 5m")
        );
    }

    #[test]
    fn edits_round_trip() {
        let mut doc = json!({});
        let r = row(&JOINGATE, "3c");
        apply(
            &mut doc,
            r.path,
            edit_for(r, &Op::Set("3c".into(), "15d".into()), &[], None).unwrap(),
        );
        assert_eq!(GuildCfg::parse(&doc).joingate.min_age_ms(), 15 * 86_400_000);
        let err = edit_for(r, &Op::Set("3c".into(), "15x".into()), &[], None).unwrap_err();
        assert!(err.contains("`12h`"), "{err}");
        assert!(edit_for(r, &Op::Toggle("3c".into(), true), &[], None).is_err());

        let r = row(&STATICS, "5");
        apply(&mut doc, r.path, Edit::Add(vec![json!("7"), json!("8")]));
        apply(&mut doc, r.path, Edit::Add(vec![json!(7)]));
        apply(&mut doc, r.path, Edit::Remove(vec![json!("8")]));
        assert_eq!(doc["lockdown"]["main_roles"], json!(["7"]));

        // ?on keeps a custom value, ?off clears it.
        let r = row(&JOINRAID, "6a");
        let on =
            |cur: Option<&Value>| edit_for(r, &Op::Toggle("6a".into(), true), &[], cur).unwrap();
        assert_eq!(on(None), Edit::Put(json!(86_400_000)));
        assert_eq!(on(Some(&json!(5))), Edit::Put(json!(5)));
        apply(&mut doc, r.path, on(None));
        assert_eq!(GuildCfg::parse(&doc).joinraid.unwrap().count, 10);

        let r = row(&HEAT, "4");
        apply(
            &mut doc,
            r.path,
            edit_for(r, &Op::Set("4".into(), "8%".into()), &[], None).unwrap(),
        );
        assert_eq!((r.get)(&GuildCfg::parse(&doc)), "8% per second");
    }

    #[test]
    fn caps_and_min_age_keep_to_their_ranges() {
        let set = |m: &'static Module, code: &str, v: &str| {
            edit_for(row(m, code), &Op::Set(code.into(), v.into()), &[], None)
        };
        assert_eq!(
            set(&CAPS, "1c", "501"),
            Err("[1C] Hour Cap must be between 2 and 500".into())
        );
        assert!(set(&CAPS, "1c", "1").is_err());
        assert_eq!(set(&CAPS, "1c", "500"), Ok(Edit::Put(json!(500))));
        assert_eq!(
            set(&CAPS, "1d", "0"),
            Err("[1D] Heat % must be between 1 and 100".into())
        );
        assert!(set(&CAPS, "1d", "101").is_err());
        assert_eq!(set(&CAPS, "1d", "1"), Ok(Edit::Put(json!(1))));
        let age = |v| set(&JOINGATE, "3c", v);
        assert_eq!(age("3"), Ok(Edit::Put(json!(3 * 86_400_000_i64))));
        assert_eq!(age("12h"), Ok(Edit::Put(json!(12 * 3_600_000_i64))));
        assert_eq!(age("5s"), Ok(Edit::Put(json!(5_000))));
        assert_eq!(age("100d"), Ok(Edit::Put(json!(8_640_000_000_i64))));
        let range = Err("[3C] Minimum Age ranges from 5 seconds to 100 days".to_string());
        for bad in ["0", "-1", "101d", "101", "4s"] {
            assert_eq!(age(bad), range, "{bad}");
        }
        assert!(age("1.5").unwrap_err().contains("whole number"));
        assert!(age("soon").is_err());
        // A stored age outside the range reads as the nearest limit.
        let cfg = GuildCfg::parse(&json!({"joingate": {"min_age_override_ms": 0}}));
        assert_eq!((row(&JOINGATE, "3c").get)(&cfg), "5s");
        // Max heat above 0, decay strictly between 0% and 100% per second.
        assert_eq!(
            set(&HEAT, "3", "0"),
            Err("[3] Max Heat Percentage must be between 1 and 100000".into())
        );
        assert_eq!(set(&HEAT, "3", "100"), Ok(Edit::Put(json!(100.0))));
        assert_eq!(
            set(&HEAT, "4", "100"),
            Err("[4] Heat Degradation must be between 0.1 and 99".into())
        );
        assert!(set(&HEAT, "4", "0").is_err());
        assert_eq!(set(&HEAT, "4", "99"), Ok(Edit::Put(json!(0.99))));
        assert_eq!(set(&HEAT, "4", "0.1"), Ok(Edit::Put(json!(0.001))));
    }

    #[test]
    fn unknown_codes_name_the_options() {
        assert_eq!(
            no_such_row(&CAPS, "51b"),
            "`51b` is not an option. Options available are [1 to 7]."
        );
        assert_eq!(
            no_such_row(&CAPS, "8b"),
            "`8b` is not an option. Options available are [1 to 7]."
        );
        assert_eq!(no_such_row(&CAPS, "5z"), "Card 5 has options a to d");
        let last = JOINGATE
            .rows
            .iter()
            .filter_map(|r| r.code.parse::<u32>().ok())
            .max()
            .unwrap();
        assert!(last > 1);
        assert_eq!(
            no_such_row(&JOINGATE, "99"),
            format!("`99` is not an option. Options available are [1 to {last}].")
        );
    }

    #[test]
    fn ancaps_shows_defaults_and_edits_one_card() {
        let mut doc = json!({});
        let cfg = GuildCfg::parse(&doc);
        assert_eq!((row(&CAPS, "1b").get)(&cfg), "5");
        assert_eq!((row(&CAPS, "4d").get)(&cfg), "16%");
        let r = row(&CAPS, "1b");
        apply(
            &mut doc,
            r.path,
            edit_for(r, &Op::Set("1b".into(), "6".into()), &[], None).unwrap(),
        );
        assert!(edit_for(r, &Op::Set("1b".into(), "1".into()), &[], None).is_err());
        let r = row(&CAPS, "1a");
        apply(
            &mut doc,
            r.path,
            edit_for(r, &Op::Toggle("1a".into(), false), &[], None).unwrap(),
        );
        let cfg = GuildCfg::parse(&doc);
        assert_eq!(
            ((row(&CAPS, "1a").get)(&cfg), (row(&CAPS, "1b").get)(&cfg)),
            ("Disabled".into(), "6".into())
        );
        assert!(cfg
            .antinuke
            .limit(tommybot_antinuke::Action::Kick)
            .is_none());
        assert_eq!((row(&CAPS, "1c").get)(&cfg), "15");
    }

    #[test]
    fn panel_layout_and_pages() {
        let cfg = GuildCfg::default();
        let (e, pages) = panel(&HEAT, &cfg, 1, 0);
        assert_eq!(pages, 1);
        let f = &e.fields;
        assert_eq!(
            (f[0].name.as_str(), f[0].value.as_str()),
            ("[1] Status: 🔴", "▸ *`Disabled`*")
        );
        let t = f.iter().find(|f| f.name == "[7] Auto Timeouts:").unwrap();
        assert!(t
            .value
            .starts_with("▸ [A] Status: *`Enabled`*\n▸ [B] Regular Strike Duration: *`1m`*"));
        let (e, pages) = panel(&HFILTERS, &cfg, 1, 1);
        assert_eq!(pages, 4);
        assert_eq!(e.fields[2].name, "[7] Emojis Heat:");
        assert_eq!(e.footer.unwrap().text, "Page: 2/4");
        assert_eq!(
            panel(&HFILTERS, &cfg, 1, 99).0.fields[0].name,
            "[13] Words Blacklist Heat:"
        );
    }

    #[test]
    fn who_may_edit() {
        assert!(!may_edit(&Level::TrustedAdmin, "misc"));
        assert!(may_edit(&Level::ExtraOwner, "antinuke"));
        assert!(may_edit(&Level::TrustedAdmin, "joingate"));
        assert!(!may_edit(&Level::None, "joingate"));
    }
}

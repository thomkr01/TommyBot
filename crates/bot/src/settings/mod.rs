//! Wick's Discord-side config. `w!<module>` shows a numbered panel; `VALUE ?set N`, `N ?on` / `N ?off`
//! and `OBJ ?add N` / `OBJ ?remove N` change it. The numbers are public API (people memorise
//! `w!jg 3c`), so never renumber a row: add new ones after the last.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_automod::join::RaidCfg;
use tommybot_core::{duration, permits::Level};
use tommybot_logging::embed::{self, ARROW};
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
            let code = toks[..at].last().ok_or(USAGE)?.to_lowercase();
            Ok(Op::Toggle(code, flag == "?on"))
        }
        "?set" | "?add" | "?remove" => {
            let code = toks.get(at + 1).ok_or(USAGE)?.to_lowercase();
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
        (Kind::Dashboard, _) => Err(format!("{label} can only be changed on the dashboard")),
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
                    json!(n * scale)
                }
                Kind::Secs => json!(duration::parse(v).ok_or_else(bad)?),
                Kind::Ms => json!(duration::parse(v).ok_or_else(bad)? * 1000),
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

fn id_of(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

pub fn get_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(doc, |v, k| v.get(k))
}

/// Writes `e` at `path`, creating objects on the way. Lists compare ids, so `"5"` and `5` are one entry.
pub fn apply(doc: &mut Value, path: &str, e: Edit) {
    // Join raid has no serde defaults for its required fields, so a first write starts from ours.
    if path.starts_with("joinraid.") && doc.get("joinraid").is_none_or(Value::is_null) {
        doc["joinraid"] = serde_json::to_value(RaidCfg::default()).expect("raid defaults");
    }
    let mut v = doc;
    for k in path.split('.') {
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

/// Wick: Trusted Admins may not change Miscellaneous or Anti-Nuke; everyone else needs Dashboard Edit.
pub fn may_edit(level: &Level, section: &str) -> bool {
    match section {
        "misc" | "antinuke" | "panic" => matches!(level, Level::Owner | Level::ExtraOwner),
        "whitelist" => level.immune(),
        _ => level.has("dashboard_edit"),
    }
}

const GREY: u32 = 0x2B2D31;

fn dashboard_link(gid: u64) -> String {
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
            Kind::Dashboard => dash.to_string(),
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
                out.push((format!("[{}] {}:", row.code, row.name), lines));
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
    let footer = match (pages, m.footer) {
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

fn button(label: &str, id: String, style: ButtonStyle, disabled: bool) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(id),
        disabled,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
        sku_id: None,
    })
}

/// Wick's `≪ ‹ › ≫ ✖` row. The custom id carries the invoker, the panel and the target page, so no
/// state is kept; the last field only keeps ids unique when two buttons lead to the same page.
pub fn page_row(invoker: u64, kind: &str, page: usize, pages: usize) -> Component {
    let id = |to: usize, slot: &str| format!("{PAGE}{invoker}:{kind}:{to}:{slot}");
    let last = pages - 1;
    let grey = ButtonStyle::Secondary;
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            button("≪", id(0, "f"), grey, page == 0),
            button("‹", id(page.saturating_sub(1), "p"), grey, page == 0),
            button("›", id((page + 1).min(last), "n"), grey, page >= last),
            button("≫", id(last, "l"), grey, page >= last),
            button(
                "✖",
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
    let guild = gid.get() as i64;
    let mut tx = app.pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(guild)
        .execute(&mut *tx)
        .await?;
    let (version, mut doc): (i32, Value) = sqlx::query_as(
        "select version, config from guild_config where guild_id = $1 order by version desc limit 1",
    )
    .bind(guild)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or((0, json!({})));
    if let Err(e) = f(&mut doc) {
        return Ok(Err(e));
    }
    for s in sections.iter().filter(|s| !doc[**s].is_null()) {
        if let Err(e) = crate::app::check_section(s, &doc[*s]) {
            return Ok(Err(format!("Not saved: {e}")));
        }
    }
    sqlx::query(
        "insert into guild_config (guild_id, version, config, changed_by) values ($1,$2,$3,$4)",
    )
    .bind(guild)
    .bind(version + 1)
    .bind(&doc)
    .bind(actor as i64)
    .execute(&mut *tx)
    .await?;
    sqlx::query("select pg_notify('config_changed', $1)")
        .bind(guild.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    app.forget_cfg(gid.get());
    Ok(Ok(doc))
}

async fn send(
    app: &App,
    msg: &MessageCreate,
    text: Option<&str>,
    embed: Option<Embed>,
    row: Option<Component>,
) -> Result<()> {
    let embeds: Vec<Embed> = embed.into_iter().collect();
    let rows: Vec<Component> = row.into_iter().collect();
    app.http
        .create_message(msg.channel_id)
        .content(text.unwrap_or_default())
        .embeds(&embeds)
        .components(&rows)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

/// Handles `w!<module> ...`; false when `rest` is not a settings command.
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
    let op = match parse_op(args) {
        Ok(op) => op,
        Err(e) => {
            send(app, msg, Some(e), None, None).await?;
            return Ok(true);
        }
    };
    let code = match &op {
        Op::Show(page) => {
            let cfg = app.cfg(gid.get()).await?;
            let (embed, pages) = panel(m, &cfg, gid.get(), *page);
            let row = (pages > 1).then(|| {
                page_row(
                    msg.author.id.get(),
                    m.names[0],
                    (*page).min(pages - 1),
                    pages,
                )
            });
            send(app, msg, None, Some(embed), row).await?;
            return Ok(true);
        }
        Op::Set(c, _) | Op::Toggle(c, _) | Op::Add(c, _) | Op::Remove(c, _) => c.clone(),
    };
    let Some(row) = m.rows.iter().find(|r| r.code == code) else {
        send(
            app,
            msg,
            Some(&format!(
                "{} has no setting `{code}`",
                m.title.trim_end_matches(':')
            )),
            None,
            None,
        )
        .await?;
        return Ok(true);
    };
    let section = row.path.split('.').next().unwrap_or_default();
    if !row.path.is_empty() && !may_edit(&level, section) {
        send(
            app,
            msg,
            Some("You may not change this setting"),
            None,
            None,
        )
        .await?;
        return Ok(true);
    }
    let ids = match (row.kind, &op) {
        (
            Kind::Role | Kind::Roles | Kind::Channel | Kind::Channels,
            Op::Set(_, v) | Op::Add(_, v) | Op::Remove(_, v),
        ) => match resolve(app, gid, matches!(row.kind, Kind::Role | Kind::Roles), v).await? {
            Ok(ids) => ids,
            Err(e) => {
                send(app, msg, Some(&e), None, None).await?;
                return Ok(true);
            }
        },
        _ => vec![],
    };
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
                "✅ [{}] {}: {}",
                row.code.to_uppercase(),
                row.name,
                (row.get)(&cfg)
            )
        }
        Err(e) => e,
    };
    send(app, msg, Some(&text), None, None).await?;
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
            app.http.delete_message(m.channel_id, m.id).await.ok();
        }
        let r = InteractionResponse {
            kind: InteractionResponseType::DeferredUpdateMessage,
            data: None,
        };
        client.create_response(i.id, &i.token, &r).await.ok();
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

    #[test]
    fn parses_wick_syntax() {
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
                    Kind::Group | Kind::Dashboard => continue,
                    Kind::Bool | Kind::Switch(..) => Op::Toggle(r.code.into(), true),
                    Kind::Int(min, _) => Op::Set(r.code.into(), min.to_string()),
                    Kind::Num(_) => Op::Set(r.code.into(), "50".into()),
                    Kind::Secs | Kind::Ms => Op::Set(r.code.into(), "10m".into()),
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
    fn edits_round_trip() {
        let mut doc = json!({});
        let r = row(&JOINGATE, "3c");
        apply(
            &mut doc,
            r.path,
            edit_for(r, &Op::Set("3c".into(), "15".into()), &[], None).unwrap(),
        );
        assert_eq!(GuildCfg::parse(&doc).joingate.min_age_days, 15);
        assert!(edit_for(r, &Op::Set("3c".into(), "0".into()), &[], None).is_err());
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
    fn panel_layout_and_pages() {
        let cfg = GuildCfg::default();
        let (e, pages) = panel(&HEAT, &cfg, 1, 0);
        assert_eq!(pages, 1);
        let f = &e.fields;
        assert_eq!(
            (f[0].name.as_str(), f[0].value.as_str()),
            ("[1] Status:", "▸ *`Disabled`*")
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

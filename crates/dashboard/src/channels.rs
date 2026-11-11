//! Channel Setup: a per-guild draft of channel changes (create, rename, move, delete, who can see
//! it). Nothing reaches Discord until Push, which checks the result with the security score's
//! rules, takes a backup and then runs the steps.
use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::Sess,
    ops::{self, overwrite_body},
    recovery::live_image,
    store, ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use reqwest::Method;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{LazyLock, Mutex},
};
use tommybot_antinuke::{
    security::{self, private_overwrites, stripped_overwrite, synced, GuildSafety, Setup},
    ChannelImg, OverwriteImg, RoleImg, CATEGORY,
};
use tommybot_core::sections::QUARANTINE_DENY;

const VIEW: u64 = 1 << 10;
const MAX_ITEMS: usize = 500;
/// Roles or strip targets one item may name; Discord allows 100 overwrites per channel.
const MAX_LIST: usize = 100;
/// Text, voice, category, announcement.
const KINDS: [u8; 4] = [0, 2, CATEGORY, 5];

/// The unpushed changes, keyed by a live channel id or `new-<n>` for a channel made in the draft.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    #[serde(default)]
    pub items: BTreeMap<String, Item>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Item {
    /// Only for a new channel: 0 text, 2 voice, 4 category, 5 announcement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `null` = no category; a live id or `new-<n>`. Left out = unchanged.
    #[serde(deserialize_with = "some", skip_serializing_if = "Option::is_none")]
    pub parent: Option<Option<String>>,
    /// Order inside the parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    /// Left out = unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    /// Drop the channel's own overwrites and take its category's.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reset: bool,
    /// Overwrite targets whose dangerous allows are removed (the Fix of an issue).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub strip: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub delete: bool,
}

/// Keeps an explicit `null` apart from a missing field.
fn some<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// Who can see a channel.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Access {
    #[serde(default)]
    pub roles: Vec<String>,
    /// Staff (the Admins and Moderators permit roles) see a limited channel too.
    #[serde(default = "yes")]
    pub staff: bool,
    /// Only `roles` and staff see it. Left out: limited when `roles` is set, so an empty list is
    /// Everyone; `true` with no roles is staff only.
    #[serde(default)]
    pub limited: Option<bool>,
}

fn yes() -> bool {
    true
}

impl Access {
    fn is_limited(&self) -> bool {
        self.limited.unwrap_or(!self.roles.is_empty())
    }
}

/// What the draft rules need besides the channels.
pub struct Ctx {
    pub guild: u64,
    /// The bot's member id, let into limited channels.
    pub bot: u64,
    pub staff: Vec<u64>,
    pub roles: Vec<RoleImg>,
    pub setup: Setup,
    pub bot_pos: i64,
}

impl Ctx {
    /// Roles a user picks: not @everyone, staff, the bot's, quarantine or a main role.
    fn pickable(&self, role: u64) -> bool {
        role != self.guild
            && !self.staff.contains(&role)
            && !self.setup.bot_roles.contains(&role)
            && self.setup.quarantine_role != Some(role)
            && !self.setup.main_roles.contains(&role)
    }
}

/// A channel as it will be after Push, with what changed.
#[derive(Serialize, Clone, Debug)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub kind: u8,
    pub parent: Option<String>,
    pub position: i64,
    pub new: bool,
    pub deleted: bool,
    pub renamed: bool,
    pub moved: bool,
    /// Its overwrites differ from its category's.
    pub exception: bool,
    pub access: Access,
    pub issues: Vec<Issue>,
    /// The desired channel; a new one has a stand-in id.
    #[serde(skip)]
    img: ChannelImg,
    #[serde(skip)]
    changed: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Issue {
    /// The node id.
    pub channel: String,
    pub id: String,
    pub title: String,
    pub detail: String,
    pub fix: Option<DraftFix>,
}

/// The draft edit Fix makes on the issue's channel.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct DraftFix {
    /// Add this target to the item's `strip`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strip: Option<String>,
    /// Set the item's `access` to this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
}

/// Inserts the overwrite when missing, edits it, and drops one the edit leaves empty.
fn edit(
    ows: &mut Vec<OverwriteImg>,
    target: u64,
    is_role: bool,
    f: impl FnOnce(&mut OverwriteImg),
) {
    let at = ows
        .iter()
        .position(|o| o.target == target && o.is_role == is_role);
    let i = at.unwrap_or_else(|| {
        ows.push(OverwriteImg {
            target,
            is_role,
            allow: 0,
            deny: 0,
        });
        ows.len() - 1
    });
    let before = ows[i].clone();
    f(&mut ows[i]);
    if ows[i].allow == 0 && ows[i].deny == 0 && (at.is_none() || ows[i] != before) {
        ows.remove(i);
    }
}

/// `ows` with `a` applied. Only View Channel bits change, and only on @everyone, the main roles,
/// the bot, staff and picked roles: overwrites other features own stay as they are.
fn apply(c: &ChannelImg, ows: Vec<OverwriteImg>, a: &Access, ctx: &Ctx) -> Vec<OverwriteImg> {
    let mut ows = ows;
    let main = &ctx.setup.main_roles;
    if !a.is_limited() {
        edit(&mut ows, ctx.guild, true, |o| {
            o.allow &= !VIEW;
            o.deny &= !VIEW;
        });
        for &r in main {
            edit(&mut ows, r, true, |o| o.deny &= !VIEW);
        }
        return ows;
    }
    let img = ChannelImg {
        overwrites: ows.clone(),
        ..c.clone()
    };
    for o in private_overwrites(&img, ctx.guild, ctx.bot, main) {
        edit(&mut ows, o.target, o.is_role, |x| *x = o.clone());
    }
    let picked: Vec<u64> = a.roles.iter().filter_map(|r| r.parse().ok()).collect();
    let dropped: Vec<u64> = ows
        .iter()
        .filter(|o| o.is_role && o.allow & VIEW != 0 && ctx.pickable(o.target))
        .map(|o| o.target)
        .filter(|t| !picked.contains(t))
        .collect();
    for r in dropped {
        edit(&mut ows, r, true, |o| o.allow &= !VIEW);
    }
    let let_in = |o: &mut OverwriteImg| {
        o.allow |= VIEW;
        o.deny &= !VIEW;
    };
    for &r in &picked {
        edit(&mut ows, r, true, let_in);
    }
    for &r in &ctx.staff {
        if a.staff {
            edit(&mut ows, r, true, let_in);
        } else if !picked.contains(&r) {
            edit(&mut ows, r, true, |o| o.allow &= !VIEW);
        }
    }
    ows
}

/// Who can see a channel with these overwrites.
fn access_of(ows: &[OverwriteImg], ctx: &Ctx) -> Access {
    let ow = |t: u64| ows.iter().find(|o| o.is_role && o.target == t);
    let limited = ow(ctx.guild).is_some_and(|o| o.deny & VIEW != 0);
    let sees = |t: u64| ow(t).is_some_and(|o| o.allow & VIEW != 0);
    Access {
        roles: ows
            .iter()
            .filter(|o| limited && o.is_role && o.allow & VIEW != 0 && ctx.pickable(o.target))
            .map(|o| o.target.to_string())
            .collect(),
        staff: !limited || ctx.staff.iter().all(|&r| sees(r)),
        limited: Some(limited),
    }
}

fn is_new(key: &str) -> bool {
    key.strip_prefix("new-")
        .is_some_and(|n| (1..=9).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Drops edits of channels deleted in Discord since, and moves into categories that are gone.
pub fn prune(draft: &mut Draft, live: &[ChannelImg]) {
    let ids: HashSet<String> = live.iter().map(|c| c.id.to_string()).collect();
    draft.items.retain(|k, _| is_new(k) || ids.contains(k));
    let keys: HashSet<String> = draft.items.keys().cloned().collect();
    for it in draft.items.values_mut() {
        if let Some(Some(p)) = &it.parent {
            if !ids.contains(p) && !keys.contains(p) {
                it.parent = None;
            }
        }
    }
}

/// Discord's rules for a text channel name: lowercase, no spaces.
fn channel_name(name: &str, kind: u8) -> String {
    let name = name.trim();
    if matches!(kind, 0 | 5) {
        name.split_whitespace()
            .collect::<Vec<_>>()
            .join("-")
            .to_lowercase()
    } else {
        name.into()
    }
}

/// Checks a draft from the UI against the live server and normalizes its names.
pub fn validate(
    draft: &mut Draft,
    live: &[ChannelImg],
    roles: &[RoleImg],
    guild: u64,
) -> Result<(), String> {
    if draft.items.len() > MAX_ITEMS {
        return Err(format!("at most {MAX_ITEMS} changes"));
    }
    let by_id: HashMap<String, &ChannelImg> = live.iter().map(|c| (c.id.to_string(), c)).collect();
    let kinds: HashMap<String, u8> = draft
        .items
        .iter()
        .filter_map(|(k, it)| Some((k.clone(), it.kind?)))
        .chain(by_id.iter().map(|(k, c)| (k.clone(), c.kind)))
        .collect();
    for (key, it) in &mut draft.items {
        let live_c = by_id.get(key);
        let kind = match (live_c, it.kind) {
            (Some(_), Some(_)) => return Err(format!("{key}: only a new channel has a kind")),
            (Some(c), None) => c.kind,
            (None, Some(k)) if is_new(key) && KINDS.contains(&k) => k,
            (None, _) => return Err(format!("{key}: not a channel of this server")),
        };
        match &it.name {
            Some(n) => {
                let n = channel_name(n, kind);
                if !(1..=100).contains(&n.chars().count()) {
                    return Err(format!("{key}: a name needs 1-100 characters"));
                }
                it.name = Some(n);
            }
            None if live_c.is_none() => return Err(format!("{key}: a new channel needs a name")),
            None => {}
        }
        if let Some(Some(p)) = &it.parent {
            if kind == CATEGORY {
                return Err(format!("{key}: a category cannot be in a category"));
            }
            if kinds.get(p) != Some(&CATEGORY) {
                return Err(format!("{key}: {p} is not a category"));
            }
        }
        if it.position.is_some_and(|p| !(0..=10_000).contains(&p)) {
            return Err(format!("{key}: position must be 0-10000"));
        }
        let digits =
            |s: &String| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
        if it.strip.len() > MAX_LIST || !it.strip.iter().all(digits) {
            return Err(format!("{key}: strip takes up to {MAX_LIST} ids"));
        }
        if let Some(a) = &it.access {
            let known = |r: &String| {
                r != &guild.to_string() && roles.iter().any(|x| &x.id.to_string() == r)
            };
            if a.roles.len() > MAX_LIST || !a.roles.iter().all(known) {
                return Err(format!(
                    "{key}: access takes up to {MAX_LIST} roles of this server"
                ));
            }
        }
    }
    for (key, it) in &draft.items {
        let cat = kinds.get(key) == Some(&CATEGORY);
        if !(it.delete && cat) {
            continue;
        }
        let kept_child = live
            .iter()
            .map(|c| (c.id.to_string(), c.parent.map(|p| p.to_string())))
            .chain(
                draft
                    .items
                    .keys()
                    .filter(|k| is_new(k))
                    .map(|k| (k.clone(), None)),
            )
            .any(|(k, live_parent)| {
                let item = draft.items.get(&k);
                let parent = item.and_then(|i| i.parent.clone()).unwrap_or(live_parent);
                parent.as_ref() == Some(key) && !item.is_some_and(|i| i.delete)
            });
        if kept_child {
            return Err("move or delete its channels first".into());
        }
    }
    Ok(())
}

/// Channels the draft hides from staff that live shows them, by a channel's own access, a move
/// or reset into such a category, or a new channel in one. Only owners may do that.
pub fn unstaffed(nodes: &[Node], live: &[ChannelImg], ctx: &Ctx) -> HashSet<String> {
    nodes
        .iter()
        .filter(|n| !n.deleted && n.access.is_limited() && !n.access.staff)
        .filter(|n| {
            live.iter()
                .find(|c| c.id.to_string() == n.id)
                .is_none_or(|c| {
                    let now = access_of(&c.overwrites, ctx);
                    !now.is_limited() || now.staff
                })
        })
        .map(|n| n.id.clone())
        .collect()
}

/// The draft applied to the live channels. Category changes reach the channels synced with the
/// category; a channel with its own overwrites (an exception) keeps them.
pub fn desired(live: &[ChannelImg], draft: &Draft, ctx: &Ctx) -> Vec<Node> {
    let none = Item::default();
    let new_keys: Vec<&String> = draft
        .items
        .iter()
        .filter(|(k, it)| is_new(k) && !it.delete)
        .map(|(k, _)| k)
        .collect();
    // New channels get stand-in ids from the top of the range, far from any snowflake.
    let id_of: HashMap<String, u64> = live
        .iter()
        .map(|c| (c.id.to_string(), c.id))
        .chain(
            new_keys
                .iter()
                .enumerate()
                .map(|(i, k)| ((*k).clone(), u64::MAX - i as u64)),
        )
        .collect();
    let next = live.iter().map(|c| c.position).max().unwrap_or(0) + 1;
    let mut rows: Vec<(String, Option<&ChannelImg>, &Item)> = live
        .iter()
        .map(|c| {
            let key = c.id.to_string();
            let it = draft.items.get(&key).unwrap_or(&none);
            (key, Some(c), it)
        })
        .chain(
            new_keys
                .iter()
                .map(|k| ((*k).clone(), None, &draft.items[*k])),
        )
        .collect();
    let quarantine = |mut ows: Vec<OverwriteImg>| {
        if let Some(q) = ctx.setup.quarantine_role {
            if !ows.iter().any(|o| o.is_role && o.target == q) {
                ows.push(OverwriteImg {
                    target: q,
                    is_role: true,
                    allow: 0,
                    deny: QUARANTINE_DENY,
                });
            }
        }
        ows
    };
    // Categories first, so their channels can follow them.
    let kind_of = |c: Option<&ChannelImg>, it: &Item| c.map_or(it.kind.unwrap_or(0), |c| c.kind);
    rows.sort_by_key(|(_, c, it)| kind_of(*c, it) != CATEGORY);
    let mut done: HashMap<String, Vec<OverwriteImg>> = HashMap::new();
    let mut nodes = vec![];
    for (key, c, it) in rows {
        let kind = kind_of(c, it);
        let name = it
            .name
            .clone()
            .or(c.map(|c| c.name.clone()))
            .unwrap_or_default();
        let parent = match (&it.parent, c) {
            (Some(p), _) => p.clone(),
            (None, c) => c.and_then(|c| c.parent).map(|p| p.to_string()),
        };
        let position = it.position.or(c.map(|c| c.position)).unwrap_or(next);
        let parent_ows = parent.as_ref().and_then(|p| done.get(p)).cloned();
        let live_cat = c
            .and_then(|c| c.parent)
            .and_then(|p| live.iter().find(|x| x.id == p));
        let follows = c.is_some_and(|c| live_cat.is_some_and(|cat| synced(c, cat)));
        let base = match (c, parent_ows) {
            (None, p) => quarantine(p.unwrap_or_default()),
            (Some(_), Some(p)) if it.reset || follows => p,
            (Some(c), _) => c.overwrites.clone(),
        };
        let img = ChannelImg {
            id: id_of[&key],
            name: name.clone(),
            kind,
            parent: parent.as_ref().and_then(|p| id_of.get(p).copied()),
            position,
            topic: c.and_then(|c| c.topic.clone()),
            nsfw: c.is_some_and(|c| c.nsfw),
            overwrites: base,
        };
        let mut ows = match &it.access {
            Some(a) => apply(&img, img.overwrites.clone(), a, ctx),
            None => img.overwrites.clone(),
        };
        for o in ows.iter_mut() {
            if it.strip.contains(&o.target.to_string()) {
                *o = stripped_overwrite(o, ctx.guild, &ctx.setup);
            }
        }
        let img = ChannelImg {
            overwrites: ows,
            ..img
        };
        done.insert(key.clone(), img.overwrites.clone());
        let renamed = c.is_some_and(|c| c.name != name);
        let moved =
            c.is_some_and(|c| c.parent.map(|p| p.to_string()) != parent || c.position != position);
        let changed = c.is_none_or(|c| !synced(&img, c)) || it.delete || renamed || moved;
        nodes.push(Node {
            id: key,
            name,
            kind,
            parent,
            position,
            new: c.is_none(),
            deleted: it.delete,
            renamed,
            moved,
            exception: false,
            access: access_of(&img.overwrites, ctx),
            issues: vec![],
            img,
            changed,
        });
    }
    let cats: HashMap<String, ChannelImg> = nodes
        .iter()
        .filter(|n| n.kind == CATEGORY)
        .map(|n| (n.id.clone(), n.img.clone()))
        .collect();
    for n in &mut nodes {
        n.exception = n
            .parent
            .as_ref()
            .and_then(|p| cats.get(p))
            .is_some_and(|cat| !synced(&n.img, cat));
    }
    // The order Discord shows: channels without a category on top, then each category and its channels.
    let pos: HashMap<String, i64> = nodes.iter().map(|n| (n.id.clone(), n.position)).collect();
    nodes.sort_by_key(|n| {
        let group = match (&n.parent, n.kind == CATEGORY) {
            (_, true) => (1, n.position, n.id.clone()),
            (Some(p), _) => (1, pos.get(p).copied().unwrap_or(0), p.clone()),
            (None, _) => (0, 0, String::new()),
        };
        (group, n.kind != CATEGORY, n.position, n.id.clone())
    });
    nodes
}

/// The section of `config` that uses channel `id`, by its page title.
fn used_by(config: &Value, id: &str) -> Option<String> {
    let needle = format!("\"{id}\"");
    let (key, _) = config.as_object()?.iter().find(|(k, v)| {
        // Old role-profile data is ignored since Channel Setup replaced it.
        k.as_str() != "channel_profiles" && v.to_string().contains(&needle)
    })?;
    let title = crate::schema::schema()
        .into_iter()
        .find(|s| s["name"] == key.as_str())
        .and_then(|s| s["title"].as_str().map(String::from));
    Some(title.unwrap_or_else(|| key.clone()))
}

/// What makes the draft unsafe: the security score's channel rows on the desired channels (role
/// overwrites only), plus deleting a channel the config still uses.
pub fn issues(nodes: &[Node], ctx: &Ctx, config: &Value) -> Vec<Issue> {
    let kept: Vec<&Node> = nodes.iter().filter(|n| !n.deleted).collect();
    let key: HashMap<u64, &str> = kept.iter().map(|n| (n.img.id, n.id.as_str())).collect();
    let imgs: Vec<ChannelImg> = kept.iter().map(|n| n.img.clone()).collect();
    // Only the channel rows are kept, so the server settings here do not matter.
    let g = GuildSafety {
        id: ctx.guild,
        mfa: 1,
        verification: 2,
        content_filter: 2,
        notifications: 1,
    };
    let mut out = vec![];
    for row in security::checks(&g, &ctx.roles, &imgs, ctx.bot_pos, &ctx.setup) {
        let (channel, id, fix) = if let Some(rest) = row.id.strip_prefix("overwrite_") {
            let Some((c, t)) = rest.split_once('_') else {
                continue;
            };
            let (Ok(c), Ok(t)) = (c.parse::<u64>(), t.parse::<u64>()) else {
                continue;
            };
            let role = imgs
                .iter()
                .find(|x| x.id == c)
                .is_some_and(|x| x.overwrites.iter().any(|o| o.target == t && o.is_role));
            if !role {
                continue;
            }
            let fix = DraftFix {
                strip: Some(t.to_string()),
                access: None,
            };
            (key[&c], format!("overwrite_{}_{t}", key[&c]), Some(fix))
        } else if let Some(c) = row.id.strip_prefix("log_channel_public_") {
            let Some(k) = c.parse::<u64>().ok().and_then(|c| key.get(&c)) else {
                continue;
            };
            let fix = row.fix.as_ref().map(|_| DraftFix {
                strip: None,
                access: Some(Access {
                    roles: vec![],
                    staff: true,
                    limited: Some(true),
                }),
            });
            (*k, format!("log_channel_public_{k}"), fix)
        } else {
            continue;
        };
        out.push(Issue {
            channel: channel.into(),
            id,
            title: row.title.into(),
            detail: row.detail,
            fix,
        });
    }
    for n in nodes.iter().filter(|n| n.deleted && !n.new) {
        if let Some(by) = used_by(config, &n.id) {
            let what = if n.kind == CATEGORY {
                format!("Category {}", n.name)
            } else {
                format!("#{}", n.name)
            };
            out.push(Issue {
                channel: n.id.clone(),
                id: format!("in_use_{}", n.id),
                title: "Channel is in use".into(),
                detail: format!("{what} is used by {by}; pick another channel first."),
                fix: None,
            });
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub id: u64,
    pub position: i64,
    /// `Some(None)` = out of its category; a live id or a `new-<n>` key.
    pub parent: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Create {
        key: String,
        kind: u8,
        name: String,
        parent: Option<String>,
        position: i64,
        overwrites: Vec<OverwriteImg>,
    },
    Rename {
        id: u64,
        name: String,
    },
    /// One bulk call for every moved channel.
    Positions(Vec<Move>),
    Overwrite {
        channel: u64,
        ow: OverwriteImg,
    },
    DropOverwrite {
        channel: u64,
        target: u64,
    },
    Delete {
        id: u64,
    },
}

/// The Discord calls that turn `live` into `nodes`, in a safe order: categories, channels, names,
/// positions, overwrites, deletes last. Only differences, so a second run resumes a failed one.
pub fn plan(live: &[ChannelImg], nodes: &[Node]) -> Vec<Step> {
    let by_id: HashMap<String, &ChannelImg> = live.iter().map(|c| (c.id.to_string(), c)).collect();
    let mut steps = vec![];
    let mut fresh: Vec<&Node> = nodes.iter().filter(|n| n.new && !n.deleted).collect();
    fresh.sort_by_key(|n| n.kind != CATEGORY);
    steps.extend(fresh.iter().map(|n| Step::Create {
        key: n.id.clone(),
        kind: n.kind,
        name: n.name.clone(),
        parent: n.parent.clone(),
        position: n.position,
        overwrites: n.img.overwrites.clone(),
    }));
    let kept: Vec<(&Node, &ChannelImg)> = nodes
        .iter()
        .filter(|n| !n.new && !n.deleted)
        .filter_map(|n| Some((n, *by_id.get(&n.id)?)))
        .collect();
    for (n, c) in &kept {
        if n.renamed {
            steps.push(Step::Rename {
                id: c.id,
                name: n.name.clone(),
            });
        }
    }
    let moves: Vec<Move> = kept
        .iter()
        .filter(|(n, _)| n.moved)
        .map(|(n, c)| Move {
            id: c.id,
            position: n.position,
            parent: (c.parent.map(|p| p.to_string()) != n.parent).then(|| n.parent.clone()),
        })
        .collect();
    if !moves.is_empty() {
        steps.push(Step::Positions(moves));
    }
    for (n, c) in &kept {
        let same =
            |a: &OverwriteImg, b: &OverwriteImg| a.target == b.target && a.is_role == b.is_role;
        for o in &n.img.overwrites {
            if !c.overwrites.contains(o) {
                steps.push(Step::Overwrite {
                    channel: c.id,
                    ow: o.clone(),
                });
            }
        }
        for o in &c.overwrites {
            if !n.img.overwrites.iter().any(|x| same(x, o)) {
                steps.push(Step::DropOverwrite {
                    channel: c.id,
                    target: o.target,
                });
            }
        }
    }
    let mut gone: Vec<&Node> = nodes.iter().filter(|n| n.deleted && !n.new).collect();
    gone.sort_by_key(|n| n.kind == CATEGORY);
    steps.extend(
        gone.iter()
            .filter_map(|n| by_id.get(&n.id))
            .map(|c| Step::Delete { id: c.id }),
    );
    steps
}

/// After `key` was created as `id`: the item becomes a live one, and channels moving into it point at it.
fn remap(draft: &mut Draft, key: &str, id: &str) {
    if let Some(mut it) = draft.items.remove(key) {
        it.kind = None;
        draft.items.insert(id.into(), it);
    }
    for it in draft.items.values_mut() {
        if it
            .parent
            .as_ref()
            .is_some_and(|p| p.as_deref() == Some(key))
        {
            it.parent = Some(Some(id.into()));
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/guilds/{guild}/channel-setup",
            get(get_draft).put(put_draft).delete(discard),
        )
        .route("/api/guilds/{guild}/channel-setup/push", post(push))
}

/// The live server as the draft rules see it.
struct World {
    config: Value,
    ctx: Ctx,
    channels: Vec<ChannelImg>,
    /// Raw Discord JSON for the backup Push takes first.
    raw: (Value, Value),
}

async fn world(st: &AppState, guild: i64) -> Result<World, ApiError> {
    let (_, config) = store::current(&st.db, guild).await?;
    let l = ops::live(st, guild, &config).await?;
    let staff = tommybot_core::permits::staff_roles(&st.db, guild)
        .await?
        .into_iter()
        .map(|r| r as u64)
        .collect();
    Ok(World {
        config,
        ctx: Ctx {
            guild: guild as u64,
            bot: st.discord.client_id.parse().unwrap_or_default(),
            staff,
            roles: l.image.roles,
            setup: l.setup,
            bot_pos: l.bot_pos,
        },
        channels: l.image.channels,
        raw: (l.roles, l.channels),
    })
}

async fn load(st: &AppState, guild: i64) -> Result<Draft, ApiError> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT draft FROM channel_drafts WHERE guild_id = $1")
            .bind(guild)
            .fetch_optional(&st.db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

/// Stores the draft; an empty one is deleted.
async fn save(st: &AppState, guild: i64, draft: &Draft, user: i64) -> Result<(), ApiError> {
    if draft.items.is_empty() {
        sqlx::query("DELETE FROM channel_drafts WHERE guild_id = $1")
            .bind(guild)
            .execute(&st.db)
            .await?;
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO channel_drafts (guild_id, draft, updated_by) VALUES ($1, $2, $3)
         ON CONFLICT (guild_id) DO UPDATE SET draft = $2, updated_by = $3, updated_at = now()",
    )
    .bind(guild)
    .bind(sqlx::types::Json(draft))
    .bind(user)
    .execute(&st.db)
    .await?;
    Ok(())
}

/// The tree with its issues, as every route answers.
fn view(w: &World, draft: &Draft, level: Level) -> (Vec<Node>, Vec<Issue>, Value) {
    let mut tree = desired(&w.channels, draft, &w.ctx);
    let issues = issues(&tree, &w.ctx, &w.config);
    for n in &mut tree {
        n.issues = issues
            .iter()
            .filter(|i| i.channel == n.id)
            .cloned()
            .collect();
    }
    let reply = json!({
        "draft": draft,
        "tree": tree,
        "issues": issues,
        "changes": tree.iter().filter(|n| n.changed).count(),
        "staff_roles": w.ctx.staff.iter().map(u64::to_string).collect::<Vec<_>>(),
        "can_unstaff": level >= Level::Owner,
    });
    (tree, issues, reply)
}

async fn get_draft(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    let level = require(&st, &s, guild, Level::Viewer, false).await?;
    let w = world(&st, guild).await?;
    let mut draft = load(&st, guild).await?;
    prune(&mut draft, &w.channels);
    Ok(Json(view(&w, &draft, level).2))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBody {
    draft: Draft,
}

async fn put_draft(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Value>,
) -> Res {
    // Access first, so someone without it learns nothing about the draft's shape.
    let level = require(&st, &s, guild, Level::Editor, false).await?;
    let mut draft = serde_json::from_value::<PutBody>(b)
        .map_err(|e| ApiError::BadRequest(format!("invalid draft: {e}")))?
        .draft;
    let w = world(&st, guild).await?;
    prune(&mut draft, &w.channels);
    validate(&mut draft, &w.channels, &w.ctx.roles, w.ctx.guild).map_err(ApiError::BadRequest)?;
    // What an owner already put in the draft may stay while someone else edits the rest.
    let hidden = |d: &Draft| unstaffed(&desired(&w.channels, d, &w.ctx), &w.channels, &w.ctx);
    if level < Level::Owner {
        let mut before = load(&st, guild).await?;
        prune(&mut before, &w.channels);
        if !hidden(&draft).is_subset(&hidden(&before)) {
            return Err(ApiError::Forbidden(
                "only owners can hide a channel from staff",
            ));
        }
    }
    save(&st, guild, &draft, s.user_id).await?;
    Ok(Json(view(&w, &draft, level).2))
}

async fn discard(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    let level = require(&st, &s, guild, Level::Editor, false).await?;
    save(&st, guild, &Draft::default(), s.user_id).await?;
    let w = world(&st, guild).await?;
    Ok(Json(view(&w, &Draft::default(), level).2))
}

/// Guilds with a push running, so a double click cannot create every channel twice.
static PUSHING: LazyLock<Mutex<HashSet<i64>>> = LazyLock::new(Default::default);

struct Pushing(i64);

impl Drop for Pushing {
    fn drop(&mut self) {
        PUSHING.lock().unwrap().remove(&self.0);
    }
}

async fn push(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
) -> Result<Response, ApiError> {
    let level = require(&st, &s, guild, Level::Admin, true).await?;
    if !PUSHING.lock().unwrap().insert(guild) {
        return Err(ApiError::BadRequest("a push is already running".into()));
    }
    let _guard = Pushing(guild);
    let w = world(&st, guild).await?;
    let mut draft = load(&st, guild).await?;
    prune(&mut draft, &w.channels);
    validate(&mut draft, &w.channels, &w.ctx.roles, w.ctx.guild).map_err(ApiError::BadRequest)?;
    let (tree, issues, _) = view(&w, &draft, level);
    if level < Level::Owner && !unstaffed(&tree, &w.channels, &w.ctx).is_empty() {
        return Err(ApiError::Forbidden(
            "only owners can hide a channel from staff",
        ));
    }
    if !issues.is_empty() {
        let body = json!({"error": "the draft is not safe yet", "issues": issues});
        return Ok((StatusCode::CONFLICT, Json(body)).into_response());
    }
    let steps = plan(&w.channels, &tree);
    if steps.is_empty() {
        save(&st, guild, &Draft::default(), s.user_id).await?;
        return Ok(Json(json!({"done": 0, "failed": [], "backup": null})).into_response());
    }
    let backup: i64 =
        sqlx::query_scalar("INSERT INTO backups (guild_id, image) VALUES ($1, $2) RETURNING id")
            .bind(guild)
            .bind(sqlx::types::Json(live_image(guild, &w.raw.0, &w.raw.1)))
            .fetch_one(&st.db)
            .await?;
    let (mut done, mut failed) = (0, vec![]);
    let mut made: HashMap<String, String> = HashMap::new();
    for step in &steps {
        match run(&st, guild, step, &made).await {
            Ok(Some((key, id))) => {
                remap(&mut draft, &key, &id);
                made.insert(key, id);
                save(&st, guild, &draft, s.user_id).await?;
                done += 1;
            }
            Ok(None) => done += 1,
            Err(e) => failed.push(format!("{}: {e:#}", what(step))),
        }
    }
    // A failed step keeps the rest of the draft (with the created ids), so Push again resumes.
    if failed.is_empty() {
        draft = Draft::default();
    }
    save(&st, guild, &draft, s.user_id).await?;
    log_change(
        &st,
        guild,
        &w.config,
        format!(
            "{} (<@{}>) pushed the channel setup: {done} change(s), {} failed, backup #{backup}",
            s.username,
            s.user_id,
            failed.len()
        ),
    )
    .await;
    Ok(Json(json!({"done": done, "failed": failed, "backup": backup})).into_response())
}

fn what(step: &Step) -> String {
    match step {
        Step::Create { name, .. } => format!("create {name}"),
        Step::Rename { id, name } => format!("rename <#{id}> to {name}"),
        Step::Positions(m) => format!("move {} channel(s)", m.len()),
        Step::Overwrite { channel, ow } => format!("permissions of {} on <#{channel}>", ow.target),
        Step::DropOverwrite { channel, target } => {
            format!("remove permissions of {target} on <#{channel}>")
        }
        Step::Delete { id } => format!("delete <#{id}>"),
    }
}

/// One step in Discord; a create returns (draft key, new id).
async fn run(
    st: &AppState,
    guild: i64,
    step: &Step,
    made: &HashMap<String, String>,
) -> anyhow::Result<Option<(String, String)>> {
    let resolve = |k: &str| -> anyhow::Result<String> {
        if is_new(k) {
            made.get(k)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("its category was not created"))
        } else {
            Ok(k.into())
        }
    };
    let call = |m: Method, path: String, body: Option<Value>| async move {
        st.discord.call(m, &path, body).await
    };
    match step {
        Step::Create {
            key,
            kind,
            name,
            parent,
            position,
            overwrites,
        } => {
            let parent = parent.as_deref().map(resolve).transpose()?;
            let ows: Vec<Value> = overwrites
                .iter()
                .map(|o| {
                    let mut b = overwrite_body(o);
                    b["id"] = json!(o.target.to_string());
                    b
                })
                .collect();
            let r = call(
                Method::POST,
                format!("/guilds/{guild}/channels"),
                Some(json!({"name": name, "type": kind, "parent_id": parent, "position": position, "permission_overwrites": ows})),
            )
            .await?;
            let id = r["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("discord returned no id"))?;
            return Ok(Some((key.clone(), id.into())));
        }
        Step::Rename { id, name } => {
            call(
                Method::PATCH,
                format!("/channels/{id}"),
                Some(json!({"name": name})),
            )
            .await?;
        }
        Step::Positions(moves) => {
            let mut body = vec![];
            for m in moves {
                let mut e = json!({"id": m.id.to_string(), "position": m.position});
                if let Some(p) = &m.parent {
                    e["parent_id"] = json!(p.as_deref().map(resolve).transpose()?);
                }
                body.push(e);
            }
            call(
                Method::PATCH,
                format!("/guilds/{guild}/channels"),
                Some(json!(body)),
            )
            .await?;
        }
        Step::Overwrite { channel, ow } => {
            call(
                Method::PUT,
                format!("/channels/{channel}/permissions/{}", ow.target),
                Some(overwrite_body(ow)),
            )
            .await?;
        }
        Step::DropOverwrite { channel, target } => {
            call(
                Method::DELETE,
                format!("/channels/{channel}/permissions/{target}"),
                None,
            )
            .await?;
        }
        Step::Delete { id } => {
            call(Method::DELETE, format!("/channels/{id}"), None).await?;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1;
    const STAFF: u64 = 50;
    const MAIN: u64 = 99;
    const Q: u64 = 60;
    const MANAGE_CHANNELS: u64 = 0x10;

    fn role(id: u64, permissions: u64) -> RoleImg {
        RoleImg {
            id,
            name: format!("r{id}"),
            color: 0,
            permissions,
            position: 1,
            hoist: false,
            mentionable: false,
            managed: false,
        }
    }

    fn ow(target: u64, allow: u64, deny: u64) -> OverwriteImg {
        OverwriteImg {
            target,
            is_role: true,
            allow,
            deny,
        }
    }

    fn ch(id: u64, kind: u8, parent: Option<u64>, overwrites: Vec<OverwriteImg>) -> ChannelImg {
        ChannelImg {
            id,
            name: format!("c{id}"),
            kind,
            parent,
            position: id as i64,
            topic: None,
            nsfw: false,
            overwrites,
        }
    }

    fn ctx() -> Ctx {
        Ctx {
            guild: G,
            bot: 7,
            staff: vec![STAFF],
            roles: [G, 5, 6, STAFF, MAIN, Q]
                .map(|r| role(r, if r == G { VIEW } else { 0 }))
                .to_vec(),
            setup: Setup {
                quarantine_role: Some(Q),
                log_fallback: true,
                roleless_staff: vec![],
                whitelisted: true,
                bot_roles: vec![],
                mod_log: true,
                modules: vec![],
                main_roles: vec![MAIN],
                trusted: vec![],
                log_channels: vec![13],
            },
            bot_pos: 10,
        }
    }

    /// 10 a category with synced 11 and exception 12; 20 a category with the quarantine deny;
    /// 13 a public log channel.
    fn live() -> Vec<ChannelImg> {
        vec![
            ch(10, CATEGORY, None, vec![]),
            ch(11, 0, Some(10), vec![]),
            ch(12, 0, Some(10), vec![ow(5, VIEW, 0)]),
            ch(20, CATEGORY, None, vec![ow(Q, 0, QUARANTINE_DENY)]),
            ch(13, 0, None, vec![]),
        ]
    }

    fn draft(v: Value) -> Draft {
        serde_json::from_value(json!({ "items": v })).unwrap()
    }

    fn node<'a>(nodes: &'a [Node], id: &str) -> &'a Node {
        nodes.iter().find(|n| n.id == id).unwrap()
    }

    #[test]
    fn draft_json_keeps_null_parent_apart_and_refuses_unknown_fields() {
        let d = draft(json!({"11": {"parent": null}, "12": {"name": "x"}}));
        assert_eq!(d.items["11"].parent, Some(None));
        assert_eq!(d.items["12"].parent, None);
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({"items": {"11": {"parent": null}, "12": {"name": "x"}}})
        );
        assert!(serde_json::from_value::<Draft>(json!({"items": {"11": {"perms": 8}}})).is_err());
    }

    #[test]
    fn category_change_skips_exceptions_and_reset_follows_again() {
        let (live, ctx) = (live(), ctx());
        let d = draft(json!({"10": {"access": {"roles": ["6"]}}}));
        let n = desired(&live, &d, &ctx);
        let cat = node(&n, "10");
        assert_eq!(
            (
                cat.access.is_limited(),
                cat.access.roles.clone(),
                cat.access.staff
            ),
            (true, vec!["6".to_string()], true)
        );
        assert_eq!(node(&n, "11").img.overwrites, cat.img.overwrites);
        assert!(!node(&n, "11").exception);
        let ex = node(&n, "12");
        assert!(ex.exception && !ex.access.is_limited());
        assert_eq!(ex.img.overwrites, live[2].overwrites);
        // Reset to category: the exception takes the category's overwrites.
        let d = draft(json!({"10": {"access": {"roles": ["6"]}}, "12": {"reset": true}}));
        let n = desired(&live, &d, &ctx);
        assert_eq!(node(&n, "12").img.overwrites, node(&n, "10").img.overwrites);
        assert!(!node(&n, "12").exception);
        // Back to Everyone: View is no longer denied, other bits and targets stay.
        let mut locked = live.clone();
        locked[0].overwrites = vec![ow(G, 0, VIEW | 0x800), ow(MAIN, 0, VIEW), ow(6, VIEW, 0)];
        let n = desired(
            &locked,
            &draft(json!({"10": {"access": {"roles": []}}})),
            &ctx,
        );
        assert_eq!(
            node(&n, "10").img.overwrites,
            vec![ow(G, 0, 0x800), ow(6, VIEW, 0)]
        );
    }

    #[test]
    fn new_channel_inherits_and_gets_the_quarantine_deny() {
        let (live, ctx) = (live(), ctx());
        let mut d = draft(json!({
            "new-1": {"kind": 0, "name": "My  Chat", "parent": "10"},
            "new-2": {"kind": 0, "name": "x", "parent": "20"},
            "new-3": {"kind": 2, "name": "Voice Room"},
        }));
        validate(&mut d, &live, &ctx.roles, G).unwrap();
        assert_eq!(d.items["new-1"].name.as_deref(), Some("my-chat"));
        assert_eq!(d.items["new-3"].name.as_deref(), Some("Voice Room"));
        let n = desired(&live, &d, &ctx);
        let qdeny = ow(Q, 0, QUARANTINE_DENY);
        assert_eq!(node(&n, "new-1").img.overwrites, vec![qdeny.clone()]);
        assert!(node(&n, "new-1").exception, "10 lacks the quarantine deny");
        assert!(!node(&n, "new-2").exception && node(&n, "new-2").new);
        assert_eq!(node(&n, "new-3").img.overwrites, vec![qdeny]);
    }

    #[test]
    fn a_synced_channel_follows_its_new_category_an_exception_keeps_its_own() {
        let (live, ctx) = (live(), ctx());
        let d = draft(json!({"11": {"parent": "20"}, "12": {"parent": "20"}}));
        let n = desired(&live, &d, &ctx);
        assert_eq!(node(&n, "11").img.overwrites, live[3].overwrites);
        assert!(node(&n, "11").moved && !node(&n, "11").exception);
        assert_eq!(node(&n, "12").img.overwrites, live[2].overwrites);
        assert!(node(&n, "12").exception);
    }

    #[test]
    fn staff_off_needs_an_owner_unless_it_is_off_already() {
        let (mut live, ctx) = (live(), ctx());
        let hidden = |d: Value, live: &[ChannelImg]| {
            let mut v: Vec<String> = unstaffed(&desired(live, &draft(d), &ctx), live, &ctx)
                .into_iter()
                .collect();
            v.sort();
            v
        };
        let off = json!({"access": {"roles": [], "limited": true, "staff": false}});
        // The category and its synced channel 11; exception 12 keeps staff.
        assert_eq!(hidden(json!({"10": off.clone()}), &live), ["10", "11"]);
        let new = json!({"kind": 0, "name": "x", "access": off["access"]});
        assert_eq!(hidden(json!({"new-1": new}), &live), ["new-1"]);
        let on = json!({"access": {"roles": ["6"], "staff": true}});
        assert!(hidden(json!({"10": on}), &live).is_empty());
        // Already hidden from staff in Discord: no owner needed, but moving 13 into it is.
        live[0].overwrites = vec![ow(G, 0, VIEW)];
        live[1].overwrites = live[0].overwrites.clone();
        assert!(hidden(json!({"10": off}), &live).is_empty());
        assert_eq!(
            hidden(json!({"13": {"parent": "10", "reset": true}}), &live),
            ["13"]
        );
    }

    #[test]
    fn deleting_a_category_with_channels_or_a_used_channel_is_refused() {
        let (live, ctx) = (live(), ctx());
        let mut d = draft(json!({"10": {"delete": true}}));
        let e = validate(&mut d, &live, &ctx.roles, G).unwrap_err();
        assert_eq!(e, "move or delete its channels first");
        let mut d =
            draft(json!({"10": {"delete": true}, "11": {"delete": true}, "12": {"parent": null}}));
        validate(&mut d, &live, &ctx.roles, G).unwrap();
        let mut d = draft(json!({"new-1": {"kind": 0, "name": "x", "parent": "11"}}));
        assert!(
            validate(&mut d, &live, &ctx.roles, G).is_err(),
            "11 is no category"
        );
        let mut d = draft(json!({"11": {"access": {"roles": ["999"]}}}));
        assert!(
            validate(&mut d, &live, &ctx.roles, G).is_err(),
            "not a role here"
        );
        let mut d = draft(json!({"11": {"kind": 0}}));
        assert!(
            validate(&mut d, &live, &ctx.roles, G).is_err(),
            "kind is for new ones"
        );
        // The config's log channel cannot go.
        let config = json!({"logging": {"fallback": "11"}});
        let n = desired(&live, &draft(json!({"11": {"delete": true}})), &ctx);
        let i = issues(&n, &ctx, &config);
        let used = i.iter().find(|i| i.channel == "11").unwrap();
        assert_eq!(
            used.detail,
            "#c11 is used by Logging; pick another channel first."
        );
        assert!(used.fix.is_none());
    }

    #[test]
    fn a_public_log_channel_is_an_issue_until_fixed() {
        let (live, ctx) = (live(), ctx());
        let n = desired(&live, &Draft::default(), &ctx);
        let i = issues(&n, &ctx, &json!({}));
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].id, "log_channel_public_13");
        let fix = i[0].fix.clone().unwrap().access.unwrap();
        assert_eq!(
            fix,
            Access {
                roles: vec![],
                staff: true,
                limited: Some(true)
            }
        );
        let item = Item {
            access: Some(fix),
            ..Default::default()
        };
        let d = Draft {
            items: [("13".to_string(), item)].into(),
        };
        let n = desired(&live, &d, &ctx);
        assert!(issues(&n, &ctx, &json!({})).is_empty());
        let a = &node(&n, "13").access;
        assert!(a.is_limited() && a.staff && a.roles.is_empty());
    }

    #[test]
    fn a_dangerous_role_overwrite_is_an_issue_a_member_one_is_not() {
        let (mut live, mut ctx) = (live(), ctx());
        ctx.setup.log_channels.clear();
        live.push(ch(14, 0, None, vec![ow(5, MANAGE_CHANNELS | VIEW, 0)]));
        let member = OverwriteImg {
            is_role: false,
            ..ow(8, MANAGE_CHANNELS, 0)
        };
        live.push(ch(15, 0, None, vec![member]));
        let n = desired(&live, &Draft::default(), &ctx);
        let i = issues(&n, &ctx, &json!({}));
        assert_eq!(i.len(), 1);
        assert_eq!(
            (i[0].id.as_str(), i[0].channel.as_str()),
            ("overwrite_14_5", "14")
        );
        assert_eq!(i[0].fix.as_ref().unwrap().strip.as_deref(), Some("5"));
        let n = desired(&live, &draft(json!({"14": {"strip": ["5"]}})), &ctx);
        assert!(issues(&n, &ctx, &json!({})).is_empty());
        assert_eq!(node(&n, "14").img.overwrites, vec![ow(5, VIEW, 0)]);
    }

    #[test]
    fn plan_runs_in_order_and_is_empty_once_applied() {
        let (live, ctx) = (live(), ctx());
        let mut d = draft(json!({
            "new-1": {"kind": 4, "name": "Events"},
            "new-2": {"kind": 0, "name": "news", "parent": "new-1"},
            "10": {"access": {"roles": ["6"]}},
            "11": {"name": "renamed"},
            "12": {"parent": "new-1", "position": 0},
            "13": {"delete": true},
        }));
        validate(&mut d, &live, &ctx.roles, G).unwrap();
        let n = desired(&live, &d, &ctx);
        let steps = plan(&live, &n);
        let rank = |s: &Step| match s {
            Step::Create { kind, .. } if *kind == CATEGORY => 0,
            Step::Create { .. } => 1,
            Step::Rename { .. } => 2,
            Step::Positions(_) => 3,
            Step::Overwrite { .. } | Step::DropOverwrite { .. } => 4,
            Step::Delete { .. } => 5,
        };
        let order: Vec<_> = steps.iter().map(rank).collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]), "{order:?}");
        assert_eq!(order.iter().filter(|k| **k == 5).count(), 1);
        let moved = Move {
            id: 12,
            position: 0,
            parent: Some(Some("new-1".into())),
        };
        assert!(steps.contains(&Step::Positions(vec![moved])));
        // Only the creates went through: the rest is still planned, nothing is created twice.
        let made: HashMap<u64, u64> = n
            .iter()
            .filter(|x| x.new)
            .enumerate()
            .map(|(i, x)| (x.img.id, 1000 + i as u64))
            .collect();
        let mut d2 = d.clone();
        for x in n.iter().filter(|x| x.new) {
            remap(&mut d2, &x.id, &made[&x.img.id].to_string());
        }
        let as_live = |x: &Node| {
            let mut c = x.img.clone();
            c.id = *made.get(&c.id).unwrap_or(&c.id);
            c.parent = c.parent.map(|p| *made.get(&p).unwrap_or(&p));
            c
        };
        let mut half = live.clone();
        half.extend(n.iter().filter(|x| x.new).map(as_live));
        let rest = plan(&half, &desired(&half, &d2, &ctx));
        assert!(rest.iter().all(|s| !matches!(s, Step::Create { .. })));
        assert_eq!(rest.len(), steps.len() - 2);
        // Everything went through: nothing is left.
        let all: Vec<ChannelImg> = n.iter().filter(|x| !x.deleted).map(as_live).collect();
        assert!(plan(&all, &desired(&all, &d2, &ctx)).is_empty());
    }
}

#[cfg(test)]
mod db_tests {
    use crate::{auth::Sess, discord::Discord, AppState};
    use axum::{body::Body, http::Request, routing::get, Json, Router};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// Runs only when DATABASE_URL points at a scratch Postgres. Discord is a local mock.
    #[tokio::test]
    async fn draft_is_saved_discarded_and_pushed_only_when_safe() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_600_000_000 + std::process::id() as i64;
        let owner = 7_800_000_000 + std::process::id() as i64;
        let calls: Arc<Mutex<Vec<String>>> = Default::default();
        let log = |c: &Arc<Mutex<Vec<String>>>, s: String| c.lock().unwrap().push(s);
        let (c1, c2) = (calls.clone(), calls.clone());
        // A role overwrite that grants Manage Channels: Push must refuse until it is stripped.
        let channels = json!([{"id": "300", "name": "chat", "type": 0, "position": 0,
            "permission_overwrites": [{"id": "5", "type": 0, "allow": "16", "deny": "0"}]}]);
        let mock = Router::new()
            .route(
                "/guilds/{g}",
                get(move || async move { Json(json!({"owner_id": owner.to_string(), "mfa_level": 1})) }),
            )
            .route(
                "/guilds/{g}/roles",
                get(move || async move {
                    Json(json!([{"id": g.to_string(), "name": "@everyone", "permissions": "1024", "position": 0},
                        {"id": "5", "name": "Mods", "permissions": "0", "position": 1}]))
                }),
            )
            .route(
                "/guilds/{g}/channels",
                get(move || async move { Json(channels) }).post(move |Json(b): Json<Value>| async move {
                    log(&c1, format!("create {}", b["name"]));
                    Json(json!({"id": "301"}))
                }),
            )
            .route("/users/@me", get(|| async { Json(json!({"id": "7"})) }))
            .route(
                "/guilds/{g}/members/{u}",
                get(|| async { Json(json!({"roles": []})) }),
            )
            .route(
                "/channels/{c}/permissions/{t}",
                axum::routing::put(move |axum::extract::Path((c, t)): axum::extract::Path<(String, String)>, Json(b): Json<Value>| async move {
                    log(&c2, format!("put {c} {t} {}", b["allow"]));
                    Json(json!({}))
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, mock).await });
        let st = AppState {
            db: db.clone(),
            discord: Discord {
                http: reqwest::Client::builder().no_proxy().build().unwrap(),
                client_id: "7".into(),
                client_secret: String::new(),
                bot_token: String::new(),
                redirect_uri: String::new(),
                api,
                memo: Default::default(),
            },
            secure: false,
            totp: Default::default(),
            rate: Default::default(),
        };
        let send = |m: &str, path: &str, user: i64, stepped_up: bool, body: Option<Value>| {
            let mut req = Request::builder()
                .method(m)
                .uri(format!("/api/guilds/{g}/channel-setup{path}"))
                .header("content-type", "application/json")
                .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
                .unwrap();
            req.extensions_mut().insert(Sess {
                id: String::new(),
                user_id: user,
                username: "t".into(),
                access_token: String::new(),
                stepped_up,
                csrf: String::new(),
            });
            let app = crate::router(st.clone(), "/nonexistent");
            async move {
                let res = app.oneshot(req).await.unwrap();
                let code = res.status().as_u16();
                let b = axum::body::to_bytes(res.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (
                    code,
                    serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null),
                )
            }
        };
        let stored = || async {
            sqlx::query_scalar::<_, i64>("select count(*) from channel_drafts where guild_id = $1")
                .bind(g)
                .fetch_one(&db)
                .await
                .unwrap()
        };

        let (code, r) = send("GET", "", owner, false, None).await;
        assert_eq!(code, 200, "{r}");
        assert_eq!(
            (r["changes"].as_i64(), r["can_unstaff"].as_bool()),
            (Some(0), Some(true))
        );
        assert_eq!(r["issues"][0]["id"], "overwrite_300_5");
        assert_eq!(r["tree"][0]["issues"][0]["fix"], json!({"strip": "5"}));

        let new = json!({"draft": {"items": {"new-1": {"kind": 0, "name": "News Room"}}}});
        let (code, r) = send("PUT", "", owner, false, Some(new)).await;
        assert_eq!(code, 200, "{r}");
        assert_eq!(r["changes"], 1);
        assert_eq!(r["draft"]["items"]["new-1"]["name"], "news-room");
        assert_eq!(stored().await, 1);
        let bad = json!({"draft": {"items": {"300": {"perms": 8}}}});
        assert_eq!(send("PUT", "", owner, false, Some(bad)).await.0, 400);
        let other = json!({"draft": {"items": {}}});
        assert_eq!(send("PUT", "", owner + 1, false, Some(other)).await.0, 403);

        let (code, r) = send("POST", "/push", owner, false, None).await;
        assert_eq!((code, r["error"].as_str()), (403, Some("step_up_required")));
        let (code, r) = send("POST", "/push", owner, true, None).await;
        assert_eq!(code, 409, "{r}");
        assert_eq!(r["issues"][0]["id"], "overwrite_300_5");
        assert!(calls.lock().unwrap().is_empty(), "nothing ran");

        let fixed = json!({"draft": {"items": {"new-1": {"kind": 0, "name": "news"}, "300": {"strip": ["5"]}}}});
        assert_eq!(send("PUT", "", owner, false, Some(fixed)).await.0, 200);
        let (code, r) = send("POST", "/push", owner, true, None).await;
        assert_eq!(code, 200, "{r}");
        assert_eq!(
            (r["done"].as_i64(), r["failed"].as_array().map(Vec::len)),
            (Some(2), Some(0))
        );
        assert!(r["backup"].is_i64());
        assert_eq!(
            *calls.lock().unwrap(),
            ["create \"news\"", "put 300 5 \"0\""]
        );
        assert_eq!(stored().await, 0, "a full push deletes the draft");

        let again = json!({"draft": {"items": {"new-1": {"kind": 4, "name": "x"}}}});
        assert_eq!(send("PUT", "", owner, false, Some(again)).await.0, 200);
        let (code, r) = send("DELETE", "", owner, false, None).await;
        assert_eq!((code, &r["draft"]), (200, &json!({"items": {}})));
        assert_eq!(stored().await, 0);
        for t in ["backups", "channel_drafts"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "delete from {t} where guild_id = $1"
            )))
            .bind(g)
            .execute(&db)
            .await
            .unwrap();
        }
    }
}

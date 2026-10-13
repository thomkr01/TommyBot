//! Server images and sync-style restore planning (spec 4.3). Pure: `plan` compares an image with
//! the live server and returns what to create, update and delete. The bot executes the plan.
//! Matching uses stable ids first, then name+kind(+parent) (spec 4.3).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleImg {
    pub id: u64,
    pub name: String,
    pub color: u32,
    pub permissions: u64,
    pub position: i64,
    pub hoist: bool,
    pub mentionable: bool,
    /// Bot/integration roles can't be created or deleted by us, so plans ignore them.
    pub managed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverwriteImg {
    pub target: u64,
    pub is_role: bool,
    pub allow: u64,
    pub deny: u64,
}

/// `kind` is Discord's channel type number; 4 is a category.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelImg {
    pub id: u64,
    pub name: String,
    pub kind: u8,
    pub parent: Option<u64>,
    pub position: i64,
    pub topic: Option<String>,
    pub nsfw: bool,
    pub overwrites: Vec<OverwriteImg>,
}

pub const CATEGORY: u8 = 4;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub guild: u64,
    pub name: String,
    pub roles: Vec<RoleImg>,
    pub channels: Vec<ChannelImg>,
}

/// A planned change. Ids on `Update`/`Delete` are live ids; `Create` carries the image object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change<T> {
    Create(T),
    Update { live: u64, to: T },
    Delete(u64),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub roles: Vec<Change<RoleImg>>,
    /// Categories first, so children can resolve their parent through the id map.
    pub channels: Vec<Change<ChannelImg>>,
    /// Image id -> live id for objects that already exist. The bot adds created objects as it goes.
    pub id_map: HashMap<u64, u64>,
}

/// Rewrites overwrite targets from image ids to live ids; unmapped targets are dropped.
pub fn map_overwrites(ows: &[OverwriteImg], id_map: &HashMap<u64, u64>) -> Vec<OverwriteImg> {
    ows.iter()
        .filter_map(|o| {
            id_map.get(&o.target).map(|t| OverwriteImg {
                target: *t,
                ..o.clone()
            })
        })
        .collect()
}

/// Role fields that matter for "needs an update"; position is restored separately in one call.
fn role_same(a: &RoleImg, b: &RoleImg) -> bool {
    (&a.name, a.color, a.permissions, a.hoist, a.mentionable)
        == (&b.name, b.color, b.permissions, b.hoist, b.mentionable)
}

fn same_overwrites(a: &[OverwriteImg], b: &[OverwriteImg]) -> bool {
    let key = |o: &OverwriteImg| (o.target, o.is_role, o.allow, o.deny);
    let (mut x, mut y): (Vec<_>, Vec<_>) =
        (a.iter().map(key).collect(), b.iter().map(key).collect());
    x.sort_unstable();
    y.sort_unstable();
    x == y
}

/// `keep` holds live ids that must never be deleted (the quarantine role, log channels).
/// `respect_lock` leaves permissions of existing roles and overwrites of existing channels alone
/// (panic mode must not undo its own lock); created objects always get theirs.
pub fn plan(image: &Image, live: &Image, keep: &HashSet<u64>, respect_lock: bool) -> Plan {
    let mut p = Plan::default();
    // @everyone shares the guild id; it always exists, so it only maps.
    p.id_map.insert(image.guild, live.guild);
    let mut used = HashSet::new();

    for r in image
        .roles
        .iter()
        .filter(|r| !r.managed && r.id != image.guild)
    {
        let hit = live.roles.iter().find(|l| l.id == r.id).or_else(|| {
            live.roles
                .iter()
                .find(|l| l.name == r.name && !used.contains(&l.id))
        });
        match hit.filter(|l| !l.managed) {
            Some(l) => {
                used.insert(l.id);
                p.id_map.insert(r.id, l.id);
                let mut to = r.clone();
                if respect_lock {
                    to.permissions = l.permissions;
                }
                if !role_same(l, &to) {
                    p.roles.push(Change::Update { live: l.id, to });
                }
            }
            None => p.roles.push(Change::Create(r.clone())),
        }
    }
    for l in &live.roles {
        if !l.managed && l.id != live.guild && !used.contains(&l.id) && !keep.contains(&l.id) {
            p.roles.push(Change::Delete(l.id));
        }
    }

    let mut chans: Vec<&ChannelImg> = image.channels.iter().collect();
    chans.sort_by_key(|c| c.kind != CATEGORY);
    for c in chans {
        let parent = c.parent.and_then(|id| p.id_map.get(&id)).copied();
        let hit = live.channels.iter().find(|l| l.id == c.id).or_else(|| {
            live.channels.iter().find(|l| {
                l.name == c.name && l.kind == c.kind && l.parent == parent && !used.contains(&l.id)
            })
        });
        match hit {
            Some(l) => {
                used.insert(l.id);
                p.id_map.insert(c.id, l.id);
                let parent_ok = match c.parent {
                    None => l.parent.is_none(),
                    Some(_) => parent.is_some() && parent == l.parent,
                };
                let ows = map_overwrites(&c.overwrites, &p.id_map);
                let ows_ok = respect_lock
                    || (ows.len() == c.overwrites.len() && same_overwrites(&ows, &l.overwrites));
                if (&c.name, &c.topic, c.nsfw) != (&l.name, &l.topic, l.nsfw)
                    || !parent_ok
                    || !ows_ok
                {
                    // `to` keeps image ids for parent and overwrites; the bot maps them when it runs.
                    let mut to = c.clone();
                    to.id = l.id;
                    if respect_lock {
                        to.overwrites = vec![];
                    }
                    p.channels.push(Change::Update { live: l.id, to });
                }
            }
            None => p.channels.push(Change::Create(c.clone())),
        }
    }
    for l in &live.channels {
        if !used.contains(&l.id) && !keep.contains(&l.id) {
            p.channels.push(Change::Delete(l.id));
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: u64, name: &str) -> RoleImg {
        RoleImg {
            id,
            name: name.into(),
            color: 0,
            permissions: 0,
            position: 1,
            hoist: false,
            mentionable: false,
            managed: false,
        }
    }
    fn chan(id: u64, name: &str, kind: u8, parent: Option<u64>) -> ChannelImg {
        ChannelImg {
            id,
            name: name.into(),
            kind,
            parent,
            position: 0,
            topic: None,
            nsfw: false,
            overwrites: vec![],
        }
    }
    fn is_empty(p: &Plan) -> bool {
        p.roles.is_empty() && p.channels.is_empty()
    }
    fn img(roles: Vec<RoleImg>, channels: Vec<ChannelImg>) -> Image {
        Image {
            guild: 1,
            name: "g".into(),
            roles,
            channels,
        }
    }

    #[test]
    fn identical_server_is_an_empty_plan() {
        let i = img(vec![role(10, "Mod")], vec![chan(20, "general", 0, None)]);
        assert!(is_empty(&plan(&i, &i, &HashSet::new(), false)));
    }

    #[test]
    fn nuke_is_reversed_roles_and_channels() {
        let image = img(
            vec![role(10, "Mod"), role(11, "Member")],
            vec![
                chan(20, "cat", CATEGORY, None),
                chan(21, "general", 0, Some(20)),
            ],
        );
        // Mod role deleted, "cat" category deleted, attacker added a role and a channel.
        let live = img(
            vec![role(11, "Member"), role(99, "evil")],
            vec![chan(21, "general", 0, None), chan(98, "spam", 0, None)],
        );
        let p = plan(&image, &live, &HashSet::new(), false);
        assert_eq!(
            p.roles,
            vec![Change::Create(role(10, "Mod")), Change::Delete(99)]
        );
        assert_eq!(
            p.channels,
            vec![
                Change::Create(chan(20, "cat", CATEGORY, None)),
                Change::Update {
                    live: 21,
                    to: chan(21, "general", 0, Some(20))
                },
                Change::Delete(98),
            ]
        );
    }

    #[test]
    fn recreated_objects_match_by_name_and_get_updated() {
        let mut changed = role(55, "Mod");
        changed.permissions = 8;
        let p = plan(
            &img(vec![role(10, "Mod")], vec![]),
            &img(vec![changed], vec![]),
            &HashSet::new(),
            false,
        );
        assert_eq!(
            p.roles,
            vec![Change::Update {
                live: 55,
                to: role(10, "Mod")
            }]
        );
        assert_eq!(p.id_map[&10], 55);
    }

    #[test]
    fn managed_and_kept_objects_survive() {
        let mut bot = role(30, "Bot");
        bot.managed = true;
        let live = img(
            vec![bot, role(31, "Quarantine")],
            vec![chan(40, "logs", 0, None)],
        );
        let keep = HashSet::from([31, 40]);
        assert!(is_empty(&plan(&img(vec![], vec![]), &live, &keep, false)));
    }

    #[test]
    fn overwrites_sync_flag_and_mapping() {
        let ow = |t| OverwriteImg {
            target: t,
            is_role: true,
            allow: 0,
            deny: 1024,
        };
        let mut want = chan(20, "general", 0, None);
        want.overwrites = vec![ow(10)];
        let mut now = chan(20, "general", 0, None);
        now.overwrites = vec![ow(1)]; // locked: @everyone denied
        let (image, live) = (
            img(vec![role(10, "Mod")], vec![want]),
            img(vec![role(10, "Mod")], vec![now]),
        );
        assert_eq!(plan(&image, &live, &HashSet::new(), true).channels, vec![]);
        assert_eq!(
            plan(&image, &live, &HashSet::new(), false).channels.len(),
            1
        );
        assert_eq!(
            map_overwrites(&[ow(10), ow(77)], &HashMap::from([(10, 500)])),
            vec![ow(500)]
        );
    }

    #[test]
    fn respect_lock_keeps_live_role_permissions() {
        let mut locked = role(10, "Mod");
        locked.permissions = 0;
        let mut want = role(10, "Mod");
        want.permissions = 8;
        let (image, live) = (img(vec![want], vec![]), img(vec![locked], vec![]));
        assert!(is_empty(&plan(&image, &live, &HashSet::new(), true)));
        assert_eq!(plan(&image, &live, &HashSet::new(), false).roles.len(), 1);
    }
}

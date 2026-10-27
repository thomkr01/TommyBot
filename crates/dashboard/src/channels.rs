//! Channel setup (role profiles): each profile gives its roles a permission set on the channels and
//! categories it targets. Preview shows the overwrite changes; apply takes a backup first, then writes them.
use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::Sess,
    recovery::live_image,
    store, ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use tommybot_antinuke::{ChannelImg, Image};

/// (key, Discord permission bit) a profile can allow or deny.
pub const PERMS: &[(&str, u64)] = &[
    ("view", 1 << 10),
    ("send", 1 << 11),
    ("history", 1 << 16),
    ("react", 1 << 6),
    ("attach", 1 << 15),
    ("embed", 1 << 14),
    ("threads", 1 << 38),
    ("mention_everyone", 1 << 17),
    ("manage_messages", 1 << 13),
    ("manage_channel", 1 << 4),
    ("manage_permissions", 1 << 28),
    ("connect", 1 << 20),
    ("speak", 1 << 21),
];
const VIEW: u64 = 1 << 10;

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Perm {
    Allow,
    Deny,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub roles: Vec<String>,
    /// Permission key from `PERMS` -> allow or deny; keys left out stay neutral.
    pub perms: BTreeMap<String, Perm>,
    /// Hide the targeted channels from @everyone (the mod/admin preset).
    pub private: bool,
    pub channels: Vec<String>,
    /// A category covers itself and every channel in it.
    pub categories: Vec<String>,
    pub exclude: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ProfilesCfg {
    pub profiles: Vec<Profile>,
}

/// One role overwrite on one channel that differs from what the profiles want.
#[derive(Debug, PartialEq)]
pub struct Change {
    pub channel: u64,
    pub role: u64,
    pub allow: u64,
    pub deny: u64,
    pub before: Option<(u64, u64)>,
    pub profiles: Vec<String>,
}

fn ids(v: &[String]) -> Vec<u64> {
    v.iter().filter_map(|s| s.parse().ok()).collect()
}

fn bits(p: &Profile, want: Perm) -> u64 {
    PERMS
        .iter()
        .filter(|(k, _)| p.perms.get(*k) == Some(&want))
        .fold(0, |acc, (_, b)| acc | b)
}

/// Overwrites the profiles want that the channels do not have yet. Allow wins when profiles
/// disagree on a role, so an explicit @everyone profile overrides another profile's `private`.
pub fn plan(guild: u64, profiles: &[Profile], channels: &[ChannelImg]) -> Vec<Change> {
    let mut want: BTreeMap<(u64, u64), (u64, u64, Vec<String>)> = BTreeMap::new();
    for p in profiles {
        let (picked, cats, skip) = (ids(&p.channels), ids(&p.categories), ids(&p.exclude));
        let (allow, deny) = (bits(p, Perm::Allow), bits(p, Perm::Deny));
        let targets: Vec<u64> = channels
            .iter()
            .filter(|c| !skip.contains(&c.id))
            .filter(|c| {
                picked.contains(&c.id)
                    || cats.contains(&c.id)
                    || c.parent.is_some_and(|x| cats.contains(&x))
            })
            .map(|c| c.id)
            .collect();
        let mut add = |channel, role, a, d| {
            let e = want.entry((channel, role)).or_default();
            e.0 |= a;
            e.1 |= d;
            if !e.2.contains(&p.name) {
                e.2.push(p.name.clone());
            }
        };
        for &c in &targets {
            for r in ids(&p.roles) {
                add(c, r, allow, deny);
            }
            if p.private {
                add(c, guild, 0, VIEW);
            }
        }
    }
    let by_id: HashMap<u64, &ChannelImg> = channels.iter().map(|c| (c.id, c)).collect();
    want.into_iter()
        .filter_map(|((channel, role), (allow, deny, profiles))| {
            let deny = deny & !allow;
            let before = by_id[&channel]
                .overwrites
                .iter()
                .find(|o| o.is_role && o.target == role)
                .map(|o| (o.allow, o.deny));
            (before.unwrap_or_default() != (allow, deny)).then_some(Change {
                channel,
                role,
                allow,
                deny,
                before,
                profiles,
            })
        })
        .collect()
}

fn names(bits: u64) -> String {
    let n: Vec<&str> = PERMS
        .iter()
        .filter(|(_, b)| bits & b != 0)
        .map(|(k, _)| *k)
        .collect();
    if n.is_empty() {
        "-".into()
    } else {
        n.join(", ")
    }
}

fn line(c: &Change, img: &Image) -> String {
    let was = c.before.map_or("none".into(), |(a, d)| {
        format!("allow {}, deny {}", names(a), names(d))
    });
    format!(
        "#{} @{} [{}]: allow {}, deny {} (was {was})",
        img.channels
            .iter()
            .find(|x| x.id == c.channel)
            .map_or("?", |x| &x.name),
        img.roles
            .iter()
            .find(|x| x.id == c.role)
            .map_or("?", |x| &x.name),
        c.profiles.join(", "),
        names(c.allow),
        names(c.deny),
    )
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/channel-setup/preview", get(preview))
        .route("/api/guilds/{guild}/channel-setup/apply", post(apply))
}

async fn live(st: &AppState, guild: i64) -> Result<(Image, Vec<Change>), ApiError> {
    let (_, config) = store::current(&st.db, guild).await?;
    let cfg: ProfilesCfg =
        serde_json::from_value(config["channel_profiles"].clone()).unwrap_or_default();
    let roles = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/roles"), None)
        .await?;
    let channels = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
        .await?;
    let img = live_image(guild, &roles, &channels);
    let changes = plan(guild as u64, &cfg.profiles, &img.channels);
    Ok((img, changes))
}

async fn preview(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let (img, changes) = live(&st, guild).await?;
    let lines: Vec<String> = changes.iter().map(|c| line(c, &img)).collect();
    Ok(Json(json!({"lines": lines})))
}

/// Snapshot first (a normal backup row, so Backups can revert it), then one overwrite at a time.
/// Already-correct overwrites are skipped, so running it again resumes after a failure.
async fn apply(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    let (img, changes) = live(&st, guild).await?;
    if changes.is_empty() {
        return Ok(Json(json!({"applied": 0, "failed": [], "backup": null})));
    }
    let backup: i64 =
        sqlx::query_scalar("INSERT INTO backups (guild_id, image) VALUES ($1, $2) RETURNING id")
            .bind(guild)
            .bind(sqlx::types::Json(&img))
            .fetch_one(&st.db)
            .await?;
    let (mut applied, mut failed) = (0, vec![]);
    // shortcut: no 429 retry here, so a big server can see rate-limit failures; Apply again resumes.
    for c in &changes {
        let path = format!("/channels/{}/permissions/{}", c.channel, c.role);
        let body = json!({"type": 0, "allow": c.allow.to_string(), "deny": c.deny.to_string()});
        match st.discord.call(Method::PUT, &path, Some(body)).await {
            Ok(_) => applied += 1,
            Err(e) => failed.push(format!("{}: {e}", line(c, &img))),
        }
    }
    let (_, config) = store::current(&st.db, guild).await?;
    log_change(
        &st,
        &config,
        format!(
            "{} (<@{}>) applied the channel setup: {applied} overwrite(s), {} failed, backup #{backup}",
            s.username,
            s.user_id,
            failed.len()
        ),
    )
    .await;
    Ok(Json(
        json!({"applied": applied, "failed": failed, "backup": backup}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tommybot_antinuke::OverwriteImg;

    fn ch(id: u64, parent: Option<u64>, ow: Vec<OverwriteImg>) -> ChannelImg {
        ChannelImg {
            id,
            name: id.to_string(),
            kind: 0,
            parent,
            position: 0,
            topic: None,
            nsfw: false,
            overwrites: ow,
        }
    }

    fn profile(
        name: &str,
        roles: &[&str],
        perms: &[(&str, Perm)],
        f: impl FnOnce(&mut Profile),
    ) -> Profile {
        let mut p = Profile {
            name: name.into(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
            perms: perms.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..Default::default()
        };
        f(&mut p);
        p
    }

    #[test]
    fn category_expands_and_excludes() {
        let chans = [
            ch(10, None, vec![]),
            ch(11, Some(10), vec![]),
            ch(12, Some(10), vec![]),
            ch(13, None, vec![]),
        ];
        let p = profile("Support", &["5"], &[("view", Perm::Allow)], |p| {
            p.categories = vec!["10".into()];
            p.exclude = vec!["12".into()];
        });
        let got: Vec<u64> = plan(1, &[p], &chans).iter().map(|c| c.channel).collect();
        assert_eq!(got, vec![10, 11]);
    }

    #[test]
    fn private_hides_from_everyone_and_merges_profiles() {
        let chans = [ch(20, None, vec![])];
        let staff = profile(
            "Staff",
            &["5"],
            &[("view", Perm::Allow), ("send", Perm::Allow)],
            |p| {
                p.private = true;
                p.channels = vec!["20".into()];
            },
        );
        let events = profile("Events", &["5"], &[("manage_messages", Perm::Deny)], |p| {
            p.channels = vec!["20".into()]
        });
        let out = plan(1, &[staff, events], &chans);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].role, out[0].allow, out[0].deny), (1, 0, VIEW));
        assert_eq!(
            (out[1].role, out[1].allow, out[1].deny),
            (5, VIEW | (1 << 11), 1 << 13)
        );
        assert_eq!(out[1].profiles, vec!["Staff", "Events"]);
    }

    #[test]
    fn skips_channels_already_correct_and_allow_wins() {
        let ow = OverwriteImg {
            target: 5,
            is_role: true,
            allow: VIEW,
            deny: 0,
        };
        let chans = [ch(30, None, vec![ow])];
        let a = profile("A", &["5"], &[("view", Perm::Allow)], |p| {
            p.channels = vec!["30".into()]
        });
        let b = profile("B", &["5"], &[("view", Perm::Deny)], |p| {
            p.channels = vec!["30".into()]
        });
        assert!(plan(1, &[a, b], &chans).is_empty());
    }
}

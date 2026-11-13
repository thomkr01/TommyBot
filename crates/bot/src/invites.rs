//! Invite tracking and the public join-raid report link.
use std::collections::HashMap;
use twilight_model::id::{marker::GuildMarker, Id};

use crate::app::App;

/// The invite the next joiner used: the one whose use count grew since the last join, or the
/// only one that vanished (a last use deletes it). Needs Manage Server; without it nothing is
/// tracked. Returns (code, inviter name). It runs before raid detection on every join, so a slow or
/// rate-limited lookup gives up after 1.5 s (no invite is named for that joiner).
pub async fn used_invite(app: &App, gid: Id<GuildMarker>) -> Option<(String, String)> {
    let fetch = async { app.http.guild_invites(gid).await.ok()?.models().await.ok() };
    let list = tokio::time::timeout(std::time::Duration::from_millis(1500), fetch)
        .await
        .ok()??;
    let now: HashMap<String, u64> = list
        .iter()
        .map(|i| (i.code.clone(), i.uses.unwrap_or(0)))
        .collect();
    let old = app.invites.lock().unwrap().insert(gid.get(), now.clone())?;
    let code = diff_invites(&old, &now)?;
    let who = list
        .iter()
        .find(|i| i.code == code)
        .and_then(|i| i.inviter.as_ref())
        .map_or("unknown".into(), |u| u.name.clone());
    Some((code, who))
}

/// The invite whose use count grew, else the only one that vanished (a last use deletes it).
fn diff_invites(old: &HashMap<String, u64>, now: &HashMap<String, u64>) -> Option<String> {
    if let Some((code, _)) = now
        .iter()
        .find(|(c, n)| **n > old.get(*c).copied().unwrap_or(0))
    {
        return Some(code.clone());
    }
    let mut gone = old.keys().filter(|c| !now.contains_key(*c));
    match (gone.next(), gone.next()) {
        (Some(code), None) => Some(code.clone()),
        _ => None,
    }
}

/// The public report page of a raid, when the dashboard URL is configured.
pub fn raid_url(code: &str) -> Option<String> {
    let base = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|b| !b.is_empty())?;
    Some(format!("{}/raid/{code}", base.trim_end_matches('/')))
}

/// The log shows the first 8 digits of the id next to the name.
pub fn short_id(id: u64) -> String {
    id.to_string().chars().take(8).collect()
}

/// `<10 random>_<last 10 digits of the guild id>`: names a raid on the public report page.
pub fn raid_code(gid: u64) -> String {
    let random: String = (0..10)
        .map(|_| char::from(b"abcdefghijklmnopqrstuvwxyz0123456789"[rand::random_range(0..36)]))
        .collect();
    let id = gid.to_string();
    format!("{random}_{}", &id[id.len().saturating_sub(10)..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_diff_finds_the_grown_or_vanished_invite() {
        let m = |v: &[(&str, u64)]| v.iter().map(|(c, n)| (c.to_string(), *n)).collect();
        let old = m(&[("a", 1), ("b", 2)]);
        assert_eq!(
            diff_invites(&old, &m(&[("a", 1), ("b", 3)])).as_deref(),
            Some("b")
        );
        assert_eq!(diff_invites(&old, &m(&[("a", 1)])).as_deref(), Some("b"));
        assert_eq!(
            diff_invites(&old, &m(&[("a", 1), ("b", 2), ("c", 1)])).as_deref(),
            Some("c")
        );
        assert_eq!(diff_invites(&old, &old), None);
    }

    #[test]
    fn raid_code_ends_in_the_guild_id() {
        let c = raid_code(123_456_789_012_345_678);
        let (random, id) = c.split_once('_').unwrap();
        assert_eq!((random.len(), id), (10, "9012345678"));
    }
}

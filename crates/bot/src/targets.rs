//! Resolves a typed name (username, global name, nick or a unique prefix) to one guild member.
use anyhow::Result;
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

use crate::app::App;

#[derive(Debug, PartialEq, Eq)]
pub enum Pick {
    One(u64),
    None,
    Ambiguous,
}

/// `cands` are (id, names) from a prefix search. An exact name match wins; otherwise exactly one
/// candidate must remain.
pub fn pick(query: &str, cands: &[(u64, Vec<String>)]) -> Pick {
    let q = query.to_lowercase();
    let exact: Vec<_> = cands
        .iter()
        .filter(|(_, names)| names.iter().any(|n| n.to_lowercase() == q))
        .collect();
    match (exact.as_slice(), cands) {
        ([(id, _)], _) | ([], [(id, _)]) => Pick::One(*id),
        ([], []) => Pick::None,
        _ => Pick::Ambiguous,
    }
}

/// `name#0` and `name#1234` (old Discord tags) search as `name`.
pub fn untag(name: &str) -> &str {
    match name.rsplit_once('#') {
        Some((n, tag))
            if !n.is_empty()
                && (1..=4).contains(&tag.len())
                && tag.bytes().all(|b| b.is_ascii_digit()) =>
        {
            n
        }
        _ => name,
    }
}

/// [`resolve`] for `unban`: a banned user is no member, so look through the ban list instead.
pub async fn resolve_banned(
    app: &App,
    gid: Id<GuildMarker>,
    name: &str,
) -> Result<Result<Id<UserMarker>, String>> {
    let name = untag(name);
    let mut bans: Vec<twilight_model::guild::Ban> = Vec::new();
    loop {
        let mut req = app.http.bans(gid).limit(1000);
        if let Some(last) = bans.last().map(|b| b.user.id) {
            req = req.after(last);
        }
        let page = req.await?.models().await?;
        let full = page.len() == 1000;
        bans.extend(page);
        if !full {
            break;
        }
    }
    let cands: Vec<_> = bans
        .into_iter()
        .map(|b| {
            let names = [Some(b.user.name), b.user.global_name]
                .into_iter()
                .flatten()
                .collect();
            (b.user.id.get(), names)
        })
        .filter(|(_, names): &(u64, Vec<String>)| {
            names
                .iter()
                .any(|n| n.to_lowercase().starts_with(&name.to_lowercase()))
        })
        .collect();
    Ok(match pick(name, &cands) {
        Pick::One(id) => Ok(Id::new(id)),
        Pick::None => Err(format!("No banned user found for `{name}`")),
        Pick::Ambiguous => Err(format!("`{name}` is ambiguous, use the id")),
    })
}

/// Ok(Err(text)) is a user-facing failure ("not found" / "ambiguous").
pub async fn resolve(
    app: &App,
    gid: Id<GuildMarker>,
    name: &str,
) -> Result<Result<Id<UserMarker>, String>> {
    let name = untag(name);
    let found = app
        .http
        .search_guild_members(gid, name)
        .limit(5)
        .await?
        .models()
        .await?;
    let cands: Vec<_> = found
        .into_iter()
        .map(|m| {
            let names = [Some(m.user.name), m.user.global_name, m.nick]
                .into_iter()
                .flatten()
                .collect();
            (m.user.id.get(), names)
        })
        .collect();
    Ok(match pick(name, &cands) {
        Pick::One(id) => Ok(Id::new(id)),
        Pick::None => Err(format!("No member found for `{name}`")),
        Pick::Ambiguous => Err(format!("`{name}` is ambiguous, use a mention or id")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u64, n: &[&str]) -> (u64, Vec<String>) {
        (id, n.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn tags_search_as_names() {
        assert_eq!(untag("tile_from_chile#0"), "tile_from_chile");
        assert_eq!(untag("bob#1234"), "bob");
        assert_eq!(untag("bob#12345"), "bob#12345");
        assert_eq!(untag("bob#x1"), "bob#x1");
        assert_eq!(untag("#0"), "#0");
        let cands = [c(1, &["tile_from_chile"])];
        assert_eq!(pick(untag("tile_from_chile#0"), &cands), Pick::One(1));
    }

    #[test]
    fn picks() {
        assert_eq!(pick("al", &[]), Pick::None);
        assert_eq!(pick("al", &[c(1, &["alice"])]), Pick::One(1));
        assert_eq!(
            pick("al", &[c(1, &["alice"]), c(2, &["alan"])]),
            Pick::Ambiguous
        );
        // exact beats prefix, case-insensitively
        assert_eq!(
            pick("Al", &[c(1, &["alice"]), c(2, &["al", "x"])]),
            Pick::One(2)
        );
        assert_eq!(pick("al", &[c(1, &["al"]), c(2, &["Al"])]), Pick::Ambiguous);
    }
}

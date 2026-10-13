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

/// Ok(Err(text)) is a user-facing failure ("not found" / "ambiguous").
pub async fn resolve(
    app: &App,
    gid: Id<GuildMarker>,
    name: &str,
) -> Result<Result<Id<UserMarker>, String>> {
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

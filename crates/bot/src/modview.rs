//! Read-only helpers of the `modcmds` commands: case and note listings and nickname cleanup.
use anyhow::Result;

use crate::app::App;

/// Wick's `?t` codes (B, UB, K, TIMEOUT, WA, Q, UQ) and plain kind names, singular or plural.
pub(crate) fn case_kind(t: Option<&str>) -> Result<Option<String>, String> {
    let Some(t) = t else { return Ok(None) };
    let kind = match t.trim_end_matches('s') {
        "b" | "ban" => "ban",
        "ub" | "unban" => "unban",
        "k" | "kick" => "kick",
        "t" | "to" | "timeout" | "mute" => "timeout",
        "wa" | "warn" => "warn",
        "q" | "quarantine" => "quarantine",
        "uq" | "unquarantine" => "unquarantine",
        _ => {
            return Err(format!(
                "Unknown case type `{t}`: use b, ub, k, timeout, wa, q or uq"
            ))
        }
    };
    Ok(Some(kind.into()))
}

/// Fancy Unicode folded to plain letters (Wick: 𝓽𝓲𝓶𝓶𝔂 -> timmy), then leading symbols trimmed.
pub(crate) fn sanitized(name: &str) -> String {
    let cured = if name.is_ascii() {
        name.to_string()
    } else {
        // Symbols would cure to lookalike letters (⚡ -> s), so only letters and digits go in.
        let letters: String = name
            .chars()
            .filter(|c| c.is_ascii() || c.is_alphanumeric())
            .collect();
        crate::automod::cure(&letters)
    };
    let ascii: String = cured
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    let s = ascii
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .trim_end();
    if s.is_empty() {
        "Moderated Nickname".into()
    } else {
        s.into()
    }
}

type CaseRow = (i32, String, i64, i64, Option<String>, bool, i64);

fn fmt_case((no, kind, user, moderator, reason, active, at): &CaseRow) -> String {
    format!(
        "`#{no}` {kind}{} <@{user}> by <@{moderator}> <t:{at}:R>: {}",
        if *active { "" } else { " (inactive)" },
        reason.as_deref().unwrap_or("no reason")
    )
}

/// One case by number, or the 10 latest (optionally of one user, by one moderator, of one kind).
pub(crate) async fn view_cases(
    app: &App,
    g: i64,
    case: Option<i32>,
    user: Option<u64>,
    by: Option<u64>,
    kind: Option<String>,
) -> Result<String> {
    let rows: Vec<CaseRow> = sqlx::query_as(
        "SELECT case_no, kind, user_id, mod_id, reason, active, EXTRACT(EPOCH FROM created_at)::bigint
         FROM cases WHERE guild_id=$1 AND ($2::int IS NULL OR case_no=$2) AND ($3::bigint IS NULL OR user_id=$3)
         AND ($4::bigint IS NULL OR mod_id=$4) AND ($5::text IS NULL OR kind=$5)
         ORDER BY case_no DESC LIMIT 10",
    )
    .bind(g)
    .bind(case)
    .bind(user.map(|u| u as i64))
    .bind(by.map(|u| u as i64))
    .bind(kind)
    .fetch_all(&app.pool)
    .await?;
    Ok(if rows.is_empty() {
        "No matching cases".into()
    } else {
        rows.iter().map(fmt_case).collect::<Vec<_>>().join("\n")
    })
}

pub(crate) async fn view_notes(
    app: &App,
    g: i64,
    user: Option<u64>,
    id: Option<i64>,
    by: Option<u64>,
) -> Result<String> {
    let rows: Vec<(i64, i64, i64, String, i64)> = sqlx::query_as(
        "SELECT id, user_id, author_id, text, EXTRACT(EPOCH FROM created_at)::bigint FROM notes
         WHERE guild_id=$1 AND ($2::bigint IS NULL OR user_id=$2) AND ($3::bigint IS NULL OR id=$3)
         AND ($4::bigint IS NULL OR author_id=$4) ORDER BY id DESC LIMIT 10",
    )
    .bind(g)
    .bind(user.map(|u| u as i64))
    .bind(id)
    .bind(by.map(|u| u as i64))
    .fetch_all(&app.pool)
    .await?;
    Ok(if rows.is_empty() {
        "No matching notes".into()
    } else {
        rows.iter()
            .map(|(id, about, by, text, at)| {
                format!("`#{id}` <@{about}> by <@{by}> <t:{at}:R>: {text}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitized("!!!Bob"), "Bob");
        assert_eq!(sanitized("  .1ｂｏｂ the Builder"), "1bob the builder");
        assert_eq!(sanitized("𝓽𝓲𝓶𝓶𝔂"), "timmy");
        assert_eq!(sanitized("Ann-Marie 2"), "Ann-Marie 2");
        assert_eq!(sanitized("⚡⚡"), "Moderated Nickname");
    }

    #[test]
    fn case_line() {
        let row = (3, "warn".to_string(), 9, 8, None, false, 100);
        assert_eq!(
            fmt_case(&row),
            "`#3` warn (inactive) <@9> by <@8> <t:100:R>: no reason"
        );
    }
}

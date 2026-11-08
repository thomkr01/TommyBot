//! Read-only helpers of the `modcmds` commands: case and note listings and nickname cleanup.
use anyhow::Result;

use tommybot_logging::embed::{Log, BLUE};
use twilight_model::channel::message::Embed;

use crate::app::App;

const PER_PAGE: usize = 10;

/// `?t` codes (B, UB, K, TIMEOUT, WA, Q, UQ) and plain kind names, singular or plural.
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

/// Fancy Unicode folded to plain letters (𝓽𝓲𝓶𝓶𝔂 -> timmy), then leading symbols trimmed.
pub(crate) fn sanitized(name: &str) -> String {
    let cured = if name.is_ascii() {
        name.to_string()
    } else {
        // Symbols would cure to lookalike letters (⚡ -> s), so only letters and digits go in.
        let letters: String = name
            .chars()
            .filter(|c| c.is_ascii() || c.is_alphanumeric())
            .collect();
        tommybot_automod::filters::cure(&letters)
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

/// One case by number, or the 10 latest (optionally of one user, by one moderator, of one kind);
/// `page` skips that many pages of 10.
pub(crate) async fn view_cases(
    app: &App,
    g: i64,
    case: Option<i32>,
    user: Option<u64>,
    by: Option<u64>,
    kind: Option<String>,
    page: usize,
) -> Result<String> {
    let rows: Vec<CaseRow> = sqlx::query_as(
        "SELECT case_no, kind, user_id, mod_id, reason, active, EXTRACT(EPOCH FROM created_at)::bigint
         FROM cases WHERE guild_id=$1 AND ($2::int IS NULL OR case_no=$2) AND ($3::bigint IS NULL OR user_id=$3)
         AND ($4::bigint IS NULL OR mod_id=$4) AND ($5::text IS NULL OR kind=$5)
         ORDER BY case_no DESC LIMIT 10 OFFSET $6",
    )
    .bind(g)
    .bind(case)
    .bind(user.map(|u| u as i64))
    .bind(by.map(|u| u as i64))
    .bind(kind)
    .bind((page * PER_PAGE) as i64)
    .fetch_all(&app.pool)
    .await?;
    Ok(if rows.is_empty() {
        "No matching cases".into()
    } else {
        rows.iter().map(fmt_case).collect::<Vec<_>>().join("\n")
    })
}

/// One member's cases, a page at a time, with the page count (`t!cases NAME`).
pub(crate) async fn cases_page(
    app: &App,
    g: i64,
    user: u64,
    page: usize,
) -> Result<(Embed, usize)> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM cases WHERE guild_id=$1 AND user_id=$2")
            .bind(g)
            .bind(user as i64)
            .fetch_one(&app.pool)
            .await?;
    let pages = (n as usize).div_ceil(PER_PAGE).max(1);
    let page = page.min(pages - 1);
    let text = view_cases(app, g, None, Some(user), None, None, page).await?;
    let e = Log::new("Cases:", BLUE)
        .text(&text)
        .footer(&format!("Page: {}/{pages}", page + 1))
        .build();
    Ok((e, pages))
}

/// A single note as text; `note_embed` turns the same text into a card.
pub(crate) fn note_text(id: &str, user: u64, by: u64, text: &str) -> String {
    format!("Note: {id} ✅\nMember: <@{user}>\nModerator: <@{by}>\nContent: {text}")
}

pub(crate) fn note_embed(text: &str) -> Embed {
    let mut lines = text.lines();
    let mut e = Log::new(lines.next().unwrap_or("Note"), BLUE);
    let rest = lines.collect::<Vec<_>>().join("\n");
    // Content comes last and may span lines, so split on the labels only up to there.
    let (head, content) = rest.split_once("\nContent: ").unwrap_or((&rest, ""));
    for l in head.lines() {
        if let Some((k, v)) = l.split_once(": ") {
            e = e.field(k, v);
        }
    }
    e.field("Content", content).build()
}

/// One note as `note_text`, or the notes as the "Mod Notes (N):" list, newest first.
pub(crate) async fn view_notes(
    app: &App,
    g: i64,
    user: Option<u64>,
    id: Option<String>,
    by: Option<u64>,
) -> Result<String> {
    let rows: Vec<(String, i64, i64, String, i64)> = sqlx::query_as(
        "SELECT short_id, user_id, author_id, text, EXTRACT(EPOCH FROM created_at)::bigint FROM notes
         WHERE guild_id=$1 AND ($2::bigint IS NULL OR user_id=$2) AND ($3::text IS NULL OR short_id=$3)
         AND ($4::bigint IS NULL OR author_id=$4) ORDER BY id DESC LIMIT 10",
    )
    .bind(g)
    .bind(user.map(|u| u as i64))
    .bind(id.as_deref())
    .bind(by.map(|u| u as i64))
    .fetch_all(&app.pool)
    .await?;
    Ok(match (rows.as_slice(), id) {
        ([], _) => "No matching notes".into(),
        ([(id, about, by, text, _)], Some(_)) => note_text(id, *about as u64, *by as u64, text),
        _ => format!(
            "{}Mod Notes ({}):\n{}",
            by.map_or(String::new(), |m| format!("<@{m}> ")),
            rows.len(),
            rows.iter()
                .map(|(id, about, _, _, at)| format!("`{id}` <@{about}> <t:{at}:R>"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
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
    fn note_card_round_trips() {
        let e = note_embed(&note_text("q413p9", 7, 8, "rude\nin vc"));
        assert_eq!(e.title.as_deref(), Some("Note: q413p9 ✅"));
        let f: Vec<_> = e
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.value.as_str()))
            .collect();
        assert_eq!(
            f,
            [
                ("Member", "<@7>"),
                ("Moderator", "<@8>"),
                ("Content", "rude\nin vc")
            ]
        );
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

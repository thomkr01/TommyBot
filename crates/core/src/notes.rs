//! Staff notes: edits that keep a history row, and the mod-log entry every note change sends.
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// What changed; the payload of the `note_log` job, also built by the bot's note commands.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoteLog {
    /// "add", "modify", "remove" or "sweep".
    pub action: String,
    pub id: Option<String>,
    /// The member the note is about (for a sweep: the member filter).
    pub user: Option<u64>,
    pub by: u64,
    /// The note's text (new text for a modify).
    pub text: Option<String>,
    /// The text before a modify.
    pub old: Option<String>,
    /// For a sweep: notes removed, and the author filter.
    pub count: Option<u64>,
    pub author: Option<u64>,
}

/// Changes a note's text and records the old one in `note_edits`, in one statement. Returns the
/// note's member and its old text, or None when the guild has no note `short_id`.
pub async fn edit(
    pool: &PgPool,
    guild: i64,
    short_id: &str,
    text: &str,
    editor: i64,
) -> sqlx::Result<Option<(i64, String)>> {
    sqlx::query_as(
        "WITH old AS (SELECT id, user_id, text FROM notes WHERE guild_id = $1 AND short_id = $2 FOR UPDATE),
              up AS (UPDATE notes n SET text = $3 FROM old WHERE n.id = old.id),
              ed AS (INSERT INTO note_edits (note_id, guild_id, old_text, new_text, editor_id)
                     SELECT id, $1, text, $3, $4 FROM old)
         SELECT user_id, text FROM old",
    )
    .bind(guild)
    .bind(short_id)
    .bind(text)
    .bind(editor)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn edits_keep_the_history_and_the_created_time() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = crate::db::connect(&url).await.unwrap();
        let g = 9_100_000_000 + std::process::id() as i64;
        sqlx::query("DELETE FROM notes WHERE guild_id = $1")
            .bind(g)
            .execute(&pool)
            .await
            .unwrap();
        let (id, short, created): (i64, String, String) = sqlx::query_as(
            "INSERT INTO notes (guild_id, user_id, author_id, text) VALUES ($1, 7, 8, 'first')
             RETURNING id, short_id, created_at::text",
        )
        .bind(g)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            super::edit(&pool, g, &short, "second", 9).await.unwrap(),
            Some((7, "first".into()))
        );
        super::edit(&pool, g, &short, "third", 10).await.unwrap();
        assert_eq!(super::edit(&pool, g, "nope", "x", 9).await.unwrap(), None);
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT old_text, new_text, editor_id FROM note_edits WHERE note_id = $1 ORDER BY id",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                ("first".into(), "second".into(), 9),
                ("second".into(), "third".into(), 10)
            ]
        );
        let (text, at): (String, String) =
            sqlx::query_as("SELECT text, created_at::text FROM notes WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((text.as_str(), at), ("third", created));
        sqlx::query("DELETE FROM notes WHERE guild_id = $1")
            .bind(g)
            .execute(&pool)
            .await
            .unwrap();
    }
}

//! Raw Discord REST calls twilight-http 0.17 has no methods for: invite pausing and native AutoMod rules.
use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};

const API: &str = "https://discord.com/api/v10";
const KEYWORD_RULE: &str = "TommyBot keywords";
/// Discord caps one keyword rule at 1000 keywords of at most 60 chars.
const MAX_KEYWORDS: usize = 1000;
const MAX_KEYWORD_LEN: usize = 60;
/// Discord pauses invites for at most 24 hours.
pub const MAX_INVITE_PAUSE_MS: i64 = 24 * 3_600_000;

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub trigger_type: u8,
}

impl Api {
    pub fn new(token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            token: token.into(),
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder, body: Option<Value>) -> Result<Value> {
        let req = req.header("Authorization", format!("Bot {}", self.token));
        let res = if let Some(b) = body {
            req.json(&b)
        } else {
            req
        }
        .send()
        .await?;
        let status = res.status();
        let text = res.text().await?;
        if !status.is_success() {
            bail!("discord {status}: {text}");
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Pause invites until `until_ms` (unix ms, clamped to 24h from `now_ms`), or lift the pause with `None`.
    pub async fn pause_invites(&self, guild: u64, until: Option<(i64, i64)>) -> Result<()> {
        let body = invites_body(until);
        self.send(
            self.http
                .put(format!("{API}/guilds/{guild}/incident-actions")),
            Some(body),
        )
        .await?;
        Ok(())
    }

    pub async fn list_rules(&self, guild: u64) -> Result<Vec<Rule>> {
        let v = self
            .send(
                self.http
                    .get(format!("{API}/guilds/{guild}/auto-moderation/rules")),
                None,
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    /// Create or update our keyword rule so it blocks exactly `words`. An empty list disables it.
    pub async fn sync_keyword_rule(&self, guild: u64, words: &[String]) -> Result<()> {
        let rules = self.list_rules(guild).await?;
        let body = keyword_rule_body(words);
        match rules.iter().find(|r| r.name == KEYWORD_RULE) {
            Some(r) => {
                self.send(
                    self.http.patch(format!(
                        "{API}/guilds/{guild}/auto-moderation/rules/{}",
                        r.id
                    )),
                    Some(body),
                )
                .await?
            }
            None if words.is_empty() => return Ok(()),
            None => {
                self.send(
                    self.http
                        .post(format!("{API}/guilds/{guild}/auto-moderation/rules")),
                    Some(body),
                )
                .await?
            }
        };
        Ok(())
    }

    pub async fn set_rule_enabled(&self, guild: u64, rule: &str, enabled: bool) -> Result<()> {
        let url = format!("{API}/guilds/{guild}/auto-moderation/rules/{rule}");
        self.send(self.http.patch(url), Some(json!({ "enabled": enabled })))
            .await?;
        Ok(())
    }

    pub async fn delete_rule(&self, guild: u64, rule: &str) -> Result<()> {
        self.send(
            self.http
                .delete(format!("{API}/guilds/{guild}/auto-moderation/rules/{rule}")),
            None,
        )
        .await?;
        Ok(())
    }
}

/// `until` is `(now_ms, until_ms)`.
fn invites_body(until: Option<(i64, i64)>) -> Value {
    let until = until.map(|(now, until)| {
        let ms = until.min(now + MAX_INVITE_PAUSE_MS);
        chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339())
    });
    json!({ "invites_disabled_until": until.flatten() })
}

fn keyword_rule_body(words: &[String]) -> Value {
    let words: Vec<&String> = words
        .iter()
        .filter(|w| !w.is_empty() && w.chars().count() <= MAX_KEYWORD_LEN)
        .take(MAX_KEYWORDS)
        .collect();
    json!({
        "name": KEYWORD_RULE,
        "event_type": 1,
        "trigger_type": 1,
        "trigger_metadata": { "keyword_filter": words },
        "actions": [{ "type": 1 }],
        "enabled": !words.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_pause_is_capped_and_lifted_with_null() {
        let v = invites_body(Some((0, 10 * MAX_INVITE_PAUSE_MS)));
        assert_eq!(v["invites_disabled_until"], "1970-01-02T00:00:00+00:00");
        assert!(invites_body(None)["invites_disabled_until"].is_null());
    }

    #[test]
    fn keyword_rule_drops_bad_keywords_and_disables_when_empty() {
        let long = "x".repeat(61);
        let v = keyword_rule_body(&["bad".into(), "".into(), long]);
        assert_eq!(v["trigger_metadata"]["keyword_filter"], json!(["bad"]));
        assert_eq!(v["enabled"], true);
        assert_eq!(keyword_rule_body(&[])["enabled"], false);
    }
}

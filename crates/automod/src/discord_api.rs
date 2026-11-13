//! Raw Discord REST calls twilight-http 0.17 has no methods for: invite pausing and native AutoMod rules.
use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

const API: &str = "https://discord.com/api/v10";
const KEYWORD_RULE: &str = "TommyBot keywords";
/// Discord caps one keyword rule at 1000 keywords of at most 60 chars.
const MAX_KEYWORDS: usize = 1000;
const MAX_KEYWORD_LEN: usize = 60;
/// Discord allows 10 regex patterns of up to 260 characters per keyword rule.
const MAX_REGEX: usize = 10;
const MAX_REGEX_LEN: usize = 260;
/// Discord pauses invites for at most 24 hours.
pub const MAX_INVITE_PAUSE_MS: i64 = 24 * 3_600_000;

const MAX_RETRIES: u32 = 4;
/// Longest single wait on a 429, so a global ban on the token can't stall a caller for minutes.
const MAX_WAIT: Duration = Duration::from_secs(30);

/// Largest server icon a revert downloads.
pub const ICON_MAX: usize = 10 * 1024 * 1024;

/// A server icon from Discord's CDN as the `data:` URI that sets it again. Sends no token; the
/// caller's client must have a timeout. Used by the bot and the dashboard revert.
pub async fn icon_data_uri(http: &reqwest::Client, guild: u64, hash: &str) -> Result<String> {
    let Some(url) = tommybot_antinuke::icon_url(guild, hash) else {
        bail!("not a Discord icon hash: {hash:?}");
    };
    let res = http.get(&url).send().await?;
    // A redirect must not take the download off the CDN.
    if res.url().host_str() != Some("cdn.discordapp.com") {
        bail!("the icon download left the Discord CDN");
    }
    image_data_uri(res).await
}

/// The body as a `data:` URI: an `image/*` type only, at most [`ICON_MAX`] bytes.
async fn image_data_uri(res: reqwest::Response) -> Result<String> {
    use base64::Engine as _;
    let mut res = res.error_for_status()?;
    let kind = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // Only token characters, so the type cannot reshape the data URI.
    let sub = kind.strip_prefix("image/").unwrap_or("");
    if sub.is_empty()
        || !sub
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b))
    {
        bail!("the icon download is not an image ({kind:?})");
    }
    if res.content_length().is_some_and(|n| n > ICON_MAX as u64) {
        bail!("the icon is larger than 10 MB");
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        if body.len() + chunk.len() > ICON_MAX {
            bail!("the icon is larger than 10 MB");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(format!(
        "data:{kind};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(body)
    ))
}

/// `retry_after` seconds from a 429 body, one second if missing, capped at `MAX_WAIT`.
fn retry_after(body: &str) -> Duration {
    let secs = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["retry_after"].as_f64())
        .unwrap_or(1.0);
    Duration::from_secs_f64(secs.clamp(0.0, MAX_WAIT.as_secs_f64()))
}

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
            // A hung request would otherwise hold its caller (a lockdown, a rule sync) forever.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                // Discord asks for this form; Cloudflare can refuse requests without one.
                .user_agent(concat!(
                    "DiscordBot (https://github.com/thomkr01/TommyBot, ",
                    env!("CARGO_PKG_VERSION"),
                    ")"
                ))
                .build()
                .unwrap_or_default(),
            token: token.into(),
        }
    }

    /// The client with the timeout and User-Agent, for downloads that need no token.
    pub fn client(&self) -> &reqwest::Client {
        &self.http
    }

    async fn send(&self, req: reqwest::RequestBuilder, body: Option<Value>) -> Result<Value> {
        self.send_tries(req, body, MAX_RETRIES).await
    }

    /// `send` with at most `tries` attempts; the last 429 fails at once instead of waiting.
    async fn send_tries(
        &self,
        req: reqwest::RequestBuilder,
        body: Option<Value>,
        tries: u32,
    ) -> Result<Value> {
        let req = req.header("Authorization", format!("Bot {}", self.token));
        let req = if let Some(b) = body {
            req.json(&b)
        } else {
            req
        };
        // On 429 wait what Discord says and retry; a few tries, then give up with the error.
        for n in 1..=tries {
            let res = req
                .try_clone()
                .expect("json body is cloneable")
                .send()
                .await?;
            let status = res.status();
            let text = res.text().await?;
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if n == tries {
                    break;
                }
                tokio::time::sleep(retry_after(&text)).await;
                continue;
            }
            if !status.is_success() {
                bail!("discord {status}: {text}");
            }
            return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
        }
        bail!("discord rate limit: still limited after {tries} tries")
    }

    /// Pause invites and DMs between members until `until_ms` (unix ms, clamped to 24h from
    /// `now_ms`), or lift both pauses with `None`.
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

    /// The raw `public_flags` of a user, with the bits twilight drops (see [`crate::join::SPAMMER_FLAG`]).
    /// One try, no wait on a 429: the verify button has to answer within 3 seconds.
    pub async fn public_flags(&self, user: u64) -> Result<u64> {
        let v = self
            .send_tries(self.http.get(format!("{API}/users/{user}")), None, 1)
            .await?;
        Ok(v["public_flags"].as_u64().unwrap_or(0))
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
    pub async fn sync_keyword_rule(
        &self,
        guild: u64,
        words: &[String],
        regex: &[String],
    ) -> Result<()> {
        let rules = self.list_rules(guild).await?;
        let body = keyword_rule_body(words, regex);
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
            None if words.is_empty() && regex.is_empty() => return Ok(()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BotAction {
    Report,
    Warn,
    Timeout,
    Kick,
    Ban,
    Lock,
}

/// "Mention Scope" for a mention rule: the bot only runs the rule's actions once a member
/// (`user`) or the whole server (`server`) hit the rule `triggers` times within `duration_s`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct MentionScope {
    pub scope: String,
    pub triggers: u32,
    pub duration_s: u32,
}

impl Default for MentionScope {
    fn default() -> Self {
        Self {
            scope: "user".into(),
            triggers: 1,
            duration_s: 5,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DiscordAutomodCfg {
    pub actions: std::collections::HashMap<String, Vec<BotAction>>,
    /// Rule id -> its mention scope.
    pub scopes: std::collections::HashMap<String, MentionScope>,
    /// Rule id -> lock submode of its Lock action ("channel", "server", "kick", "ban", "invites");
    /// missing means the server-wide lockdown kind.
    pub locks: std::collections::HashMap<String, String>,
}

/// Records a rule hit at `now` (seconds) and says whether the scope's trigger count is reached;
/// the hits are cleared when it is.
pub fn scope_tripped(hits: &mut Vec<i64>, now: i64, s: &MentionScope) -> bool {
    hits.retain(|t| now - t < i64::from(s.duration_s));
    hits.push(now);
    let tripped = hits.len() >= s.triggers.max(1) as usize;
    if tripped {
        hits.clear();
    }
    tripped
}

#[derive(Debug, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct RuleSpec {
    pub name: String,
    pub enabled: bool,
    pub trigger: String,
    pub keywords: Vec<String>,
    pub allow: Vec<String>,
    /// 1 profanity, 2 sexual content, 3 slurs.
    pub presets: Vec<u8>,
    pub mention_limit: u32,
    /// Mention rules: "discord" (Discord's mention raid detection), "user" or "server" (bot-side
    /// scope), anything else a plain per-message limit.
    pub scope: String,
    pub scope_triggers: u32,
    pub scope_duration_s: u32,
    pub block_message: bool,
    pub custom_message: String,
    pub alert_channel: String,
    pub timeout_s: u32,
    pub exempt_roles: Vec<String>,
    pub exempt_channels: Vec<String>,
    pub bot_actions: Vec<BotAction>,
    /// Submode of the Lock bot action, see `DiscordAutomodCfg::locks`.
    pub lock: String,
}

impl RuleSpec {
    pub fn from_discord(v: &Value) -> Self {
        let strs = |x: &Value| -> Vec<String> {
            x.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let meta = &v["trigger_metadata"];
        let mut r = Self {
            name: v["name"].as_str().unwrap_or_default().into(),
            enabled: v["enabled"].as_bool().unwrap_or(false),
            trigger: match v["trigger_type"].as_u64() {
                Some(1) => "keyword",
                Some(3) => "spam",
                Some(4) => "preset",
                Some(5) => "mention",
                _ => "other",
            }
            .into(),
            keywords: strs(&meta["keyword_filter"]),
            allow: strs(&meta["allow_list"]),
            presets: meta["presets"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|n| n.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default(),
            mention_limit: meta["mention_total_limit"].as_u64().unwrap_or(0) as u32,
            scope: if meta["mention_raid_protection_enabled"].as_bool() == Some(true) {
                "discord".into()
            } else {
                String::new()
            },
            exempt_roles: strs(&v["exempt_roles"]),
            exempt_channels: strs(&v["exempt_channels"]),
            ..Default::default()
        };
        for a in v["actions"].as_array().into_iter().flatten() {
            let m = &a["metadata"];
            match a["type"].as_u64() {
                Some(1) => {
                    r.block_message = true;
                    r.custom_message = m["custom_message"].as_str().unwrap_or_default().into();
                }
                Some(2) => r.alert_channel = m["channel_id"].as_str().unwrap_or_default().into(),
                Some(3) => r.timeout_s = m["duration_seconds"].as_u64().unwrap_or(0) as u32,
                _ => {}
            }
        }
        r
    }
}

/// Discord limits: custom block message 150 chars, timeout 4 weeks, mention limit 50 (min 1).
pub fn rule_body(r: &RuleSpec) -> Result<Value> {
    let (trigger_type, meta) = match r.trigger.as_str() {
        "keyword" => (
            1,
            json!({ "keyword_filter": r.keywords.iter().filter(|w| !w.is_empty() && w.chars().count() <= MAX_KEYWORD_LEN).take(MAX_KEYWORDS).collect::<Vec<_>>(),
                    "allow_list": r.allow.iter().take(100).collect::<Vec<_>>() }),
        ),
        "spam" => (3, json!({})),
        "preset" => (
            4,
            json!({ "presets": r.presets, "allow_list": r.allow.iter().take(1000).collect::<Vec<_>>() }),
        ),
        "mention" => (
            5,
            json!({ "mention_total_limit": r.mention_limit.clamp(1, 50),
                    "mention_raid_protection_enabled": r.scope == "discord" }),
        ),
        other => bail!("unknown trigger {other:?}"),
    };
    if r.name.trim().is_empty() {
        bail!("rule needs a name");
    }
    let mut actions = vec![];
    if r.block_message {
        let msg: String = r.custom_message.chars().take(150).collect();
        actions.push(json!({ "type": 1, "metadata": { "custom_message": msg } }));
    }
    if !r.alert_channel.is_empty() {
        actions.push(json!({ "type": 2, "metadata": { "channel_id": r.alert_channel } }));
    }
    // Discord rejects timeouts on the spam and preset triggers.
    if r.timeout_s > 0 && matches!(trigger_type, 1 | 5) {
        actions.push(
            json!({ "type": 3, "metadata": { "duration_seconds": r.timeout_s.min(2_419_200) } }),
        );
    }
    if actions.is_empty() {
        bail!("rule needs at least one action");
    }
    if !r.bot_actions.is_empty() && !r.block_message {
        bail!("bot actions need the block message action");
    }
    Ok(json!({
        "name": r.name,
        "event_type": 1,
        "trigger_type": trigger_type,
        "trigger_metadata": meta,
        "actions": actions,
        "enabled": r.enabled,
        "exempt_roles": r.exempt_roles.iter().take(20).collect::<Vec<_>>(),
        "exempt_channels": r.exempt_channels.iter().take(50).collect::<Vec<_>>(),
    }))
}

/// `until` is `(now_ms, until_ms)`.
fn invites_body(until: Option<(i64, i64)>) -> Value {
    let until = until.map(|(now, until)| {
        let ms = until.min(now + MAX_INVITE_PAUSE_MS);
        chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339())
    });
    let until = until.flatten();
    json!({ "invites_disabled_until": until, "dms_disabled_until": until })
}

/// Which entries of `list` our keyword rule sends to Discord: non-empty, within `len` chars, the first `max`.
fn fits(list: &[String], len: usize, max: usize) -> Vec<bool> {
    let mut n = 0;
    list.iter()
        .map(|w| {
            let ok = !w.is_empty() && w.chars().count() <= len && n < max;
            n += usize::from(ok);
            ok
        })
        .collect()
}

/// Per word and per regex pattern: true when it fits in Discord's keyword rule. The rest stays
/// with the local filter.
pub fn on_discord(words: &[String], regex: &[String]) -> (Vec<bool>, Vec<bool>) {
    (
        fits(words, MAX_KEYWORD_LEN, MAX_KEYWORDS),
        fits(regex, MAX_REGEX_LEN, MAX_REGEX),
    )
}

fn keyword_rule_body(words: &[String], regex: &[String]) -> Value {
    let fit = |list: &[String], ok: Vec<bool>| -> Vec<String> {
        list.iter()
            .zip(ok)
            .filter(|(_, ok)| *ok)
            .map(|(w, _)| w.clone())
            .collect()
    };
    let (w_ok, r_ok) = on_discord(words, regex);
    let words = fit(words, w_ok);
    let regex = fit(regex, r_ok);
    json!({
        "name": KEYWORD_RULE,
        "event_type": 1,
        "trigger_type": 1,
        "trigger_metadata": { "keyword_filter": words, "regex_patterns": regex },
        "actions": [{ "type": 1 }],
        "enabled": !words.is_empty() || !regex.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves one raw HTTP answer on a local port and fetches it.
    async fn fetch(head: &str, body: Vec<u8>) -> Result<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap());
        let head = head.to_string();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(head.as_bytes()).await;
            let _ = s.write_all(&body).await;
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        image_data_uri(http.get(url).send().await?).await
    }

    #[tokio::test]
    async fn icon_download_takes_only_small_images() {
        let ok = "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: 3\r\n\r\n";
        assert_eq!(
            fetch(ok, b"abc".to_vec()).await.unwrap(),
            "data:image/png;base64,YWJj"
        );
        let html = "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 3\r\n\r\n";
        assert!(fetch(html, b"abc".to_vec()).await.is_err());
        let odd = "HTTP/1.1 200 OK\r\ncontent-type: image/png,x\r\ncontent-length: 3\r\n\r\n";
        assert!(fetch(odd, b"abc".to_vec()).await.is_err());
        let gone = "HTTP/1.1 404 Not Found\r\ncontent-type: image/png\r\ncontent-length: 3\r\n\r\n";
        assert!(fetch(gone, b"abc".to_vec()).await.is_err());
        let big = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: {}\r\n\r\n",
            ICON_MAX + 1
        );
        assert!(fetch(&big, vec![]).await.is_err());
        // No length given: the cap holds while reading.
        let open = "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\nconnection: close\r\n\r\n";
        assert!(fetch(open, vec![0; ICON_MAX + 1]).await.is_err());
    }

    #[tokio::test]
    async fn icon_download_refuses_a_bad_hash_before_any_request() {
        let e = icon_data_uri(&reqwest::Client::new(), 1, "../x")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not a Discord icon hash"));
    }

    #[test]
    fn old_configs_without_locks_still_load() {
        let c: DiscordAutomodCfg =
            serde_json::from_value(json!({"actions": {"1": ["lock"]}})).unwrap();
        assert!(c.locks.is_empty());
        let c: DiscordAutomodCfg = serde_json::from_value(json!({"locks": {"1": "ban"}})).unwrap();
        assert_eq!(c.locks["1"], "ban");
    }

    #[test]
    fn mention_scope_counts_triggers_in_the_window() {
        let s = MentionScope {
            scope: "user".into(),
            triggers: 2,
            duration_s: 5,
        };
        let mut hits = vec![];
        assert!(!scope_tripped(&mut hits, 100, &s));
        assert!(!scope_tripped(&mut hits, 106, &s)); // first hit expired
        assert!(scope_tripped(&mut hits, 108, &s));
        assert!(hits.is_empty());
    }

    #[test]
    fn retry_after_reads_the_body_and_is_capped() {
        assert_eq!(
            retry_after(r#"{"retry_after":0.5}"#),
            Duration::from_millis(500)
        );
        assert_eq!(retry_after("<html>"), Duration::from_secs(1));
        assert_eq!(retry_after(r#"{"retry_after":600}"#), MAX_WAIT);
    }

    #[test]
    fn invite_pause_is_capped_and_lifted_with_null() {
        let v = invites_body(Some((0, 10 * MAX_INVITE_PAUSE_MS)));
        assert_eq!(v["invites_disabled_until"], "1970-01-02T00:00:00+00:00");
        assert_eq!(v["dms_disabled_until"], v["invites_disabled_until"]);
        assert!(invites_body(None)["invites_disabled_until"].is_null());
        assert!(invites_body(None)["dms_disabled_until"].is_null());
    }

    #[test]
    fn rule_body_builds_each_trigger_and_rejects_empty_rules() {
        let mut r = RuleSpec {
            name: "n".into(),
            trigger: "mention".into(),
            mention_limit: 99,
            timeout_s: 60,
            block_message: true,
            ..Default::default()
        };
        let v = rule_body(&r).unwrap();
        assert_eq!(v["trigger_metadata"]["mention_total_limit"], 50);
        assert_eq!(v["actions"].as_array().unwrap().len(), 2);
        let back = RuleSpec::from_discord(&v);
        assert_eq!(
            (back.trigger.as_str(), back.mention_limit, back.timeout_s),
            ("mention", 50, 60)
        );
        r.timeout_s = 0;
        r.trigger = "spam".into();
        assert_eq!(
            rule_body(&r).unwrap()["actions"].as_array().unwrap().len(),
            1
        );
        r.bot_actions = vec![BotAction::Warn];
        assert!(rule_body(&r).is_ok());
        r.block_message = false;
        r.alert_channel = "1".into();
        assert!(rule_body(&r).is_err(), "bot actions need block message");
        r.alert_channel.clear();
        r.bot_actions.clear();
        assert!(rule_body(&r).is_err(), "no actions");
        r.trigger = "nope".into();
        assert!(rule_body(&r).is_err());
    }

    #[test]
    fn keyword_rule_drops_bad_keywords_and_disables_when_empty() {
        let long = "x".repeat(61);
        let v = keyword_rule_body(&["bad".into(), "".into(), long], &[]);
        assert_eq!(v["trigger_metadata"]["keyword_filter"], json!(["bad"]));
        assert_eq!(v["enabled"], true);
        assert_eq!(keyword_rule_body(&[], &[])["enabled"], false);
        let r = keyword_rule_body(&[], &["b[a4]d".into()]);
        assert_eq!(r["trigger_metadata"]["regex_patterns"], json!(["b[a4]d"]));
        assert_eq!(r["enabled"], true);
    }
}

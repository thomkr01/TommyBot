use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};
use tommybot_automod::lockdown::VIEW_CHANNEL;

/// How long owner and member lookups are reused, so page loads do not drain the bot's rate limit.
const MEMO_TTL: Duration = Duration::from_secs(30);
const ADMINISTRATOR: u64 = 1 << 3;

#[derive(Clone)]
pub struct Discord {
    pub http: reqwest::Client,
    pub client_id: String,
    pub client_secret: String,
    pub bot_token: String,
    pub redirect_uri: String,
    /// Overridable (`DISCORD_API_BASE`) so the e2e script can point it at a mock.
    pub api: String,
    /// Bot GETs by path, with when they were fetched (None: 404).
    pub memo: crate::Shared<String, (Instant, Option<serde_json::Value>)>,
}

#[derive(Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
}

#[derive(Deserialize, serde::Serialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

fn app_owner(app: Option<&serde_json::Value>) -> Option<String> {
    let app = app?;
    app["team"]["owner_user_id"]
        .as_str()
        .or(app["owner"]["id"].as_str())
        .map(String::from)
}

impl Discord {
    pub fn authorize_url(&self, state: &str) -> String {
        format!(
            "https://discord.com/oauth2/authorize?response_type=code&client_id={}&scope=identify%20guilds&state={state}&redirect_uri={}",
            self.client_id,
            urlencode(&self.redirect_uri)
        )
    }

    pub async fn exchange(&self, code: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct T {
            access_token: String,
        }
        let t: T = self
            .http
            .post(format!("{}/oauth2/token", self.api))
            .form(&[
                ("client_id", &self.client_id[..]),
                ("client_secret", &self.client_secret),
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.redirect_uri),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(t.access_token)
    }

    pub async fn me(&self, token: &str) -> Result<User> {
        Ok(self
            .http
            .get(format!("{}/users/@me", self.api))
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn user_guilds(&self, token: &str) -> Result<Vec<Guild>> {
        Ok(self
            .http
            .get(format!("{}/users/@me/guilds", self.api))
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Ids of guilds the bot is in, paged 200 at a time.
    pub async fn bot_guild_ids(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = vec![];
        loop {
            let after = out
                .last()
                .map_or(String::new(), |id| format!("&after={id}"));
            let page: Vec<Guild> = self
                .bot(format!("/users/@me/guilds?limit=200{after}"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let n = page.len();
            out.extend(page.into_iter().map(|g| g.id));
            if n < 200 {
                return Ok(out);
            }
        }
    }

    /// Loads the custom emoji ids, as the bot does, for the messages the dashboard sends.
    pub async fn load_emojis(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct E {
            name: String,
            id: String,
        }
        #[derive(Deserialize)]
        struct List {
            items: Vec<E>,
        }
        let list: List = self
            .bot(format!("/applications/{}/emojis", self.client_id))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        tommybot_logging::emoji::load(
            list.items
                .into_iter()
                .filter_map(|e| Some((e.name, e.id.parse().ok()?))),
        );
        Ok(())
    }

    fn bot(&self, path: String) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.api))
            .header("Authorization", format!("Bot {}", self.bot_token))
    }

    /// A bot GET reused for [`MEMO_TTL`]; None when Discord answers 404.
    async fn memo_get(&self, path: String) -> Result<Option<serde_json::Value>> {
        let hit = self.memo.lock().unwrap().get(&path).cloned();
        if let Some((_, v)) = hit.filter(|(at, _)| at.elapsed() < MEMO_TTL) {
            return Ok(v);
        }
        let r = self.bot(path.clone()).send().await?;
        let v = if r.status() == 404 {
            None
        } else {
            Some(r.error_for_status()?.json().await?)
        };
        let mut m = self.memo.lock().unwrap();
        // shortcut: prunes the whole map on insert, fine for one process with a few thousand users.
        m.retain(|_, (at, _)| at.elapsed() < MEMO_TTL);
        m.insert(path, (Instant::now(), v.clone()));
        Ok(v)
    }

    pub async fn guild_owner(&self, guild: i64) -> Result<String> {
        let g = self.memo_get(format!("/guilds/{guild}")).await?;
        g.as_ref()
            .and_then(|g| g["owner_id"].as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("guild {guild} not found"))
    }

    /// The server's name, or None when the bot is not in it.
    pub async fn guild_name(&self, guild: i64) -> Result<Option<String>> {
        let g = self.memo_get(format!("/guilds/{guild}")).await?;
        Ok(g.and_then(|g| g["name"].as_str().map(String::from)))
    }

    /// The server's name and approximate member count, or None when the bot is not in it.
    pub async fn guild_size(&self, guild: i64) -> Result<Option<(String, u64)>> {
        let g = self
            .memo_get(format!("/guilds/{guild}?with_counts=true"))
            .await?;
        Ok(g.map(|g| {
            let name = g["name"].as_str().unwrap_or_default().to_string();
            (name, g["approximate_member_count"].as_u64().unwrap_or(0))
        }))
    }

    /// The bot's own user id.
    pub async fn bot_id(&self) -> Result<String> {
        let me = self.memo_get("/users/@me".into()).await?;
        me.as_ref()
            .and_then(|m| m["id"].as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("bot user not found"))
    }

    /// When the bot joined `guild` (its member's `joined_at`), or None when it is not in it.
    pub async fn bot_joined(&self, guild: i64) -> Result<Option<String>> {
        let id = self.bot_id().await?;
        let m = self
            .memo_get(format!("/guilds/{guild}/members/{id}"))
            .await?;
        Ok(m.and_then(|m| m["joined_at"].as_str().map(String::from)))
    }

    /// A Discord user (name, avatar, bot flag), or None for an unknown id.
    pub async fn user(&self, id: i64) -> Result<Option<serde_json::Value>> {
        self.memo_get(format!("/users/{id}")).await
    }

    /// The user id of the bot application's owner (the team owner for a team app).
    pub async fn bot_owner(&self) -> Result<String> {
        let app = self.memo_get("/applications/@me".into()).await?;
        app_owner(app.as_ref()).ok_or_else(|| anyhow::anyhow!("application has no owner"))
    }

    /// The bot's own user id; for older applications it is not the application id.
    pub async fn bot_user(&self) -> Result<i64> {
        let me = self.memo_get("/users/@me".into()).await?;
        me.as_ref()
            .and_then(|u| u["id"].as_str()?.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("bot user not found"))
    }

    /// The member's role ids, or None when they are not in the guild.
    pub async fn member(&self, guild: i64, user: &str) -> Result<Option<Vec<String>>> {
        let m = self
            .memo_get(format!("/guilds/{guild}/members/{user}"))
            .await?;
        Ok(m.map(|m| {
            m["roles"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r.as_str().map(String::from))
                .collect()
        }))
    }

    /// Whether `ch` is in `guild` and hidden from @everyone, the bot's rule for log channels.
    pub async fn private_channel_of(&self, guild: i64, ch: &str) -> Result<bool> {
        let c = self
            .call(reqwest::Method::GET, &format!("/channels/{ch}"), None)
            .await?;
        if c["guild_id"].as_str() != Some(&guild.to_string()) {
            return Ok(false);
        }
        let g = self.memo_get(format!("/guilds/{guild}")).await?;
        let base = g
            .as_ref()
            .and_then(|g| g["roles"].as_array())
            .into_iter()
            .flatten()
            .find(|r| r["id"].as_str() == Some(&guild.to_string()))
            .and_then(|r| r["permissions"].as_str()?.parse().ok())
            .unwrap_or(0);
        Ok(!everyone_sees(base, &c, guild))
    }

    pub async fn member_roles(&self, guild: i64, user: &str) -> Result<Vec<String>> {
        Ok(self.member(guild, user).await?.unwrap_or_default())
    }

    /// Bot-authenticated call for the less common endpoints; returns the JSON body (Null when empty).
    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let text = self
            .send(method, path, body)
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    /// DMs `user` plain text with mentions disabled.
    pub async fn dm(&self, user: i64, text: &str) -> Result<()> {
        let ch = self
            .call(
                reqwest::Method::POST,
                "/users/@me/channels",
                Some(json!({"recipient_id": user.to_string()})),
            )
            .await?;
        let ch = ch["id"].as_str().context("no DM channel id")?;
        self.call(
            reqwest::Method::POST,
            &format!("/channels/{ch}/messages"),
            Some(json!({"content": text, "allowed_mentions": {"parse": []}})),
        )
        .await?;
        Ok(())
    }

    /// Like `call` but hands back the raw response, for callers that branch on the status.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response> {
        // A 429 waits the time Discord asks for (at most 10 s) and tries again, up to 3 times.
        let mut tries = 0;
        loop {
            let mut r = self
                .http
                .request(method.clone(), format!("{}{path}", self.api))
                .header("Authorization", format!("Bot {}", self.bot_token));
            if let Some(b) = &body {
                r = r.json(b);
            }
            let res = r.send().await?;
            if res.status() != reqwest::StatusCode::TOO_MANY_REQUESTS || tries == 3 {
                return Ok(res);
            }
            tries += 1;
            let wait = res
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()?.parse::<f64>().ok())
                .unwrap_or(1.0)
                .clamp(0.0, 10.0);
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }

    /// Posts a log embed with mentions disabled.
    pub async fn post_log(&self, channel: &str, log: tommybot_logging::Log) -> Result<()> {
        self.post_log_with(channel, log, serde_json::json!([]))
            .await
            .map(drop)
    }

    /// [`Self::post_log`] with message components (raw JSON action rows).
    pub async fn post_log_with(
        &self,
        channel: &str,
        log: tommybot_logging::Log,
        components: serde_json::Value,
    ) -> Result<String> {
        let sent: serde_json::Value = self.http
            .post(format!("{}/channels/{channel}/messages", self.api))
            .header("Authorization", format!("Bot {}", self.bot_token))
            .json(&serde_json::json!({"embeds": [log.build()], "components": components, "allowed_mentions": {"parse": []}}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(sent["id"].as_str().unwrap_or_default().to_string())
    }
}

/// Whether @everyone can read `ch` (a Discord channel object), as the bot's `everyone_sees`.
fn everyone_sees(base: u64, ch: &serde_json::Value, guild: i64) -> bool {
    if base & ADMINISTRATOR != 0 {
        return true;
    }
    let bits = |o: &serde_json::Value, k: &str| {
        o[k].as_str()
            .and_then(|b| b.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let (allow, deny) = ch["permission_overwrites"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["id"].as_str() == Some(&guild.to_string()))
        .map_or((0, 0), |o| (bits(o, "allow"), bits(o, "deny")));
    ((base & !deny) | allow) & VIEW_CHANNEL != 0
}

pub(crate) fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use serde_json::json;

    #[test]
    fn app_owner_prefers_the_team_owner() {
        let solo = json!({"owner": {"id": "1"}});
        let team = json!({"owner": {"id": "9"}, "team": {"owner_user_id": "2"}});
        assert_eq!(app_owner(Some(&solo)).as_deref(), Some("1"));
        assert_eq!(app_owner(Some(&team)).as_deref(), Some("2"));
        assert_eq!(app_owner(None), None);
    }

    #[tokio::test]
    async fn send_retries_after_429() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static HITS: AtomicU32 = AtomicU32::new(0);
        let app = axum::Router::new().route(
            "/x",
            axum::routing::get(|| async {
                if HITS.fetch_add(1, Ordering::SeqCst) < 2 {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0")],
                    )
                        .into_response()
                } else {
                    "{}".into_response()
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        let d = Discord {
            http: reqwest::Client::new(),
            client_id: String::new(),
            client_secret: String::new(),
            bot_token: String::new(),
            redirect_uri: String::new(),
            api,
            memo: Default::default(),
        };
        let res = d.send(reqwest::Method::GET, "/x", None).await.unwrap();
        assert!(res.status().is_success());
        assert_eq!(HITS.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn everyone_sees_follows_base_and_overwrite() {
        let ow = |allow: u64, deny: u64| json!({"permission_overwrites": [{"id": "9", "allow": allow.to_string(), "deny": deny.to_string()}]});
        assert!(everyone_sees(VIEW_CHANNEL, &json!({}), 9));
        assert!(!everyone_sees(VIEW_CHANNEL, &ow(0, VIEW_CHANNEL), 9));
        assert!(!everyone_sees(0, &json!({}), 9));
        assert!(everyone_sees(0, &ow(VIEW_CHANNEL, 0), 9));
        assert!(everyone_sees(ADMINISTRATOR, &ow(0, VIEW_CHANNEL), 9));
    }
}

use anyhow::Result;
use serde::Deserialize;

const API: &str = "https://discord.com/api/v10";

#[derive(Clone)]
pub struct Discord {
    pub http: reqwest::Client,
    pub client_id: String,
    pub client_secret: String,
    pub bot_token: String,
    pub redirect_uri: String,
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
            .post(format!("{API}/oauth2/token"))
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
            .get(format!("{API}/users/@me"))
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
            .get(format!("{API}/users/@me/guilds"))
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

    fn bot(&self, path: String) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{API}{path}"))
            .header("Authorization", format!("Bot {}", self.bot_token))
    }

    pub async fn guild_owner(&self, guild: i64) -> Result<String> {
        #[derive(Deserialize)]
        struct G {
            owner_id: String,
        }
        let g: G = self
            .bot(format!("/guilds/{guild}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(g.owner_id)
    }

    pub async fn member_roles(&self, guild: i64, user: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct M {
            roles: Vec<String>,
        }
        let r = self
            .bot(format!("/guilds/{guild}/members/{user}"))
            .send()
            .await?;
        if r.status() == 404 {
            return Ok(vec![]);
        }
        Ok(r.error_for_status()?.json::<M>().await?.roles)
    }

    /// Bot-authenticated call for the less common endpoints; returns the JSON body (Null when empty).
    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let mut r = self
            .http
            .request(method, format!("{API}{path}"))
            .header("Authorization", format!("Bot {}", self.bot_token));
        if let Some(b) = body {
            r = r.json(&b);
        }
        let text = r.send().await?.error_for_status()?.text().await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    pub async fn post_message(&self, channel: &str, content: &str) -> Result<()> {
        self.http
            .post(format!("{API}/channels/{channel}/messages"))
            .header("Authorization", format!("Bot {}", self.bot_token))
            .json(&serde_json::json!({"content": content, "allowed_mentions": {"parse": []}}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

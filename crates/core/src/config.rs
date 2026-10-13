use anyhow::{Context, Result};

pub struct Config {
    pub discord_token: String,
    pub database_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            discord_token: std::env::var("DISCORD_TOKEN").context("DISCORD_TOKEN not set")?,
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL not set")?,
        })
    }
}

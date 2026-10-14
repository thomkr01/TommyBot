//! Phishing and NSFW domain blocklists (downloaded, refreshed on a timer) and shortener unrolling.
use std::{collections::HashSet, sync::RwLock, time::Duration};

const PHISHING_URL: &str =
    "https://raw.githubusercontent.com/blocklistproject/Lists/master/alt-version/phishing-nl.txt";
const NSFW_URL: &str =
    "https://raw.githubusercontent.com/blocklistproject/Lists/master/alt-version/porn-nl.txt";
pub const REFRESH: Duration = Duration::from_secs(12 * 3600);

/// Bare domains. A host hits when it or any parent domain is listed.
#[derive(Default)]
pub struct SiteList(HashSet<String>);

impl SiteList {
    /// One domain per line; `#` comments and hosts-file lines (`0.0.0.0 domain`) are tolerated.
    pub fn parse(text: &str) -> Self {
        Self(
            text.lines()
                .filter_map(|l| l.split('#').next()?.split_whitespace().last())
                .filter(|d| d.contains('.'))
                .map(str::to_lowercase)
                .collect(),
        )
    }

    pub fn hits(&self, hosts: &[String]) -> bool {
        hosts.iter().any(|h| {
            let mut d = h.as_str();
            loop {
                if self.0.contains(d) {
                    return true;
                }
                match d.split_once('.') {
                    Some((_, rest)) if rest.contains('.') => d = rest,
                    _ => return false,
                }
            }
        })
    }
}

pub struct Sites {
    phishing: RwLock<SiteList>,
    nsfw: RwLock<SiteList>,
    http: reqwest::Client,
}

impl Default for Sites {
    fn default() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            phishing: Default::default(),
            nsfw: Default::default(),
            http,
        }
    }
}

impl Sites {
    pub fn phishing(&self, hosts: &[String]) -> bool {
        self.phishing.read().unwrap().hits(hosts)
    }

    pub fn nsfw(&self, hosts: &[String]) -> bool {
        self.nsfw.read().unwrap().hits(hosts)
    }

    /// Re-downloads both lists. `PHISHING_LIST_URL` / `NSFW_LIST_URL` override the sources, empty disables one.
    /// A failed download keeps the old list.
    pub async fn refresh(&self) {
        for (var, default, slot) in [
            ("PHISHING_LIST_URL", PHISHING_URL, &self.phishing),
            ("NSFW_LIST_URL", NSFW_URL, &self.nsfw),
        ] {
            let url = std::env::var(var).unwrap_or_else(|_| default.into());
            if url.is_empty() {
                continue;
            }
            match self.download(&url).await {
                Ok(list) => *slot.write().unwrap() = list,
                Err(e) => tracing::warn!(?e, var, "refreshing a site list failed"),
            }
        }
    }

    async fn download(&self, url: &str) -> reqwest::Result<SiteList> {
        let text = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(SiteList::parse(&text))
    }

    /// Host a shortener link ends up on (redirects followed, only the headers are read).
    pub async fn unshorten(&self, url: &str) -> Option<String> {
        let res = self
            .http
            .get(url)
            .timeout(Duration::from_secs(4))
            .send()
            .await
            .ok()?;
        let host = res.url().host_str()?.to_lowercase();
        Some(host.trim_start_matches("www.").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_match_parent_domains() {
        let l = SiteList::parse("# c\nevil.com\n0.0.0.0 bad.example.org # x\nlocalhost\n");
        let h = |s: &str| vec![s.to_string()];
        assert!(l.hits(&h("evil.com")) && l.hits(&h("a.b.evil.com")));
        assert!(l.hits(&h("bad.example.org")) && !l.hits(&h("example.org")));
        assert!(!l.hits(&h("notevil.com")) && !l.hits(&h("com")));
    }
}

//! Phishing and NSFW domain blocklists (downloaded, refreshed on a timer) and shortener unrolling.
use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    redirect::Policy,
};
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::RwLock,
    time::Duration,
};

const PHISHING_URL: &str =
    "https://raw.githubusercontent.com/blocklistproject/Lists/master/alt-version/phishing-nl.txt";
const NSFW_URL: &str =
    "https://raw.githubusercontent.com/blocklistproject/Lists/master/alt-version/porn-nl.txt";
pub const REFRESH: Duration = Duration::from_secs(12 * 3600);
/// Largest list download (site and IP lists).
pub(crate) const LIST_MAX_BYTES: usize = 20 << 20;

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

/// Not loopback, private, link-local, CGNAT, multicast or unspecified (v4-mapped v6 judged as v4).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (o[0] == 100 && o[1] & 0xc0 == 64))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public(v4.into()),
            None => {
                let first = v6.segments()[0];
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || first & 0xfe00 == 0xfc00
                    || first & 0xffc0 == 0xfe80)
            }
        },
    }
}

/// Resolves names but only to public addresses, so no redirect hop can reach the internal network.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("no public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// IP literals skip the resolver, so redirects to them are checked here.
fn follow(attempt: reqwest::redirect::Attempt) -> reqwest::redirect::Action {
    let literal = attempt
        .url()
        .host_str()
        .map(|h| h.trim_matches(['[', ']']))
        .and_then(|h| h.parse::<IpAddr>().ok());
    if attempt.previous().len() >= 5 || literal.is_some_and(|ip| !is_public(ip)) {
        attempt.stop()
    } else {
        attempt.follow()
    }
}

/// A refreshed list with under half the entries of the one it replaces is refused.
pub(crate) fn shrank(old: usize, new: usize) -> bool {
    new * 2 < old
}

/// The body, refused as soon as it passes `max` bytes (a missing or wrong Content-Length included).
pub async fn read_capped(mut r: reqwest::Response, max: usize) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        r.content_length().is_none_or(|l| l <= max as u64),
        "body over {max} bytes"
    );
    let mut buf = Vec::new();
    while let Some(c) = r.chunk().await? {
        anyhow::ensure!(buf.len() + c.len() <= max, "body over {max} bytes");
        buf.extend_from_slice(&c);
    }
    Ok(buf)
}

pub struct Sites {
    phishing: RwLock<SiteList>,
    nsfw: RwLock<SiteList>,
    http: reqwest::Client,
}

/// Client for list downloads and fetches: public addresses only, at most 5 redirects.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .dns_resolver(std::sync::Arc::new(PublicOnly))
        .redirect(Policy::custom(follow))
        .build()
        .unwrap_or_default()
}

impl Default for Sites {
    fn default() -> Self {
        Self {
            phishing: Default::default(),
            nsfw: Default::default(),
            http: client(),
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
                // An empty or garbled page would switch the filter off until the next refresh.
                Ok(list) if shrank(slot.read().unwrap().0.len(), list.0.len()) => tracing::warn!(
                    var,
                    new = list.0.len(),
                    "site list shrank by over half, keeping the old one"
                ),
                Ok(list) => *slot.write().unwrap() = list,
                Err(e) => tracing::warn!(?e, var, "refreshing a site list failed"),
            }
        }
    }

    /// Downloads an attachment for the QR check; `None` past `max` bytes or on any error.
    pub async fn fetch(&self, url: &str, max: usize) -> Option<Vec<u8>> {
        let r = self
            .http
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        read_capped(r, max).await.ok()
    }

    async fn download(&self, url: &str) -> anyhow::Result<SiteList> {
        let r = self.http.get(url).send().await?.error_for_status()?;
        let body = read_capped(r, LIST_MAX_BYTES).await?;
        Ok(SiteList::parse(&String::from_utf8_lossy(&body)))
    }

    /// Host a shortener link ends up on: HEAD, redirects followed (public addresses only), no body read.
    pub async fn unshorten(&self, url: &str) -> Option<String> {
        let res = self
            .http
            .head(url)
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
    fn internal_addresses_are_refused() {
        let ip = |s: &str| is_public(s.parse().unwrap());
        for bad in [
            "127.0.0.1",
            "10.0.0.5",
            "172.17.0.2",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!ip(bad), "{bad}");
        }
        assert!(ip("8.8.8.8") && ip("2606:4700::1111") && ip("::ffff:8.8.8.8"));
    }

    #[test]
    fn a_list_that_lost_over_half_is_refused() {
        assert!(shrank(1000, 0) && shrank(1000, 499));
        assert!(!shrank(1000, 500) && !shrank(0, 0) && !shrank(10, 50));
    }

    #[test]
    fn lists_match_parent_domains() {
        let l = SiteList::parse("# c\nevil.com\n0.0.0.0 bad.example.org # x\nlocalhost\n");
        let h = |s: &str| vec![s.to_string()];
        assert!(l.hits(&h("evil.com")) && l.hits(&h("a.b.evil.com")));
        assert!(l.hits(&h("bad.example.org")) && !l.hits(&h("example.org")));
        assert!(!l.hits(&h("notevil.com")) && !l.hits(&h("com")));
    }
}

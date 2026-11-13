//! Tor exit, VPN and hosting-network IP ranges for web verification (downloaded, refreshed on a timer).
use crate::sites::{client, is_public, read_capped, ShrinkGuard};
use std::{
    net::{IpAddr, Ipv6Addr},
    sync::RwLock,
};

const TOR_URL: &str = "https://check.torproject.org/torbulkexitlist";
const VPN_URLS: &str = "https://raw.githubusercontent.com/X4BNet/lists_vpn/main/output/vpn/ipv4.txt https://raw.githubusercontent.com/X4BNet/lists_vpn/main/output/vpn/ipv6.txt";
const DC_URLS: &str = "https://raw.githubusercontent.com/X4BNet/lists_vpn/main/output/datacenter/ipv4.txt https://raw.githubusercontent.com/X4BNet/lists_vpn/main/output/datacenter/ipv6.txt";
const MAX_BYTES: usize = 20 << 20;
/// A real list has no range this wide; one that does is broken or poisoned and would block whole countries.
const MIN_V4_PREFIX: u8 = 8;
const MIN_V6_PREFIX: u8 = 16;
/// Refuse a list that covers more than a quarter of the IPv4 space.
const MAX_V4_COVER: u64 = 1 << 30;

/// Merged, sorted address ranges (inclusive ends).
#[derive(Default, Debug)]
pub struct IpList {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
}

fn merge<T: Ord + Copy>(mut r: Vec<(T, T)>, next: impl Fn(T) -> Option<T>) -> Vec<(T, T)> {
    r.sort_unstable();
    let mut out: Vec<(T, T)> = Vec::with_capacity(r.len());
    for (s, e) in r {
        match out.last_mut() {
            // Overlapping or touching ranges become one.
            Some(last) if next(last.1).is_none_or(|n| s <= n) => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

fn find<T: Ord + Copy>(r: &[(T, T)], x: T) -> bool {
    let i = r.partition_point(|&(s, _)| s <= x);
    i > 0 && r[i - 1].1 >= x
}

/// v4-mapped IPv6 is judged as IPv4.
fn plain(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

impl IpList {
    /// One address or CIDR per line; `#` comments and blank lines are skipped, unparsable lines too.
    /// Err when a range is wider than /8 (v4) or /16 (v6), or the list covers over a quarter of IPv4.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let (mut v4, mut v6) = (vec![], vec![]);
        for line in text.lines() {
            let l = line.split('#').next().unwrap_or("").trim();
            let (addr, len) = l.split_once('/').unwrap_or((l, ""));
            let Ok(ip) = addr.parse::<IpAddr>() else {
                continue;
            };
            let len: Option<u8> = if len.is_empty() {
                None
            } else {
                len.parse().ok()
            };
            match (ip, len) {
                (IpAddr::V6(a), _) if a.to_ipv4_mapped().is_some() => {
                    let a4 = a.to_ipv4_mapped().unwrap();
                    let l = len.map_or(Some(32), |l| l.checked_sub(96));
                    match l {
                        Some(l) if l <= 32 => push4(&mut v4, a4.into(), l)?,
                        _ => continue,
                    }
                }
                (IpAddr::V4(a), l) => match l.unwrap_or(32) {
                    l if l <= 32 => push4(&mut v4, a.into(), l)?,
                    _ => continue,
                },
                (IpAddr::V6(a), l) => match l.unwrap_or(128) {
                    l if l <= 128 => push6(&mut v6, a, l)?,
                    _ => continue,
                },
            }
        }
        let list = Self {
            v4: merge(v4, |x: u32| x.checked_add(1)),
            v6: merge(v6, |x: u128| x.checked_add(1)),
        };
        let cover: u64 = list.v4.iter().map(|&(s, e)| u64::from(e - s) + 1).sum();
        if cover > MAX_V4_COVER {
            return Err("list covers over a quarter of IPv4");
        }
        Ok(list)
    }

    pub fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match plain(ip) {
            IpAddr::V4(a) => find(&self.v4, u32::from(a)),
            IpAddr::V6(a) => find(&self.v6, u128::from(a)),
        }
    }
}

fn push4(out: &mut Vec<(u32, u32)>, a: u32, len: u8) -> Result<(), &'static str> {
    if len < MIN_V4_PREFIX {
        return Err("IPv4 range wider than /8");
    }
    let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
    out.push((a & mask, a | !mask));
    Ok(())
}

fn push6(out: &mut Vec<(u128, u128)>, a: Ipv6Addr, len: u8) -> Result<(), &'static str> {
    if len < MIN_V6_PREFIX {
        return Err("IPv6 range wider than /16");
    }
    let a = u128::from(a);
    let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
    out.push((a & mask, a | !mask));
    Ok(())
}

pub struct IpLists {
    tor: RwLock<IpList>,
    vpn: RwLock<IpList>,
    hosting: RwLock<IpList>,
    http: reqwest::Client,
    shrink: ShrinkGuard,
}

impl Default for IpLists {
    fn default() -> Self {
        Self {
            tor: Default::default(),
            vpn: Default::default(),
            hosting: Default::default(),
            http: client(),
            shrink: Default::default(),
        }
    }
}

impl IpLists {
    /// Which list a public address is on: "Tor", "VPN", or "hosting network" (only when `hosting`).
    pub fn verdict(&self, ip: IpAddr, hosting: bool) -> Option<&'static str> {
        if !is_public(ip) {
            return None;
        }
        if self.tor.read().unwrap().contains(ip) {
            Some("Tor")
        } else if self.vpn.read().unwrap().contains(ip) {
            Some("VPN")
        } else if hosting && self.hosting.read().unwrap().contains(ip) {
            Some("hosting network")
        } else {
            None
        }
    }

    /// False until a first download worked (or when every list is turned off).
    pub fn loaded(&self) -> bool {
        !self.tor.read().unwrap().is_empty() || !self.vpn.read().unwrap().is_empty()
    }

    /// Re-downloads every list. `TOR_LIST_URL` / `VPN_LIST_URL` / `HOSTING_LIST_URL` (space-separated URLs)
    /// override the sources, empty disables one. A failed, broken or much smaller download keeps the old list.
    /// True when every list that is on holds entries afterwards.
    pub async fn refresh(&self) -> bool {
        let slots = [
            ("TOR_LIST_URL", TOR_URL, &self.tor),
            ("VPN_LIST_URL", VPN_URLS, &self.vpn),
            ("HOSTING_LIST_URL", DC_URLS, &self.hosting),
        ];
        let urls = |var, default: &str| std::env::var(var).unwrap_or_else(|_| default.into());
        for (var, default, slot) in slots {
            let urls = urls(var, default);
            if urls.trim().is_empty() {
                *slot.write().unwrap() = IpList::default();
                continue;
            }
            let mut text = String::new();
            let mut failed = None;
            for url in urls.split_whitespace() {
                match self.download(url).await {
                    Ok(t) => {
                        text.push_str(&t);
                        text.push('\n');
                    }
                    Err(e) => failed = Some(e),
                }
            }
            if let Some(e) = failed {
                tracing::warn!(
                    ?e,
                    var,
                    "downloading an IP list failed, keeping the old one"
                );
                continue;
            }
            match IpList::parse(&text) {
                Ok(new) => {
                    let old = slot.read().unwrap().len();
                    if self.shrink.accept(var, old, new.len()) {
                        *slot.write().unwrap() = new;
                    }
                }
                Err(e) => tracing::warn!(e, var, "IP list refused, keeping the old one"),
            }
        }
        // `loaded()` is true once any list is, so say which one is still missing.
        let mut all = true;
        for (var, default, slot) in slots {
            if !urls(var, default).trim().is_empty() && slot.read().unwrap().is_empty() {
                tracing::warn!(var, "IP list is empty, addresses on it are let through");
                all = false;
            }
        }
        all
    }

    async fn download(&self, url: &str) -> anyhow::Result<String> {
        let r = self.http.get(url).send().await?.error_for_status()?;
        let buf = read_capped(r, MAX_BYTES).await?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Lists from text instead of a download (tests and tools).
    pub fn from_texts(tor: &str, vpn: &str, hosting: &str) -> Result<Self, &'static str> {
        let l = Self::default();
        *l.tor.write().unwrap() = IpList::parse(tor)?;
        *l.vpn.write().unwrap() = IpList::parse(vpn)?;
        *l.hosting.write().unwrap() = IpList::parse(hosting)?;
        Ok(l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_cidrs_addresses_and_skips_junk() {
        let l = IpList::parse(
            "# head\n2.26.8.0/22\n\n1.2.3.4 # tor\nnot an ip\n5.5.5.5/40\n2001:db8::/32\n",
        )
        .unwrap();
        assert!(l.contains(ip("2.26.8.0")) && l.contains(ip("2.26.11.255")));
        assert!(!l.contains(ip("2.26.7.255")) && !l.contains(ip("2.26.12.0")));
        assert!(l.contains(ip("1.2.3.4")) && !l.contains(ip("1.2.3.5")));
        assert!(!l.contains(ip("5.5.5.5")), "bad prefix line is skipped");
        assert!(l.contains(ip("2001:db8:ffff::1")) && !l.contains(ip("2001:db9::")));
        assert!(
            l.contains(ip("::ffff:1.2.3.4")),
            "v4-mapped v6 is judged as v4"
        );
    }

    #[test]
    fn merges_overlapping_and_touching_ranges() {
        let l = IpList::parse(
            "10.0.0.0/24\n10.0.1.0/24\n10.0.0.128/25\n10.0.5.0/24\n255.255.255.255\n",
        )
        .unwrap();
        assert_eq!(l.v4.len(), 3);
        assert!(l.contains(ip("10.0.1.255")) && !l.contains(ip("10.0.2.0")));
        assert!(l.contains(ip("255.255.255.255")));
        let full = IpList::parse("1.2.3.4/32\n9.0.0.0/8\n").unwrap();
        assert!(full.contains(ip("9.255.255.255")) && !full.contains(ip("10.0.0.0")));
    }

    #[test]
    fn refuses_poisoned_lists() {
        assert!(IpList::parse("0.0.0.0/0\n").is_err());
        assert!(IpList::parse("8.0.0.0/7\n").is_err());
        assert!(IpList::parse("2000::/3\n").is_err());
        let quarter: String = (0..65).map(|i| format!("{i}.0.0.0/8\n")).collect();
        assert!(IpList::parse(&quarter).is_err());
        assert!(IpList::parse(&quarter.lines().take(60).collect::<Vec<_>>().join("\n")).is_ok());
    }

    #[test]
    fn verdict_names_the_list_and_skips_private_addresses() {
        let l =
            IpLists::from_texts("1.1.1.1\n", "2.2.2.0/24\n", "3.3.0.0/16\n10.0.0.0/8\n").unwrap();
        assert_eq!(l.verdict(ip("1.1.1.1"), true), Some("Tor"));
        assert_eq!(l.verdict(ip("2.2.2.9"), false), Some("VPN"));
        assert_eq!(l.verdict(ip("3.3.1.1"), true), Some("hosting network"));
        assert_eq!(l.verdict(ip("3.3.1.1"), false), None);
        assert_eq!(
            l.verdict(ip("10.1.1.1"), true),
            None,
            "private addresses never match"
        );
        assert_eq!(l.verdict(ip("8.8.8.8"), true), None);
        assert!(l.loaded() && !IpLists::default().loaded());
    }
}

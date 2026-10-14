//! Message detectors feeding `MsgFacts`. Pure and config-free: callers pass the lists.

/// Combining marks per visible char above this ratio count as zalgo.
const ZALGO_RATIO: f64 = 0.3;

pub fn is_zalgo(s: &str) -> bool {
    let marks = s.chars().filter(|c| matches!(*c as u32, 0x300..=0x36F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)).count();
    let base = s.chars().count() - marks;
    marks >= 3 && marks as f64 > base.max(1) as f64 * ZALGO_RATIO
}

/// (uppercase, lowercase) letter counts, for the separate case weights.
pub fn case_counts(s: &str) -> (usize, usize) {
    s.chars().fold((0, 0), |(u, l), c| {
        (u + c.is_uppercase() as usize, l + c.is_lowercase() as usize)
    })
}

/// Every http(s) URL in the text, with the wrapping punctuation trimmed.
pub fn urls(s: &str) -> Vec<&str> {
    s.split_whitespace()
        .map(|w| w.trim_matches(|c: char| "<>()[]\"'".contains(c)))
        .filter(|w| w.starts_with("https://") || w.starts_with("http://"))
        .collect()
}

/// Hosts of every http(s) URL in the text, lowercased, without port or `www.`.
pub fn hosts(s: &str) -> Vec<String> {
    urls(s)
        .into_iter()
        .filter_map(|u| {
            let rest = u.split_once("://")?.1;
            let host = rest
                .split(['/', '?', '#'])
                .next()?
                .rsplit('@')
                .next()?
                .split(':')
                .next()?;
            Some(host.to_lowercase().trim_start_matches("www.").to_string())
        })
        .filter(|h| h.contains('.'))
        .collect()
}

/// True when `host` is `domain` or a subdomain of it.
fn on_domain(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|p| p.ends_with('.'))
}

pub fn any_on_domains(hosts: &[String], domains: &[String]) -> bool {
    hosts
        .iter()
        .any(|h| domains.iter().any(|d| on_domain(h, &d.to_lowercase())))
}

pub const SHORTENERS: &[&str] = &[
    "bit.ly",
    "tinyurl.com",
    "t.co",
    "goo.gl",
    "is.gd",
    "cutt.ly",
    "rb.gy",
    "shorturl.at",
    "ow.ly",
];

pub fn has_shortener(hosts: &[String]) -> bool {
    hosts
        .iter()
        .any(|h| SHORTENERS.iter().any(|d| on_domain(h, d)))
}

/// The URLs in the text that point at a known shortener.
pub fn shortener_urls(s: &str) -> Vec<&str> {
    urls(s)
        .into_iter()
        .filter(|u| has_shortener(&hosts(u)))
        .collect()
}

const OFFICIAL: &[&str] = &[
    "discord.com",
    "discord.gg",
    "discordapp.com",
    "discord.media",
    "steampowered.com",
    "steamcommunity.com",
];

/// Nitro/Steam scam links: a lure word in the text with a link that is not an official host,
/// or a lookalike host such as `discorcl-gift.com`.
pub fn is_nitro_steam_scam(s: &str) -> bool {
    let hosts = hosts(s);
    if hosts.is_empty() {
        return false;
    }
    let lower = s.to_lowercase();
    let lure = ["nitro", "steam"].iter().any(|w| lower.contains(w));
    let brand_host = hosts.iter().any(|h| {
        ["discord", "steam", "nitro"].iter().any(|b| h.contains(b))
            && !OFFICIAL.iter().any(|d| on_domain(h, d))
    });
    brand_host
        || (lure
            && hosts
                .iter()
                .any(|h| !OFFICIAL.iter().any(|d| on_domain(h, d))))
}

/// Number of blacklist hits. A `*` term is a wildcard matched per space-separated chunk; a plain term
/// matches whole words only, so "ass" does not hit "class".
pub fn word_hits(s: &str, words: &[String]) -> usize {
    let lower = s.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    words
        .iter()
        .filter(|w| {
            let w = w.to_lowercase();
            if w.contains('*') {
                lower
                    .split_whitespace()
                    .any(|t| crate::join::term_matches(&w, t))
            } else {
                tokens.contains(&w.as_str())
            }
        })
        .count()
}

/// Bundled word lists the dashboard toggles, one word per line, `#` lines are comments.
pub fn premade_words(profane: bool, vulgar: bool, racist: bool) -> Vec<String> {
    [
        (profane, include_str!("../wordlists/profane.txt")),
        (vulgar, include_str!("../wordlists/vulgar.txt")),
        (racist, include_str!("../wordlists/racist.txt")),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .flat_map(|(_, list)| list.lines())
    .map(str::trim)
    .filter(|l| !l.is_empty() && !l.starts_with('#'))
    .map(String::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn zalgo() {
        assert!(is_zalgo(
            "h\u{300}\u{301}\u{302}e\u{303}\u{304}l\u{305}\u{306}lo"
        ));
        assert!(!is_zalgo("héllo wörld"));
        assert!(!is_zalgo("e\u{301}"));
    }

    #[test]
    fn case() {
        assert_eq!(case_counts("Hello WORLD 1"), (6, 4));
    }

    #[test]
    fn url_hosts_and_domains() {
        let h = hosts("see <https://www.Evil.com:8080/x?y> and http://a.b.co/p, not example");
        assert_eq!(h, v(&["evil.com", "a.b.co"]));
        assert!(any_on_domains(&h, &v(&["evil.com"])));
        assert!(any_on_domains(
            &hosts("https://sub.evil.com"),
            &v(&["evil.com"])
        ));
        assert!(!any_on_domains(
            &hosts("https://notevil.com"),
            &v(&["evil.com"])
        ));
        assert!(has_shortener(&hosts("https://bit.ly/abc")));
        assert!(!has_shortener(&hosts("https://example.com")));
        assert_eq!(
            shortener_urls("see <https://bit.ly/x> and https://example.com/y"),
            ["https://bit.ly/x"]
        );
    }

    #[test]
    fn scam_links() {
        assert!(is_nitro_steam_scam(
            "free nitro https://discorcl-gift.com/x"
        ));
        assert!(is_nitro_steam_scam("steam gift https://random.site/x"));
        assert!(is_nitro_steam_scam("https://steamcommunlty.com/login"));
        assert!(!is_nitro_steam_scam("free nitro for all"));
        assert!(!is_nitro_steam_scam("get nitro https://discord.com/nitro"));
        assert!(!is_nitro_steam_scam("check https://example.com"));
    }

    #[test]
    fn blacklist_words() {
        let w = v(&["ass", "bad*word", "x"]);
        assert_eq!(word_hits("what a class act", &w), 0);
        assert_eq!(word_hits("you ASS", &w), 1);
        assert_eq!(word_hits("so badddword here, x", &w), 2);
    }

    #[test]
    fn premade_lists_toggle_and_skip_comments() {
        assert!(premade_words(false, false, false).is_empty());
        let p = premade_words(true, false, true);
        assert!(p.contains(&"shit".to_string()) && !p.contains(&"porn".to_string()));
        assert!(p.iter().all(|w| !w.starts_with('#')));
    }
}

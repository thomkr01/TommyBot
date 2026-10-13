//! Wick-style prefix syntax: `ban @a @b 5d ?r reason ?dm`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub command: String,
    pub targets: Vec<u64>,
    /// Usernames or nicknames to look up, in order.
    pub names: Vec<String>,
    pub duration_s: Option<i64>,
    pub reason: Option<String>,
    pub dm: bool,
}

/// `s` is the message text with the prefix already stripped. Targets are mentions, raw ids, or names (words before the first `?flag`; with commas,
/// each comma-separated chunk is one name so multi-word names work).
pub fn parse(s: &str) -> Option<Parsed> {
    let mut it = s.split_whitespace().peekable();
    let mut p = Parsed {
        command: it.next()?.to_lowercase(),
        ..Default::default()
    };
    let mut name_words: Vec<&str> = vec![];
    let mut flagged = false;
    while let Some(tok) = it.next() {
        flagged |= tok.starts_with('?');
        match tok {
            "?r" => {
                let mut words = vec![];
                while let Some(t) = it.next_if(|t| *t != "?dm") {
                    words.push(t);
                }
                if !words.is_empty() {
                    p.reason = Some(words.join(" "));
                }
            }
            "?dm" => p.dm = true,
            _ => {
                let id = tok
                    .trim_matches(|c: char| !c.is_ascii_digit())
                    .parse::<u64>()
                    .ok();
                if let (Some(id), true) = (
                    id,
                    tok.starts_with("<@") || tok.chars().all(|c| c.is_ascii_digit()),
                ) {
                    p.targets.push(id);
                } else if let Some(d) = tommybot_core::duration::parse(tok) {
                    p.duration_s = Some(d);
                } else if !flagged {
                    name_words.push(tok);
                }
            }
        }
    }
    let joined = name_words.join(" ");
    p.names = if joined.contains(',') {
        joined
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(String::from)
            .collect()
    } else {
        name_words.into_iter().map(String::from).collect()
    };
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets_duration_reason() {
        let p = parse("ban <@!111> 222 5d ?r spamming links ?dm").unwrap();
        assert_eq!(p.command, "ban");
        assert_eq!(p.targets, vec![111, 222]);
        assert_eq!(p.duration_s, Some(432_000));
        assert_eq!(p.reason.as_deref(), Some("spamming links"));
        assert!(p.dm);
    }

    #[test]
    fn names_and_commas() {
        let p = parse("ban alice bob 5d ?r x").unwrap();
        assert_eq!(p.names, vec!["alice", "bob"]);
        let p = parse("kick john smith, <@5>, bob ?r why ok").unwrap();
        assert_eq!(p.names, vec!["john smith", "bob"]);
        assert_eq!(p.targets, vec![5]);
        assert!(parse("ban ?r not a name").unwrap().names.is_empty());
    }

    #[test]
    fn bare_command() {
        assert_eq!(parse("warn 5").unwrap().targets, vec![5]);
        assert!(parse("").is_none());
    }
}

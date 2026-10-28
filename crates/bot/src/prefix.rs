//! Wick-style prefix syntax: `ban @a @b 5d ?r reason ?dm`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub command: String,
    pub targets: Vec<u64>,
    /// Usernames or nicknames to look up, in order.
    pub names: Vec<String>,
    pub duration_s: Option<i64>,
    pub reason: Option<String>,
    /// `?t TYPE` that is not a duration (lock types such as `sc`); lock shortcuts like `slock` set it too.
    pub kind: Option<String>,
    pub flags: Flags,
}

/// Wick's command aliases -> (canonical command, implied `?t` type).
fn alias(cmd: &str) -> (&str, Option<&str>) {
    match cmd {
        "b" => ("ban", None),
        "hb" => ("hackban", None),
        "k" => ("kick", None),
        "mute" | "silence" | "m" | "to" | "tout" | "t" => ("timeout", None),
        "deltimeout" | "unmute" | "um" => ("untimeout", None),
        "q" => ("quarantine", None),
        "uq" | "restore" | "r" => ("unquarantine", None),
        "wa" => ("warn", None),
        "ub" => ("unban", None),
        "lockdown" | "ld" => ("lock", None),
        "lockc" | "clock" => ("lock", Some("c")),
        "serverlock" | "slock" | "locks" | "lockserver" | "lockchannels" => ("lock", Some("sc")),
        "ul" => ("unlock", None),
        "sweep" | "clear" | "p" | "c" => ("purge", None),
        "san" | "sanitise" | "dehoist" => ("sanitize", None),
        "sm" => ("slowmode", None),
        c => (c, None),
    }
}

/// `?dm` tell the target, `?s` softban (ban + unban to purge messages), `?p` short reply.
/// `?h` (hackban) is accepted but needs no code: bans already work on ids that left the server.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Flags {
    pub dm: bool,
    pub soft: bool,
    pub compact: bool,
}

/// `s` is the message text with the prefix already stripped. Targets are mentions, raw ids, or names (words before the first `?flag`; with commas,
/// each comma-separated chunk is one name so multi-word names work).
pub fn parse(s: &str) -> Option<Parsed> {
    let mut it = s.split_whitespace().peekable();
    let word = it.next()?.to_lowercase();
    let (command, kind) = alias(&word);
    let mut p = Parsed {
        command: command.into(),
        kind: kind.map(Into::into),
        ..Default::default()
    };
    let mut name_words: Vec<&str> = vec![];
    let mut flagged = false;
    while let Some(tok) = it.next() {
        flagged |= tok.starts_with('?');
        match tok {
            "?r" => {
                let mut words = vec![];
                while let Some(t) = it.next_if(|t| !matches!(*t, "?dm" | "?s" | "?h" | "?p" | "?t"))
                {
                    words.push(t);
                }
                if !words.is_empty() {
                    p.reason = Some(words.join(" "));
                }
            }
            "?t" => {
                if let Some(v) = it.next_if(|t| !t.starts_with('?')) {
                    match tommybot_core::duration::parse(v) {
                        Some(d) => p.duration_s = Some(d),
                        None => p.kind = Some(v.to_lowercase()),
                    }
                }
            }
            "?dm" => p.flags.dm = true,
            "?s" => p.flags.soft = true,
            "?p" => p.flags.compact = true,
            "?h" => {}
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
        assert!(p.flags.dm);
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
    fn softban_hackban_compact_flags() {
        let p = parse("ban 5 ?r spam ?s ?h ?p").unwrap();
        assert_eq!(p.reason.as_deref(), Some("spam"));
        assert_eq!(
            (p.flags.soft, p.flags.compact, p.flags.dm),
            (true, true, false)
        );
    }

    #[test]
    fn aliases_and_t_flag() {
        assert_eq!(parse("b 5").unwrap().command, "ban");
        assert_eq!(parse("hb 5").unwrap().command, "hackban");
        assert_eq!(parse("deltimeout 5").unwrap().command, "untimeout");
        assert_eq!(parse("r 5").unwrap().command, "unquarantine");
        let p = parse("slock").unwrap();
        assert_eq!(
            (p.command.as_str(), p.kind.as_deref()),
            ("lock", Some("sc"))
        );
        assert_eq!(parse("ld ?t K").unwrap().kind.as_deref(), Some("k"));
        let p = parse("t 5 ?t 15m ?r why").unwrap();
        assert_eq!(
            (p.duration_s, p.reason.as_deref()),
            (Some(900), Some("why"))
        );
        // `?t` ends a reason.
        let p = parse("m 5 ?r too loud ?t 1h").unwrap();
        assert_eq!(
            (p.duration_s, p.reason.as_deref()),
            (Some(3600), Some("too loud"))
        );
    }

    #[test]
    fn bare_command() {
        assert_eq!(parse("warn 5").unwrap().targets, vec![5]);
        assert!(parse("").is_none());
    }
}

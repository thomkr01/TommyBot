//! Wick-style prefix syntax: `ban @a @b 5d ?r reason ?dm`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub command: String,
    pub targets: Vec<u64>,
    pub duration_s: Option<i64>,
    pub reason: Option<String>,
    pub dm: bool,
}

/// `s` is the message text with the prefix already stripped. Targets are mentions or raw ids.
pub fn parse(s: &str) -> Option<Parsed> {
    let mut it = s.split_whitespace().peekable();
    let mut p = Parsed { command: it.next()?.to_lowercase(), ..Default::default() };
    while let Some(tok) = it.next() {
        match tok {
            "?r" => {
                let rest: Vec<&str> = it.by_ref().take_while(|t| *t != "?dm").collect();
                if !rest.is_empty() {
                    p.reason = Some(rest.join(" "));
                }
                // take_while consumed the `?dm` flag if it ended the reason
                p.dm |= s.trim_end().ends_with("?dm");
            }
            "?dm" => p.dm = true,
            _ => {
                let id = tok.trim_matches(|c: char| !c.is_ascii_digit()).parse::<u64>().ok();
                if let (Some(id), true) = (id, tok.starts_with("<@") || tok.chars().all(|c| c.is_ascii_digit())) {
                    p.targets.push(id);
                } else if let Some(d) = tommybot_core::duration::parse(tok) {
                    p.duration_s = Some(d);
                }
            }
        }
    }
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
    fn bare_command() {
        assert_eq!(parse("warn 5").unwrap().targets, vec![5]);
        assert!(parse("").is_none());
    }
}

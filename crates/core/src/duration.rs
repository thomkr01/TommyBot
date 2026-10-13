/// Parses `30s`, `10m`, `2h`, `5d`, `1w` into seconds.
pub fn parse(s: &str) -> Option<i64> {
    let (n, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: i64 = n.parse().ok().filter(|n| *n > 0)?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    n.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses() {
        assert_eq!(super::parse("5d"), Some(432_000));
        assert_eq!(super::parse("10m"), Some(600));
        assert_eq!(super::parse("0m"), None);
        assert_eq!(super::parse("x"), None);
        assert_eq!(super::parse(""), None);
    }
}

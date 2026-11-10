//! Allowed ranges of number settings, shared by the prefix panels, the dashboard form and the API.
use serde_json::Value;

/// Anti-nuke card: minute cap, hour cap and heat % per action.
pub const NUKE_MINUTE: (i64, i64) = (2, 50);
pub const NUKE_HOUR: (i64, i64) = (2, 500);
pub const NUKE_HEAT: (i64, i64) = (1, 100);
pub use tommybot_automod::join::MIN_AGE_MS;

/// The range of the number at `path` (below the section, e.g. `limits.Ban.per_hour`), if it has one.
pub fn for_path(section: &str, path: &str) -> Option<(i64, i64)> {
    match (section, path.split('.').collect::<Vec<_>>().as_slice()) {
        ("antinuke", ["limits", _, "per_minute"]) => Some(NUKE_MINUTE),
        ("antinuke", ["limits", _, "per_hour"]) => Some(NUKE_HOUR),
        ("antinuke", ["limits", _, "heat"]) => Some(NUKE_HEAT),
        ("joingate", ["min_age_override_ms"]) => Some(MIN_AGE_MS),
        _ => None,
    }
}

/// Rejects a section document with a set number outside its range; unset (null) values are fine.
pub fn check(section: &str, doc: &Value) -> Result<(), String> {
    let mut found = vec![];
    match section {
        "antinuke" => {
            for (card, c) in doc["limits"].as_object().into_iter().flatten() {
                for k in ["per_minute", "per_hour", "heat"] {
                    found.push((format!("limits.{card}.{k}"), &c[k]));
                }
            }
        }
        "joingate" => found.push(("min_age_override_ms".into(), &doc["min_age_override_ms"])),
        _ => {}
    }
    for (path, v) in found {
        let (Some((lo, hi)), false) = (for_path(section, &path), v.is_null()) else {
            continue;
        };
        if !v.as_i64().is_some_and(|n| (lo..=hi).contains(&n)) {
            return Err(format!("{path} must be between {lo} and {hi}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ranges_by_path() {
        assert_eq!(for_path("antinuke", "limits.Ban.per_hour"), Some((2, 500)));
        assert_eq!(for_path("antinuke", "limits.Ban.heat"), Some((1, 100)));
        assert_eq!(for_path("antinuke", "per_hour"), None);
        assert_eq!(
            for_path("joingate", "min_age_override_ms"),
            Some((5_000, 8_640_000_000))
        );
    }

    #[test]
    fn check_rejects_out_of_range_numbers_only() {
        let nuke = |k: &str, v: Value| json!({"limits": {"Ban": {k: v}}});
        assert!(check("antinuke", &nuke("per_hour", json!(500))).is_ok());
        assert!(check("antinuke", &nuke("per_hour", json!(501))).is_err());
        assert!(check("antinuke", &nuke("per_hour", json!(1))).is_err());
        assert!(check("antinuke", &nuke("heat", json!(0))).is_err());
        assert!(check("antinuke", &nuke("heat", json!(101))).is_err());
        assert!(check("antinuke", &nuke("per_minute", json!(51))).is_err());
        assert!(check("antinuke", &nuke("per_minute", json!(null))).is_ok());
        assert!(check("antinuke", &nuke("per_hour", json!("9"))).is_err());
        let age = |v: i64| json!({"min_age_override_ms": v});
        assert!(check("joingate", &age(5_000)).is_ok());
        assert!(check("joingate", &age(4_999)).is_err());
        assert!(check("joingate", &age(8_640_000_000)).is_ok());
        assert!(check("joingate", &age(8_640_000_001)).is_err());
        assert!(check("joingate", &json!({})).is_ok());
    }
}

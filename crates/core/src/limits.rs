//! Allowed ranges of number settings, shared by the prefix panels, the dashboard form and the API.
use serde_json::Value;

/// Anti-nuke card: minute cap, hour cap and heat % per action.
pub const NUKE_MINUTE: (i64, i64) = (2, 50);
pub const NUKE_HOUR: (i64, i64) = (2, 500);
pub const NUKE_HEAT: (i64, i64) = (1, 100);
pub use tommybot_automod::heat::{HEAT_DECAY, HEAT_MAX, SLOW_CALM, SLOW_ON, SLOW_PCT, SLOW_SECS};
pub use tommybot_automod::join::MIN_AGE_MS;

/// Auto slowmode card: field and range.
const SLOWMODE: [(&str, (i64, i64)); 4] = [
    ("channel_pct", SLOW_PCT),
    ("slowmode_secs", SLOW_SECS),
    ("min_on_secs", SLOW_ON),
    ("calm_secs", SLOW_CALM),
];
/// Ban Sync: seconds a ban waits before it is shared, so anti-nuke can stop a hacked account first.
pub const BAN_SYNC_HOLD_S: i64 = 60;
/// Ban Sync: bans one server may share per hour before sharing pauses for the owner.
pub const BAN_SYNC_HOURLY: i64 = 30;

/// Raider List: different server owners whose strong detections list a user by themselves.
pub const RAIDER_AUTO_OWNERS: i64 = 3;
/// Raider List reports: per owner per day, and what a reporting server needs.
pub const RAIDER_REPORTS_DAY: i64 = 10;
pub const RAIDER_MIN_DAYS: i64 = 7;
pub const RAIDER_MIN_MEMBERS: u64 = 100;
/// Raider List: days a scam listing lasts after its last evidence.
pub const RAIDER_SCAM_DAYS: i64 = 14;

/// The range of the number at `path` (below the section, e.g. `limits.Ban.per_hour`), if it has one.
pub fn for_path(section: &str, path: &str) -> Option<(i64, i64)> {
    match (section, path.split('.').collect::<Vec<_>>().as_slice()) {
        ("antinuke", ["limits", _, "per_minute"]) => Some(NUKE_MINUTE),
        ("antinuke", ["limits", _, "per_hour"]) => Some(NUKE_HOUR),
        ("antinuke", ["limits", _, "heat"]) => Some(NUKE_HEAT),
        ("joingate", ["min_age_override_ms"]) => Some(MIN_AGE_MS),
        ("automod", ["auto_slowmode", k]) => SLOWMODE.iter().find(|s| s.0 == *k).map(|s| s.1),
        _ => None,
    }
}

/// The range of a fractional number at `path`, if it has one. The bot clamps these too when it reads them.
pub fn float_for_path(section: &str, path: &str) -> Option<(f64, f64)> {
    match (section, path) {
        ("automod", "heat.max") => Some(HEAT_MAX),
        ("automod", "heat.decay_per_sec") | ("panic", "heat_decay") => Some(HEAT_DECAY),
        _ => None,
    }
}

/// Rejects a section document with a set number outside its range. Unset (null) values are fine, and
/// so is a value equal to the one in `old` (saved before the limits; the bot reads it inside them),
/// so an old value never blocks saving an unrelated change.
pub fn check(section: &str, doc: &Value, old: &Value) -> Result<(), String> {
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
        "automod" => {
            for (k, _) in SLOWMODE {
                found.push((format!("auto_slowmode.{k}"), &doc["auto_slowmode"][k]));
            }
            for k in ["max", "decay_per_sec"] {
                found.push((format!("heat.{k}"), &doc["heat"][k]));
            }
        }
        "panic" => found.push(("heat_decay".into(), &doc["heat_decay"])),
        _ => {}
    }
    for (path, v) in found {
        let kept = old.pointer(&format!("/{}", path.replace('.', "/"))) == Some(v);
        if v.is_null() || kept {
            continue;
        }
        let out = match (for_path(section, &path), float_for_path(section, &path)) {
            (Some((lo, hi)), _) => (!v.as_i64().is_some_and(|n| (lo..=hi).contains(&n)))
                .then(|| format!("{lo} and {hi}")),
            (_, Some((lo, hi))) => (!v.as_f64().is_some_and(|n| (lo..=hi).contains(&n)))
                .then(|| format!("{lo} and {hi}")),
            _ => None,
        };
        if let Some(range) = out {
            return Err(format!("{path} must be between {range}"));
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
        assert!(check("antinuke", &nuke("per_hour", json!(500)), &Value::Null).is_ok());
        assert!(check("antinuke", &nuke("per_hour", json!(501)), &Value::Null).is_err());
        assert!(check("antinuke", &nuke("per_hour", json!(1)), &Value::Null).is_err());
        assert!(check("antinuke", &nuke("heat", json!(0)), &Value::Null).is_err());
        assert!(check("antinuke", &nuke("heat", json!(101)), &Value::Null).is_err());
        assert!(check("antinuke", &nuke("per_minute", json!(51)), &Value::Null).is_err());
        assert!(check("antinuke", &nuke("per_minute", json!(null)), &Value::Null).is_ok());
        assert!(check("antinuke", &nuke("per_hour", json!("9")), &Value::Null).is_err());
        let age = |v: i64| json!({"min_age_override_ms": v});
        assert!(check("joingate", &age(5_000), &Value::Null).is_ok());
        assert!(check("joingate", &age(4_999), &Value::Null).is_err());
        assert!(check("joingate", &age(8_640_000_000), &Value::Null).is_ok());
        assert!(check("joingate", &age(8_640_000_001), &Value::Null).is_err());
        let slow = |k: &str, v: i64| json!({"auto_slowmode": {k: v}});
        assert!(check("automod", &slow("slowmode_secs", 21_600), &Value::Null).is_ok());
        assert!(check("automod", &slow("slowmode_secs", 21_601), &Value::Null).is_err());
        assert!(check("automod", &slow("channel_pct", 99), &Value::Null).is_err());
        assert!(check("automod", &json!({}), &Value::Null).is_ok());
        assert_eq!(for_path("automod", "auto_slowmode.min_users"), None);
        assert!(check("joingate", &json!({}), &Value::Null).is_ok());
        let heat = |k: &str, v: f64| json!({"heat": {k: v}});
        assert!(check("automod", &heat("max", 100.0), &Value::Null).is_ok());
        assert!(check("automod", &heat("max", 0.0), &Value::Null).is_err());
        assert!(check("automod", &heat("decay_per_sec", 0.08), &Value::Null).is_ok());
        assert!(check("automod", &heat("decay_per_sec", 0.0), &Value::Null).is_err());
        assert!(check("automod", &heat("decay_per_sec", 1.0), &Value::Null).is_err());
        assert!(check("automod", &json!({"heat": {"max": "9"}}), &Value::Null).is_err());
        assert!(check("panic", &json!({"heat_decay": 0}), &Value::Null).is_err());
        assert!(check("panic", &json!({"heat_decay": 0.01}), &Value::Null).is_ok());
        // A value saved before the limits may stay; changing it must fit.
        let old = nuke("heat", json!(0));
        assert!(check("antinuke", &nuke("heat", json!(0)), &old).is_ok());
        assert!(check("antinuke", &nuke("heat", json!(101)), &old).is_err());
    }
}

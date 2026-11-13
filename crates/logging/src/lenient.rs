//! Serde readers that skip what this version does not know, one entry at a time (D48): a newer
//! binary may store a key or an enum value an older one cannot read, and failing the whole
//! section would put it back to defaults (for anti-nuke: off).
use serde::{de::DeserializeOwned, Deserialize, Deserializer};
use serde_json::Value;

/// A map whose unreadable entries (key or value) are dropped.
pub fn map<'de, D, K, V, M>(d: D) -> Result<M, D::Error>
where
    D: Deserializer<'de>,
    K: DeserializeOwned,
    V: DeserializeOwned,
    M: FromIterator<(K, V)>,
{
    let raw = serde_json::Map::<String, Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, v)| {
            Some((
                serde_json::from_value(Value::String(k)).ok()?,
                serde_json::from_value(v).ok()?,
            ))
        })
        .collect())
}

/// A map of lists: an unreadable key or a value that is not a list drops its entry, and an
/// unreadable item drops only itself (one unknown Discord AutoMod action keeps its rule, S-h3 5).
pub fn map_of_lists<'de, D, K, T, M>(d: D) -> Result<M, D::Error>
where
    D: Deserializer<'de>,
    K: DeserializeOwned,
    T: DeserializeOwned,
    M: FromIterator<(K, Vec<T>)>,
{
    let raw = serde_json::Map::<String, Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, v)| {
            let items = v.as_array()?.iter();
            Some((
                serde_json::from_value(Value::String(k)).ok()?,
                items
                    .filter_map(|x| serde_json::from_value(x.clone()).ok())
                    .collect(),
            ))
        })
        .collect())
}

/// A list whose unreadable items are dropped.
pub fn list<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let raw = Vec::<Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use crate::{LogConfig, LogKind};

    #[test]
    fn an_unknown_route_or_event_drops_only_itself() {
        let c: LogConfig = serde_json::from_value(serde_json::json!({
            "fallback": "11",
            "routes": {"panic": "12", "from_the_future": "13"},
            "events": {"guild_update": "14", "new_event": "15"},
            "custom": [{"events": ["message_delete", "new_event"], "channel": "16"}]
        }))
        .unwrap();
        assert_eq!(c.routes.len(), 1);
        assert_eq!(c.routes[&LogKind::Panic].get(), 12);
        assert_eq!(c.events.len(), 1);
        assert_eq!(c.custom[0].events.len(), 1);
        assert_eq!(c.fallback.map(|f| f.get()), Some(11));
    }
}

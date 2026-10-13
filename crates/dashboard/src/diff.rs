use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Debug, PartialEq)]
pub struct Change {
    pub path: String,
    pub old: Option<Value>,
    pub new: Option<Value>,
}

/// Recursive object diff; arrays and scalars are compared whole.
pub fn diff(a: &Value, b: &Value) -> Vec<Change> {
    let mut out = vec![];
    walk("", Some(a), Some(b), &mut out);
    out
}

fn walk(path: &str, a: Option<&Value>, b: Option<&Value>, out: &mut Vec<Change>) {
    match (a, b) {
        (Some(Value::Object(x)), Some(Value::Object(y))) => {
            let mut keys: Vec<_> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                walk(&format!("{path}/{k}"), x.get(k), y.get(k), out);
            }
        }
        (x, y) if x != y => out.push(Change { path: path.into(), old: x.cloned(), new: y.cloned() }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_changes() {
        let d = diff(&json!({"a": {"x": 1, "y": 2}, "b": [1]}), &json!({"a": {"x": 1, "y": 3, "z": 0}, "b": [1, 2]}));
        let paths: Vec<_> = d.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["/a/y", "/a/z", "/b"]);
    }
}

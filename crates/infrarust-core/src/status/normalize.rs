//! Coerce the status fields this proxy stores as numbers and strings.
//!
//! The proxy re-encodes status JSON, so a document it cannot deserialize
//! becomes "unreachable" even when the backend response was usable. String
//! counts, a missing description, and `"sample": null` are filled in here.
//! A missing or null player list is omitted again so the client shows "???".
//! Description text and sample entries are left unchanged.

use serde_json::{Map, Value};

/// Adjusts `value` so [`super::response::ServerPingResponse`] can deserialize it.
///
/// # Errors
/// Returns an error when the root value is not a JSON object.
pub(crate) fn normalize_status_json(value: &mut Value) -> Result<(), &'static str> {
    let Some(obj) = value.as_object_mut() else {
        return Err("status JSON must be an object");
    };

    if let Some(Value::Object(version)) = obj.get_mut("version") {
        ensure_string(version, "name");
        ensure_i32(version, "protocol");
    }
    if obj.get("players").is_none_or(Value::is_null) {
        obj.remove("players");
        obj.insert(OMIT_PLAYERS.to_owned(), Value::Bool(true));
    }
    match obj.get_mut("players") {
        Some(Value::Object(players)) => {
            ensure_i32(players, "max");
            ensure_i32(players, "online");
            if players.get("sample").is_some_and(Value::is_null) {
                players.remove("sample");
            }
        }
        Some(players) => *players = serde_json::json!({ "max": 0, "online": 0 }),
        None => {}
    }
    if obj.get("description").is_none_or(Value::is_null) {
        obj.insert("description".into(), Value::String(String::new()));
    }
    if obj
        .get("favicon")
        .is_some_and(|favicon| !(favicon.is_string() || favicon.is_null()))
    {
        obj.remove("favicon");
    }
    Ok(())
}

fn ensure_string(map: &mut Map<String, Value>, key: &str) {
    let text = match map.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    };
    map.insert(key.to_owned(), Value::String(text));
}

/// Hidden extra key. Present only while the backend sent no player list.
pub(crate) const OMIT_PLAYERS: &str = "\u{0}players";

fn ensure_i32(map: &mut Map<String, Value>, key: &str) {
    let number = match map.get(key) {
        Some(Value::Number(number)) => number.as_i64().and_then(|n| i32::try_from(n).ok()),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    };
    map.insert(key.to_owned(), Value::from(number.unwrap_or(0)));
}

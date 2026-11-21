//! Logs to stderr: one line per event, as text or as JSON (`--log-json`), for unattended runs.

use std::sync::atomic::{AtomicBool, Ordering};

static JSON: AtomicBool = AtomicBool::new(false);

pub fn use_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

/// `fields` must not carry customer values: names of operations, counts, paths and error codes only.
pub fn event(level: &str, message: &str, fields: &[(&str, String)]) {
    let now = jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    if JSON.load(Ordering::Relaxed) {
        let mut line = serde_json::Map::new();
        line.insert("time".into(), now.into());
        line.insert("level".into(), level.into());
        line.insert("message".into(), message.into());
        for (k, v) in fields {
            line.insert((*k).into(), v.clone().into());
        }
        eprintln!("{}", serde_json::Value::Object(line));
    } else {
        let extra: Vec<String> = fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
        eprintln!("{now} {level:5} {message} {}", extra.join(" "));
    }
}

pub fn info(message: &str, fields: &[(&str, String)]) {
    event("info", message, fields);
}

pub fn warn(message: &str, fields: &[(&str, String)]) {
    event("warn", message, fields);
}

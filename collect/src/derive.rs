//! Fields derived from raw data before redaction, added as synthetic tags. v1 derives the environment
//! (`qloudpulse:environment`), so a resource's name can be pseudonymized and its costs still group by environment;
//! qloudpulse prefers this tag over its own guess from names (ADR 0015).

use crate::body::{Body, Element};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ENV_TAG: &str = "qloudpulse:environment";

/// `[environment]` in the config.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
    /// Copy this tag's value when a resource has it (e.g. `Environment`).
    #[serde(default)]
    pub from_tag: Option<String>,
    /// The tag whose value `rules` test, when `from_tag` gave nothing.
    #[serde(default = "name_tag")]
    pub name_tag: String,
    /// Tried in order; the first whose pattern matches gives the value.
    #[serde(default)]
    pub rules: Vec<EnvironmentRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRule {
    pub pattern: String,
    pub value: String,
}

fn name_tag() -> String {
    "Name".into()
}

pub struct Deriver {
    from_tag: Option<String>,
    name_tag: String,
    rules: Vec<(Regex, String)>,
}

impl Deriver {
    pub fn new(c: &EnvironmentConfig) -> anyhow::Result<Deriver> {
        let rules = c
            .rules
            .iter()
            .map(|r| Ok((Regex::new(&r.pattern)?, r.value.clone())))
            .collect::<anyhow::Result<_>>()?;
        Ok(Deriver {
            from_tag: c.from_tag.clone(),
            name_tag: c.name_tag.clone(),
            rules,
        })
    }

    fn environment(&self, tags: &[(String, String)]) -> Option<String> {
        if tags.iter().any(|(k, _)| k == ENV_TAG) {
            return None;
        }
        let value_of = |key: &str| {
            tags.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .filter(|v| !v.is_empty())
        };
        if let Some(v) = self.from_tag.as_deref().and_then(value_of) {
            return Some(v.to_string());
        }
        let name = value_of(&self.name_tag)?;
        self.rules
            .iter()
            .find(|(re, _)| re.is_match(name))
            .map(|(_, v)| v.clone())
    }

    /// Adds the environment to every tag set in `body` that yields one. How many were added.
    pub fn apply(&self, body: &mut Body) -> usize {
        match body {
            Body::Xml(root) => self.xml(root),
            Body::Json(v) => self.json(v),
            _ => 0,
        }
    }

    fn xml(&self, e: &mut Element) -> usize {
        let mut added = 0;
        let is_set = !e.children.is_empty() && e.children.iter().all(|c| xml_pair(c).is_some());
        if is_set {
            let tags: Vec<(String, String)> = e.children.iter().filter_map(xml_pair).collect();
            if let Some(env) = self.environment(&tags) {
                let mut item = e.children[0].clone();
                for c in &mut item.children {
                    if c.name.eq_ignore_ascii_case("key") {
                        c.text = ENV_TAG.into();
                    } else if c.name.eq_ignore_ascii_case("value") {
                        c.text = env.clone();
                    } else {
                        c.text.clear();
                    }
                }
                e.children.push(item);
                added += 1;
            }
            return added;
        }
        for c in &mut e.children {
            added += self.xml(c);
        }
        added
    }

    fn json(&self, v: &mut Value) -> usize {
        let mut added = 0;
        match v {
            Value::Array(items) if !items.is_empty() && items.iter().all(|i| json_pair(i).is_some()) => {
                let tags: Vec<(String, String)> = items.iter().filter_map(json_pair).collect();
                if let Some(env) = self.environment(&tags) {
                    let (k, val) = if items[0].get("Key").is_some() {
                        ("Key", "Value")
                    } else {
                        ("key", "value")
                    };
                    items.push(serde_json::json!({k: ENV_TAG, val: env}));
                    added += 1;
                }
            }
            Value::Array(items) => {
                for i in items {
                    added += self.json(i);
                }
            }
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    let is_map = (key == "tags" || key == "Tags")
                        && child
                            .as_object()
                            .is_some_and(|m| !m.is_empty() && m.values().all(Value::is_string));
                    if is_map {
                        let tags: Vec<(String, String)> = child
                            .as_object()
                            .expect("checked")
                            .iter()
                            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                            .collect();
                        if let Some(env) = self.environment(&tags) {
                            child
                                .as_object_mut()
                                .expect("checked")
                                .insert(ENV_TAG.into(), Value::String(env));
                            added += 1;
                        }
                    } else {
                        added += self.json(child);
                    }
                }
            }
            _ => {}
        }
        added
    }
}

fn xml_pair(e: &Element) -> Option<(String, String)> {
    if e.children.iter().any(|c| !c.children.is_empty()) {
        return None;
    }
    let k = e.children.iter().find(|c| c.name.eq_ignore_ascii_case("key"))?;
    let v = e.children.iter().find(|c| c.name.eq_ignore_ascii_case("value"))?;
    Some((k.text.clone(), v.text.clone()))
}

fn json_pair(v: &Value) -> Option<(String, String)> {
    for (k, val) in [("Key", "Value"), ("key", "value")] {
        if let (Some(k), Some(val)) = (v.get(k).and_then(Value::as_str), v.get(val).and_then(Value::as_str)) {
            return Some((k.to_string(), val.to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deriver() -> Deriver {
        Deriver::new(&EnvironmentConfig {
            from_tag: Some("Environment".into()),
            name_tag: "Name".into(),
            rules: vec![
                EnvironmentRule {
                    pattern: "(?i)prod".into(),
                    value: "production".into(),
                },
                EnvironmentRule {
                    pattern: "(?i)(dev|test)".into(),
                    value: "development".into(),
                },
            ],
        })
        .unwrap()
    }

    #[test]
    fn from_the_tag_first_then_the_name() {
        let xml = "<R><tagSet><item><key>Name</key><value>web-prod-1</value></item></tagSet>\
                   <tagSet><item><key>Name</key><value>web-prod-2</value></item><item><key>Environment</key><value>staging</value></item></tagSet>\
                   <tagSet><item><key>Name</key><value>misc</value></item></tagSet></R>";
        let mut body = Body::parse(xml.as_bytes());
        assert_eq!(deriver().apply(&mut body), 2);
        let out = body.serialize();
        assert!(
            out.contains("<key>qloudpulse:environment</key><value>production</value>"),
            "{out}"
        );
        assert!(
            out.contains("<key>qloudpulse:environment</key><value>staging</value>"),
            "{out}"
        );
    }

    #[test]
    fn json_lists_and_maps() {
        let mut body =
            Body::parse(br#"{"cluster":{"tags":{"Name":"api-dev"}},"Tags":[{"Key":"Name","Value":"db-prod"}]}"#);
        assert_eq!(deriver().apply(&mut body), 2);
        let v = body.json().unwrap();
        assert_eq!(v["cluster"]["tags"][ENV_TAG], "development");
        assert_eq!(v["Tags"][1], serde_json::json!({"Key": ENV_TAG, "Value": "production"}));
    }
}

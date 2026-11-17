//! Reads the JSON of `pulumi preview --json` and turns it into a request.
//!
//! The format is Pulumi's `display.PreviewDigest`: `steps[]`, each with `op`, `urn` and the resource's `oldState` and
//! `newState` (`apitype.ResourceV3`: `type`, `custom`, `id`, `inputs`, `outputs`, `provider`). Pulumi prints a secret
//! as `"[secret]"` and a value it knows only after apply as its unknown sentinel; the client sends neither.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::catalogue;
use crate::request::{ChangeKind, FORMAT_VERSION, NotPriced, Request, ResourceChange, Tool};

/// What Pulumi writes for a value it will only know after apply.
pub const UNKNOWN: &str = "04da6b54-80e4-46f7-96ec-b56ff0331ba9";
/// What `pulumi preview --json` writes in place of a secret.
pub const SECRET: &str = "[secret]";

#[derive(Debug, Deserialize)]
pub struct Preview {
    #[serde(default)]
    pub config: BTreeMap<String, Value>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub op: String,
    pub urn: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub old_state: Option<State>,
    #[serde(default)]
    pub new_state: Option<State>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct State {
    #[serde(rename = "type")]
    pub type_token: String,
    #[serde(default)]
    pub custom: bool,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub inputs: Option<Value>,
    #[serde(default)]
    pub provider: Option<String>,
}

/// What the request is built with besides the preview.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The region for resources whose provider and stack name none.
    pub region: Option<String>,
    /// Send the cloud ID of each resource that exists before the change.
    pub with_resource_ids: bool,
}

/// What the client keeps in CI about a resource it sends: the answer names it by key, and the comment by these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub key: String,
    pub name: String,
    pub label: String,
    pub change: ChangeKind,
}

/// The request, and what stays in CI to write the comment.
#[derive(Debug)]
pub struct Estimate {
    pub request: Request,
    pub locals: Vec<Local>,
}

pub fn parse(json: &str) -> Result<Preview> {
    serde_json::from_str(json).context("the input is not the JSON of `pulumi preview --json`")
}

/// The key a resource is sent under: `r` and the first 16 hex digits of the SHA-256 of its URN.
pub fn key(urn: &str) -> String {
    let digest = Sha256::digest(urn.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("r{hex}")
}

/// The resource's name: the URN's last part.
fn name(urn: &str) -> &str {
    urn.rsplit_once("::").map_or(urn, |(_, n)| n)
}

/// A provider reference is `<provider URN>::<provider id>`.
fn provider_urn(reference: &str) -> &str {
    reference.rsplit_once("::").map_or(reference, |(u, _)| u)
}

fn is_hidden(v: &Value) -> bool {
    match v {
        Value::String(s) => s == UNKNOWN || s == SECRET,
        Value::Array(items) => items.iter().any(is_hidden),
        Value::Object(m) => m.values().any(is_hidden),
        _ => false,
    }
}

/// An AWS region name such as `eu-west-1`, so nothing else is sent in its place.
pub fn is_region(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() >= 3
        && parts[0].len() == 2
        && parts[0].chars().all(|c| c.is_ascii_lowercase())
        && parts[1..parts.len() - 1]
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase()))
        && parts
            .last()
            .is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Copies the value at `path` in `src` into `dst`, through objects and every item of a list. Returns whether a hidden
/// value was met on the way, which is then left out.
fn pick_into(src: &Value, path: &[&str], dst: &mut Value) -> bool {
    if path.is_empty() {
        if is_hidden(src) {
            return true;
        }
        *dst = src.clone();
        return false;
    }
    match src {
        Value::String(_) => is_hidden(src),
        Value::Object(m) => {
            let Some(child) = m.get(path[0]) else { return false };
            if !dst.is_object() {
                *dst = Value::Object(Map::new());
            }
            let obj = dst.as_object_mut().expect("just made an object");
            let slot = obj.entry(path[0]).or_insert(Value::Null);
            let hidden = pick_into(child, &path[1..], slot);
            if slot.is_null() {
                obj.remove(path[0]);
            }
            if obj.is_empty() {
                *dst = Value::Null;
            }
            hidden
        }
        Value::Array(items) => {
            if !dst.is_array() {
                *dst = Value::Array(vec![Value::Null; items.len()]);
            }
            let out = dst.as_array_mut().expect("just made a list");
            let mut hidden = false;
            for (item, slot) in items.iter().zip(out.iter_mut()) {
                hidden |= pick_into(item, path, slot);
            }
            if out.iter().all(Value::is_null) {
                *dst = Value::Null;
            }
            hidden
        }
        _ => false,
    }
}

/// The priced attributes of a resource's inputs, and the ones that were hidden.
fn priced_attributes(inputs: Option<&Value>, attributes: &[&str]) -> (Map<String, Value>, Vec<String>) {
    let mut out = Value::Object(Map::new());
    let mut hidden = Vec::new();
    if let Some(inputs) = inputs {
        for attribute in attributes {
            let path: Vec<&str> = attribute.split('.').collect();
            if pick_into(inputs, &path, &mut out) {
                hidden.push((*attribute).to_string());
            }
        }
    }
    let map = match out {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    (map, hidden)
}

/// One resource's steps, gathered: a replacement can come as `create-replacement`, `replace` and `delete-replaced`.
#[derive(Default)]
struct Gathered {
    ops: BTreeSet<String>,
    old: Option<State>,
    new: Option<State>,
    provider: Option<String>,
}

impl Gathered {
    fn change(&self) -> Option<ChangeKind> {
        let has = |op: &str| self.ops.contains(op);
        if has("replace") || has("create-replacement") || has("delete-replaced") {
            Some(ChangeKind::Replace)
        } else if has("create") {
            Some(ChangeKind::Create)
        } else if has("update") {
            Some(ChangeKind::Update)
        } else if has("delete") {
            Some(ChangeKind::Delete)
        } else {
            None
        }
    }
}

/// Builds the request from a preview.
pub fn estimate(preview: &Preview, options: &Options) -> Estimate {
    let mut order: Vec<&str> = Vec::new();
    let mut gathered: HashMap<&str, Gathered> = HashMap::new();
    let mut provider_regions: HashMap<&str, String> = HashMap::new();

    for step in &preview.steps {
        for state in [&step.new_state, &step.old_state].into_iter().flatten() {
            if state.type_token.starts_with("pulumi:providers:aws")
                && let Some(region) = state
                    .inputs
                    .as_ref()
                    .and_then(|i| i.get("region"))
                    .and_then(Value::as_str)
                    .filter(|r| is_region(r))
            {
                provider_regions
                    .entry(step.urn.as_str())
                    .or_insert_with(|| region.to_string());
            }
        }
        let g = gathered.entry(step.urn.as_str()).or_insert_with(|| {
            order.push(step.urn.as_str());
            Gathered::default()
        });
        g.ops.insert(step.op.clone());
        if g.old.is_none() {
            g.old.clone_from(&step.old_state);
        }
        if g.new.is_none() {
            g.new.clone_from(&step.new_state);
        }
        if g.provider.is_none() {
            g.provider.clone_from(&step.provider);
        }
    }

    let stack_region = preview
        .config
        .get("aws:region")
        .and_then(Value::as_str)
        .filter(|r| is_region(r))
        .map(str::to_string);
    let fallback_region = stack_region.or_else(|| options.region.clone().filter(|r| is_region(r)));

    let mut resources = Vec::new();
    let mut locals = Vec::new();
    let mut not_priced: BTreeMap<String, u32> = BTreeMap::new();

    for urn in order {
        let g = &gathered[urn];
        let Some(change) = g.change() else { continue };
        let Some(state) = g.new.as_ref().or(g.old.as_ref()) else {
            continue;
        };
        if !state.custom || state.type_token.starts_with("pulumi:") {
            continue;
        }
        let Some(priced) = catalogue::priced(&state.type_token) else {
            if catalogue::is_aws(&state.type_token) {
                *not_priced.entry(state.type_token.clone()).or_default() += 1;
            }
            continue;
        };

        let provider = g
            .new
            .as_ref()
            .and_then(|s| s.provider.as_deref())
            .or_else(|| g.old.as_ref().and_then(|s| s.provider.as_deref()))
            .or(g.provider.as_deref());
        let region = provider
            .and_then(|p| provider_regions.get(provider_urn(p)))
            .cloned()
            .or_else(|| fallback_region.clone());

        let before = match change {
            ChangeKind::Create => None,
            _ => Some(priced_attributes(g.old.as_ref().and_then(|s| s.inputs.as_ref()), priced.attributes).0),
        };
        let (after, unknown) = match change {
            ChangeKind::Delete => (None, Vec::new()),
            _ => {
                let (attrs, hidden) =
                    priced_attributes(g.new.as_ref().and_then(|s| s.inputs.as_ref()), priced.attributes);
                (Some(attrs), hidden)
            }
        };
        let resource_id = if options.with_resource_ids && change != ChangeKind::Create {
            g.old
                .as_ref()
                .and_then(|s| s.id.as_deref())
                .filter(|id| !id.is_empty() && *id != UNKNOWN && *id != SECRET)
                .map(str::to_string)
        } else {
            None
        };

        let key = key(urn);
        locals.push(Local {
            key: key.clone(),
            name: name(urn).to_string(),
            label: priced.label.to_string(),
            change,
        });
        resources.push(ResourceChange {
            key,
            change,
            type_token: state.type_token.clone(),
            region,
            before,
            after,
            unknown,
            resource_id,
        });
    }

    Estimate {
        request: Request {
            format_version: FORMAT_VERSION,
            tool: Tool::Pulumi,
            resources,
            not_priced: not_priced
                .into_iter()
                .map(|(type_token, count)| NotPriced { type_token, count })
                .collect(),
        },
        locals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_is_r_and_16_hex_digits_and_differs_per_urn() {
        let a = key("urn:pulumi:dev::shop::aws:rds/instance:Instance::orders");
        let b = key("urn:pulumi:dev::shop::aws:rds/instance:Instance::payments");
        assert_eq!(a.len(), 17);
        assert!(a.starts_with('r') && a[1..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_eq!(a, key("urn:pulumi:dev::shop::aws:rds/instance:Instance::orders"));
    }

    #[test]
    fn region_names_only() {
        assert!(is_region("eu-west-1"));
        assert!(is_region("us-gov-west-1"));
        assert!(is_region("ap-southeast-2"));
        assert!(!is_region("eu-west"));
        assert!(!is_region("EU-west-1"));
        assert!(!is_region("[secret]"));
        assert!(!is_region(""));
    }

    #[test]
    fn picks_listed_attributes_only() {
        let inputs = json!({"instanceClass": "db.r6i.xlarge", "username": "app", "tags": {"team": "a"}});
        let (attrs, hidden) = priced_attributes(Some(&inputs), &["instanceClass", "multiAz"]);
        assert_eq!(Value::Object(attrs), json!({"instanceClass": "db.r6i.xlarge"}));
        assert!(hidden.is_empty());
    }

    #[test]
    fn picks_through_objects_and_lists() {
        let inputs = json!({
            "rootBlockDevice": {"volumeSize": 50, "volumeType": "gp3", "kmsKeyId": "k"},
            "ebsBlockDevices": [{"volumeSize": 100, "deviceName": "/dev/sdf"}, {"volumeSize": 200}]
        });
        let (attrs, _) = priced_attributes(
            Some(&inputs),
            &[
                "rootBlockDevice.volumeSize",
                "rootBlockDevice.volumeType",
                "ebsBlockDevices.volumeSize",
            ],
        );
        assert_eq!(
            Value::Object(attrs),
            json!({
                "rootBlockDevice": {"volumeSize": 50, "volumeType": "gp3"},
                "ebsBlockDevices": [{"volumeSize": 100}, {"volumeSize": 200}]
            })
        );
    }

    #[test]
    fn unknown_and_secret_values_are_named_not_sent() {
        let inputs =
            json!({"instanceType": UNKNOWN, "tenancy": SECRET, "rootBlockDevice": UNKNOWN, "ebsOptimized": true});
        let (attrs, hidden) = priced_attributes(
            Some(&inputs),
            &["instanceType", "tenancy", "ebsOptimized", "rootBlockDevice.volumeSize"],
        );
        assert_eq!(Value::Object(attrs), json!({"ebsOptimized": true}));
        assert_eq!(hidden, vec!["instanceType", "tenancy", "rootBlockDevice.volumeSize"]);
    }

    #[test]
    fn a_list_with_an_unknown_item_is_not_sent() {
        let inputs = json!({"instanceTypes": ["m7g.large", UNKNOWN]});
        let (attrs, hidden) = priced_attributes(Some(&inputs), &["instanceTypes"]);
        assert!(attrs.is_empty());
        assert_eq!(hidden, vec!["instanceTypes"]);
    }

    #[test]
    fn provider_urn_drops_the_id() {
        assert_eq!(
            provider_urn("urn:pulumi:dev::shop::pulumi:providers:aws::default_7_10_0::0a1b"),
            "urn:pulumi:dev::shop::pulumi:providers:aws::default_7_10_0"
        );
    }
}

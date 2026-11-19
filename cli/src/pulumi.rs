//! Reads the JSON of `pulumi preview --json` and turns it into a request.
//!
//! The format is Pulumi's `display.PreviewDigest`: `steps[]`, each with `op`, `urn` and the resource's `oldState` and
//! `newState` (`apitype.ResourceV3`: `type`, `custom`, `id`, `inputs`, `outputs`, `provider`,
//! `propertyDependencies`). Pulumi prints a secret as `"[secret]"` and a value it knows only after apply as its
//! unknown sentinel; the client sends neither. Which types and attributes are sent is the catalogue's.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::catalogue::{Catalogue, CatalogueType};
use crate::request::{ChangeKind, FORMAT_VERSION, MAX_RESOURCES, NotPriced, Request, ResourceChange, Tool};

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
    /// For each input, the URNs of the resources its value came from.
    #[serde(default, rename = "propertyDependencies")]
    pub property_dependencies: BTreeMap<String, Vec<String>>,
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
    /// One for each resource sent, those left the same included.
    pub locals: Vec<Local>,
    /// Whether the preview listed a cloud resource left the same, as `pulumi preview --json --show-sames` does. The
    /// stack and its providers, which Pulumi lists as the same without it too, do not count.
    pub stack_listed: bool,
    /// The stack's unchanged resources of the catalogue's providers whose type it does not price.
    pub stack_not_priced: u32,
    /// When sending every unchanged resource would pass [`MAX_RESOURCES`], how many resources that would have been:
    /// then only those a changed one refers to are sent, and the comment leaves the stack's cost out.
    pub stack_too_large: Option<usize>,
    /// The changed resources of types AWS charges nothing for of their own, by type, in the order first met. They are
    /// not sent; unchanged ones of such types are neither sent nor counted.
    pub free: Vec<Free>,
}

/// Changed resources of one type AWS charges nothing for of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Free {
    pub type_token: String,
    pub reason: String,
    pub count: u32,
}

impl Free {
    /// What the comment calls the type: the type token's last part, such as `Role`.
    pub fn label(&self) -> &str {
        self.type_token.rsplit(':').next().unwrap_or(&self.type_token)
    }
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

/// The most characters a stack's name may have.
pub const MAX_STACK: usize = 200;

/// The stack a preview is of, `project/stack`, from its URNs, `urn:pulumi:<stack>::<project>::…`: only when every
/// URN names the same one, and it is at most [`MAX_STACK`] characters with no control characters.
pub fn stack_of(preview: &Preview) -> Option<String> {
    let mut found: Option<(&str, &str)> = None;
    for step in &preview.steps {
        let mut parts = step.urn.strip_prefix("urn:pulumi:")?.split("::");
        let (stack, project) = (parts.next()?, parts.next()?);
        if stack.is_empty() || project.is_empty() {
            return None;
        }
        match found {
            None => found = Some((stack, project)),
            Some(f) if f != (stack, project) => return None,
            Some(_) => {}
        }
    }
    let (stack, project) = found?;
    let name = format!("{project}/{stack}");
    (name.chars().count() <= MAX_STACK && !name.chars().any(char::is_control)).then_some(name)
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
fn priced_attributes<S: AsRef<str>>(inputs: Option<&Value>, attributes: &[S]) -> (Map<String, Value>, Vec<String>) {
    let mut out = Value::Object(Map::new());
    let mut hidden = Vec::new();
    if let Some(inputs) = inputs {
        for attribute in attributes {
            let attribute = attribute.as_ref();
            let path: Vec<&str> = attribute.split('.').collect();
            if pick_into(inputs, &path, &mut out) {
                hidden.push(attribute.to_string());
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

/// The resource types a preview holds, each once, to ask qloudpulse which it prices: of every resource the preview
/// lists, changed or not, that is a cloud resource and not one of Pulumi's own.
pub fn found_types(preview: &Preview) -> Vec<String> {
    let found: BTreeSet<&str> = preview
        .steps
        .iter()
        .flat_map(|step| [&step.new_state, &step.old_state])
        .flatten()
        .filter(|state| state.custom && !state.type_token.starts_with("pulumi:"))
        .map(|state| state.type_token.as_str())
        .collect();
    found.into_iter().map(str::to_string).collect()
}

/// The keys of the resources a state's references name, from the inputs' dependencies; the URNs go into `referred`.
fn references_of<'a>(
    state: Option<&'a State>,
    t: &CatalogueType,
    referred: &mut Vec<&'a str>,
) -> Option<BTreeMap<String, String>> {
    let state = state?;
    let mut out = BTreeMap::new();
    for name in &t.references {
        if let Some(urn) = state.property_dependencies.get(name).and_then(|urns| urns.first()) {
            out.insert(name.clone(), key(urn));
            referred.push(urn.as_str());
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Builds the request from a preview, sending what the catalogue lists.
pub fn estimate(preview: &Preview, catalogue: &Catalogue, options: &Options) -> Estimate {
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
    let region_of = |g: &Gathered| {
        let provider = g
            .new
            .as_ref()
            .and_then(|s| s.provider.as_deref())
            .or_else(|| g.old.as_ref().and_then(|s| s.provider.as_deref()))
            .or(g.provider.as_deref());
        provider
            .and_then(|p| provider_regions.get(provider_urn(p)))
            .cloned()
            .or_else(|| fallback_region.clone())
    };

    let mut resources = Vec::new();
    let mut locals = Vec::new();
    let mut not_priced: BTreeMap<String, u32> = BTreeMap::new();
    let mut free: Vec<Free> = Vec::new();
    let mut sent: BTreeSet<&str> = BTreeSet::new();
    let mut referred: Vec<&str> = Vec::new();

    for &urn in &order {
        let g = &gathered[urn];
        let Some(change) = g.change() else { continue };
        let Some(state) = g.new.as_ref().or(g.old.as_ref()) else {
            continue;
        };
        if !state.custom || state.type_token.starts_with("pulumi:") {
            continue;
        }
        if let Some(f) = catalogue.free(&state.type_token) {
            match free.iter_mut().find(|x| x.type_token == f.token) {
                Some(x) => x.count += 1,
                None => free.push(Free {
                    type_token: f.token.clone(),
                    reason: f.reason.clone(),
                    count: 1,
                }),
            }
            continue;
        }
        let Some(priced) = catalogue.priced(&state.type_token) else {
            if catalogue.counts(&state.type_token) {
                *not_priced.entry(state.type_token.clone()).or_default() += 1;
            }
            continue;
        };
        let region = region_of(g);

        let before = match change {
            ChangeKind::Create => None,
            _ => Some(priced_attributes(g.old.as_ref().and_then(|s| s.inputs.as_ref()), &priced.attributes).0),
        };
        let (after, unknown) = match change {
            ChangeKind::Delete => (None, Vec::new()),
            _ => {
                let (attrs, hidden) =
                    priced_attributes(g.new.as_ref().and_then(|s| s.inputs.as_ref()), &priced.attributes);
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

        let references = match change {
            ChangeKind::Delete => references_of(g.old.as_ref(), priced, &mut referred),
            _ => references_of(g.new.as_ref(), priced, &mut referred),
        };

        let key = key(urn);
        sent.insert(urn);
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
            references,
        });
    }

    // Every resource the change leaves the same goes too, so that the comment can add up what the stack costs: Pulumi
    // lists them with `--show-sames`. Only what a changed resource sends goes for each, never its cloud ID, and its
    // name stays in CI. Those of the catalogue's providers that it does not price are counted for the comment only;
    // `notPriced` is the change's.
    let mut stack_listed = false;
    let mut stack_not_priced = 0;
    let mut sames: Vec<&str> = Vec::new();
    for &urn in &order {
        let g = &gathered[urn];
        if !g.ops.contains("same") {
            continue;
        }
        let Some(state) = g.new.as_ref().or(g.old.as_ref()) else {
            continue;
        };
        // Pulumi lists the stack and its providers as the same without `--show-sames` too: only a cloud resource
        // left the same says the preview lists the stack.
        if !state.custom || state.type_token.starts_with("pulumi:") {
            continue;
        }
        stack_listed = true;
        if sent.contains(urn) {
            continue;
        }
        if catalogue.priced(&state.type_token).is_some() {
            sames.push(urn);
        } else if catalogue.free(&state.type_token).is_none() && catalogue.counts(&state.type_token) {
            stack_not_priced += 1;
        }
    }

    // A resource a changed one refers to, such as the task definition of a service whose count changes, goes whatever
    // the stack's size. With every other one the request would pass its limit, the stack's cost is left out.
    let mut stack_too_large = None;
    if resources.len() + sames.len() > MAX_RESOURCES {
        stack_too_large = Some(resources.len() + sames.len());
        sames.clear();
    }
    for urn in referred.into_iter().chain(sames) {
        if !sent.insert(urn) {
            continue;
        }
        let Some(g) = gathered.get(urn) else { continue };
        let Some(state) = g.new.as_ref().or(g.old.as_ref()) else {
            continue;
        };
        let Some(priced) = catalogue.priced(&state.type_token) else {
            continue;
        };
        let (attrs, hidden) = priced_attributes(state.inputs.as_ref(), &priced.attributes);
        let key = key(urn);
        locals.push(Local {
            key: key.clone(),
            name: name(urn).to_string(),
            label: priced.label.to_string(),
            change: ChangeKind::Same,
        });
        resources.push(ResourceChange {
            key,
            change: ChangeKind::Same,
            type_token: state.type_token.clone(),
            region: region_of(g),
            before: None,
            after: Some(attrs),
            unknown: hidden,
            resource_id: None,
            references: None,
        });
    }

    Estimate {
        request: Request {
            format_version: FORMAT_VERSION,
            tool: Tool::Pulumi,
            stack: stack_of(preview),
            resources,
            not_priced: not_priced
                .into_iter()
                .map(|(type_token, count)| NotPriced { type_token, count })
                .collect(),
        },
        locals,
        stack_listed,
        stack_not_priced,
        stack_too_large,
        free,
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

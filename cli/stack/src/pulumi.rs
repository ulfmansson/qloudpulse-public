//! Reads the JSON of `pulumi preview --json` and turns it into a request.
//!
//! The format is Pulumi's `display.PreviewDigest`: `steps[]`, each with `op`, `urn` and the resource's `oldState` and
//! `newState` (`apitype.ResourceV3`: `type`, `custom`, `id`, `inputs`, `outputs`, `provider`,
//! `propertyDependencies`). Pulumi prints a secret as `"[secret]"` and a value it knows only after apply as its
//! unknown sentinel; the client sends neither. Which types and attributes are sent is the catalogue's.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

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
    /// What the provider holds after the last apply, the defaults it filled in included; in `newState`, what it plans.
    #[serde(default)]
    pub outputs: Option<Value>,
    #[serde(default)]
    pub provider: Option<String>,
    /// For each input, the URNs of the resources its value came from.
    #[serde(default, rename = "propertyDependencies")]
    pub property_dependencies: BTreeMap<String, Vec<String>>,
}

/// What the request is built with besides the preview.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `--region`: the region of a resource for which neither the preview nor its provider names one. It wins over the
    /// stack's `aws:region` and this machine's AWS settings, as an option given on the command line is the more
    /// deliberate choice.
    pub region: Option<String>,
    /// This machine's AWS region settings, for resources whose provider, stack and `--region` name none.
    pub aws: AwsEnvironment,
    /// Send the cloud ID of each resource that exists before the change.
    pub with_resource_ids: bool,
    /// `--no-store`: ask qloudpulse to keep nothing, and send no names or URNs (qloudpulse's ADR 0140).
    pub keep_nothing: bool,
}

/// The AWS region settings of the machine the client runs on, as the AWS provider falls back to them: the
/// environment, then the region of the profile in use in the shared config file. Only regions and a profile's name
/// are kept; no credentials are read, and nothing here is sent but a region.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AwsEnvironment {
    /// `AWS_REGION`.
    pub region: Option<String>,
    /// `AWS_DEFAULT_REGION`.
    pub default_region: Option<String>,
    /// `AWS_PROFILE`.
    pub profile: Option<String>,
    /// The `region` of each profile in `AWS_CONFIG_FILE`, else `~/.aws/config`, by profile name.
    pub profile_regions: BTreeMap<String, String>,
}

impl AwsEnvironment {
    /// Reads the environment and the shared config file. A file that is missing or cannot be read names no region.
    pub fn from_system() -> AwsEnvironment {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let home = var("HOME").or_else(|| var("USERPROFILE"));
        let path = match var("AWS_CONFIG_FILE") {
            Some(file) => match (file.strip_prefix("~/"), &home) {
                (Some(rest), Some(home)) => Some(std::path::Path::new(home).join(rest)),
                _ => Some(file.into()),
            },
            None => home.map(|h| std::path::Path::new(&h).join(".aws").join("config")),
        };
        AwsEnvironment {
            region: var("AWS_REGION"),
            default_region: var("AWS_DEFAULT_REGION"),
            profile: var("AWS_PROFILE"),
            profile_regions: path
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|text| profile_regions(&text))
                .unwrap_or_default(),
        }
    }
}

/// The `region` of each profile in an AWS shared config file, by profile name: `[default]`, `[profile default]` and
/// `[profile NAME]`. Every other key, and every other kind of section such as `[sso-session …]`, is passed over, so
/// nothing but a region is kept, and only a value [`is_region`] takes.
pub fn profile_regions(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut profile: Option<String> = None;
    for line in text.lines() {
        // An indented line belongs to a nested setting, such as `s3 =`, and not to the profile itself.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let section = section.trim();
            profile = if section == "default" {
                Some("default".to_string())
            } else {
                section
                    .strip_prefix("profile ")
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
            };
            continue;
        }
        let (Some(name), Some((key, value))) = (&profile, line.split_once('=')) else {
            continue;
        };
        let value = value.split([' ', '\t', '#', ';']).find(|v| !v.is_empty()).unwrap_or("");
        if key.trim() == "region" && is_region(value) {
            out.insert(name.clone(), value.to_string());
        }
    }
    out
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
    /// How many of the resources sent, those left the same included, have no region: neither the preview, its
    /// provider, `--region`, the stack nor this machine names one, and qloudpulse cannot price them.
    pub no_region: usize,
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

/// The longest name qloudpulse keeps.
pub const MAX_NAME: usize = 300;
/// The longest URN qloudpulse keeps.
pub const MAX_URN: usize = 1000;

/// The name and URN a resource is sent with, as qloudpulse takes them (ADR 0140): the name without control characters
/// and cut to [`MAX_NAME`], the URN only as it is and when it fits. Neither with `--no-store`.
fn identity(urn: &str, options: &Options) -> (Option<String>, Option<String>) {
    if options.keep_nothing {
        return (None, None);
    }
    let name: String = name(urn).chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    let name = (!name.is_empty()).then_some(name);
    let urn = (urn.starts_with("urn:pulumi:") && urn.chars().count() <= MAX_URN && !urn.chars().any(char::is_control))
        .then(|| urn.to_string());
    (name, urn)
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
#[cfg(test)]
fn priced_attributes<S: AsRef<str>>(inputs: Option<&Value>, attributes: &[S]) -> (Map<String, Value>, Vec<String>) {
    priced_attributes_filled(inputs, &Fill::default(), attributes)
}

/// Where a priced attribute the inputs leave out is taken from: the outputs of a state that exists, which hold the
/// defaults the provider filled in, such as an EBS volume's `type` or a Lambda function's `memorySize`.
#[derive(Default)]
struct Fill<'a> {
    /// The outputs to take it from, the first that holds it.
    from: Vec<&'a Value>,
    /// Inputs that, when they set the attribute, mean the outputs no longer say what it will be: the old inputs, for
    /// the side after an update or a replacement. Then only an attribute neither side sets, which the change leaves to
    /// the provider as before, is taken from the old outputs.
    unless_set_in: Option<&'a Value>,
}

/// Whether `v` holds a value at `path`, a hidden one included.
fn has_value(v: &Value, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return !v.is_null();
    };
    match v {
        Value::Object(m) => m.get(*first).is_some_and(|c| has_value(c, rest)),
        Value::Array(items) => items.iter().any(|i| has_value(i, path)),
        // An object Pulumi knows only after apply, or a secret one, sets everything under it.
        Value::String(_) => is_hidden(v),
        _ => false,
    }
}

/// Adds to `dst` what `src` holds and `dst` does not, never changing a value `dst` already has. Two lists are merged
/// only when each holds one item: Pulumi may list a set's items in another order in the outputs than in the inputs.
fn merge_missing(dst: &mut Value, src: Value) {
    if src.is_null() {
        return;
    }
    if dst.is_null() {
        *dst = src;
        return;
    }
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(&k) {
                    Some(slot) => merge_missing(slot, v),
                    None if !v.is_null() => {
                        d.insert(k, v);
                    }
                    None => {}
                }
            }
        }
        (Value::Array(d), Value::Array(mut s)) if d.len() == 1 && s.len() == 1 => {
            merge_missing(&mut d[0], s.remove(0));
        }
        _ => {}
    }
}

/// The priced attributes of a resource's inputs, those they leave out taken as `fill` says, and the ones that were
/// hidden. A hidden input is never filled from the outputs, and a hidden output is named, not sent, like an input.
fn priced_attributes_filled<S: AsRef<str>>(
    inputs: Option<&Value>,
    fill: &Fill,
    attributes: &[S],
) -> (Map<String, Value>, Vec<String>) {
    let paths: Vec<Vec<&str>> = attributes.iter().map(|a| a.as_ref().split('.').collect()).collect();
    let mut out = Value::Object(Map::new());
    let mut hidden_input = vec![false; paths.len()];
    for (path, hidden) in paths.iter().zip(hidden_input.iter_mut()) {
        *hidden = inputs.is_some_and(|inputs| pick_into(inputs, path, &mut out));
    }
    // What may be filled: neither hidden in the inputs nor set in the inputs that say the outputs no longer hold.
    let fills: Vec<bool> = paths
        .iter()
        .zip(&hidden_input)
        .map(|(path, hidden)| !hidden && !fill.unless_set_in.is_some_and(|set| has_value(set, path)))
        .collect();
    // Each source's attributes are picked together, so that two of one list, such as each EBS device's size and type,
    // stay in the same items.
    let mut hidden_output = vec![false; paths.len()];
    for outputs in &fill.from {
        let mut found = Value::Null;
        for (i, path) in paths.iter().enumerate() {
            if fills[i] {
                hidden_output[i] |= pick_into(outputs, path, &mut found);
            }
        }
        merge_missing(&mut out, found);
    }
    let hidden = attributes
        .iter()
        .enumerate()
        .filter(|&(i, _)| hidden_input[i] || (hidden_output[i] && !has_value(&out, &paths[i])))
        .map(|(_, a)| a.as_ref().to_string())
        .collect();
    let map = match out {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    (map, hidden)
}

/// The AWS region a resource or provider state names as its `region`, from its inputs, else its outputs.
fn region_in(state: Option<&State>) -> Option<String> {
    let state = state?;
    [&state.inputs, &state.outputs]
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("region").and_then(Value::as_str))
        .find(|r| is_region(r))
        .map(str::to_string)
}

/// A string a provider state names under `key`, unless it is hidden or empty.
fn named_in(state: Option<&State>, key: &str) -> Option<String> {
    let state = state?;
    [&state.inputs, &state.outputs]
        .into_iter()
        .flatten()
        .filter_map(|v| v.get(key).and_then(Value::as_str))
        .find(|s| !s.is_empty() && *s != UNKNOWN && *s != SECRET)
        .map(str::to_string)
}

/// What an AWS provider in the preview says about the region of its resources.
#[derive(Debug, Default)]
struct ProviderRegion {
    /// Its `region`: the new state's, else the old one's, so that a provider whose region is known only after apply
    /// keeps the one it had.
    region: Option<String>,
    /// Its `profile`, whose region the AWS SDK falls back to.
    profile: Option<String>,
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

    for step in &preview.steps {
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

    // An AWS provider's region and profile: the new state's, else the old one's.
    let providers: HashMap<&str, ProviderRegion> = gathered
        .iter()
        .filter_map(|(&urn, g)| {
            let state = g.new.as_ref().or(g.old.as_ref())?;
            state.type_token.starts_with("pulumi:providers:aws").then(|| {
                let (new, old) = (g.new.as_ref(), g.old.as_ref());
                let provider = ProviderRegion {
                    region: region_in(new).or_else(|| region_in(old)),
                    profile: named_in(new, "profile").or_else(|| named_in(old, "profile")),
                };
                (urn, provider)
            })
        })
        .collect();

    // The order the AWS provider itself takes, most specific first, with --region before the stack's and this
    // machine's settings: the resource's own region, its provider's, --region, the stack's aws:region, AWS_REGION,
    // AWS_DEFAULT_REGION, and the region of the AWS profile in use (the provider's, else the stack's aws:profile, else
    // AWS_PROFILE, else default).
    let config = |name: &str| {
        preview
            .config
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty() && *v != SECRET && *v != UNKNOWN)
    };
    let region_flag = options.region.as_deref().filter(|r| is_region(r));
    let stack_region = config("aws:region").filter(|r| is_region(r));
    let env_region = [&options.aws.region, &options.aws.default_region]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .find(|r| is_region(r));
    let stack_profile = config("aws:profile");
    let region_of = |g: &Gathered| -> Option<String> {
        if let Some(own) = region_in(g.new.as_ref()).or_else(|| region_in(g.old.as_ref())) {
            return Some(own);
        }
        let provider = g
            .new
            .as_ref()
            .and_then(|s| s.provider.as_deref())
            .or_else(|| g.old.as_ref().and_then(|s| s.provider.as_deref()))
            .or(g.provider.as_deref())
            .and_then(|p| providers.get(provider_urn(p)));
        if let Some(region) = provider.and_then(|p| p.region.clone()) {
            return Some(region);
        }
        if let Some(region) = region_flag.or(stack_region).or(env_region) {
            return Some(region.to_string());
        }
        let profile = provider
            .and_then(|p| p.profile.as_deref())
            .or(stack_profile)
            .or(options.aws.profile.as_deref())
            .unwrap_or("default");
        options.aws.profile_regions.get(profile).cloned()
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

        let old_inputs = g.old.as_ref().and_then(|s| s.inputs.as_ref());
        let old_outputs: Vec<&Value> = g.old.as_ref().and_then(|s| s.outputs.as_ref()).into_iter().collect();
        // The side before the change is what exists: an attribute its inputs leave out, its outputs hold.
        let before = match change {
            ChangeKind::Create => None,
            _ => {
                let fill = Fill {
                    from: old_outputs.clone(),
                    unless_set_in: None,
                };
                Some(priced_attributes_filled(old_inputs, &fill, &priced.attributes).0)
            }
        };
        // The side after: an attribute neither side's inputs set is one the change leaves as it was, so it is taken
        // from the old outputs too, and the two sides do not differ by a default only one of them names.
        let (after, unknown) = match change {
            ChangeKind::Delete => (None, Vec::new()),
            _ => {
                let fill = match change {
                    ChangeKind::Create => Fill::default(),
                    _ => Fill {
                        from: old_outputs,
                        unless_set_in: old_inputs,
                    },
                };
                let (attrs, hidden) = priced_attributes_filled(
                    g.new.as_ref().and_then(|s| s.inputs.as_ref()),
                    &fill,
                    &priced.attributes,
                );
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
        let (sent_name, sent_urn) = identity(urn, options);
        resources.push(ResourceChange {
            key,
            name: sent_name,
            urn: sent_urn,
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
    // lists them with `--show-sames`. Only what a changed resource sends goes for each, never its cloud ID. Those of the catalogue's providers that it does not price are counted for the comment only;
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
    // Each one sent here names what it refers to as a changed one does, since qloudpulse prices an Auto Scaling group
    // from its launch template and an ECS service from its task definition whether or not they change. What it
    // refers to and is not sent yet goes after the rest, while the request has room.
    let mut queue: VecDeque<&str> = referred.into_iter().chain(sames).collect();
    let mut queued: BTreeSet<&str> = queue.iter().copied().collect();
    let mut found: Vec<&str> = Vec::new();
    while let Some(urn) = queue.pop_front() {
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
        // What exists: an attribute its inputs leave out, its outputs hold.
        let fill = Fill {
            from: [&g.new, &g.old]
                .into_iter()
                .flatten()
                .filter_map(|s| s.outputs.as_ref())
                .collect(),
            unless_set_in: None,
        };
        let (attrs, hidden) = priced_attributes_filled(state.inputs.as_ref(), &fill, &priced.attributes);
        let references = references_of(Some(state), priced, &mut found);
        for next in found.drain(..) {
            // This one, those waiting and the one found must all fit.
            if !sent.contains(next) && resources.len() + 1 + queue.len() < MAX_RESOURCES && queued.insert(next) {
                queue.push_back(next);
            }
        }
        let key = key(urn);
        locals.push(Local {
            key: key.clone(),
            name: name(urn).to_string(),
            label: priced.label.to_string(),
            change: ChangeKind::Same,
        });
        let (sent_name, sent_urn) = identity(urn, options);
        resources.push(ResourceChange {
            key,
            name: sent_name,
            urn: sent_urn,
            change: ChangeKind::Same,
            type_token: state.type_token.clone(),
            region: region_of(g),
            before: None,
            after: Some(attrs),
            unknown: hidden,
            resource_id: None,
            references,
        });
    }

    let no_region = resources.iter().filter(|r| r.region.is_none()).count();
    Estimate {
        request: Request {
            format_version: FORMAT_VERSION,
            tool: Tool::Pulumi,
            stack: stack_of(preview),
            store: options.keep_nothing.then_some(false),
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
        no_region,
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

    const VOLUME: &str = "aws:ebs/volume:Volume";
    const INSTANCE: &str = "aws:ec2/instance:Instance";

    fn catalogue() -> Catalogue {
        Catalogue::parse(
            r#"{"formatVersion": 1, "providers": ["aws"], "types": [
                {"type": "aws:ebs/volume:Volume", "label": "EBS volume", "attributes": ["size", "type", "iops"]},
                {"type": "aws:ec2/instance:Instance", "label": "EC2 instance", "attributes": ["instanceType",
                 "rootBlockDevice.volumeSize", "rootBlockDevice.volumeType", "ebsBlockDevices.volumeSize",
                 "ebsBlockDevices.volumeType"]}]}"#,
        )
        .unwrap()
    }

    fn urn(type_token: &str, name: &str) -> String {
        format!("urn:pulumi:dev::shop::{type_token}::{name}")
    }

    fn provider_ref(name: &str) -> String {
        format!("{}::0a1b", urn("pulumi:providers:aws", name))
    }

    /// A state of `type_token`, with its inputs and outputs, and its provider when one is named.
    fn state(type_token: &str, inputs: Value, outputs: Value, provider: Option<&str>) -> Value {
        let mut s = json!({"type": type_token, "custom": true, "inputs": inputs, "outputs": outputs});
        if let Some(p) = provider {
            s["provider"] = json!(provider_ref(p));
        }
        s
    }

    /// A step that leaves a resource or provider the same, its old and new state alike.
    fn same(name: &str, state: Value) -> Value {
        let t = state["type"].as_str().unwrap().to_string();
        json!({"op": "same", "urn": urn(&t, name), "oldState": state, "newState": state})
    }

    /// A step that creates a volume of 100 GiB with the provider named.
    fn new_volume(name: &str, inputs: Value, provider: Option<&str>) -> Value {
        json!({"op": "create", "urn": urn(VOLUME, name), "newState": state(VOLUME, inputs, json!({}), provider)})
    }

    fn aws_provider(name: &str, inputs: Value, outputs: Value) -> Value {
        same(name, state("pulumi:providers:aws", inputs, outputs, None))
    }

    fn run(config: Value, steps: Vec<Value>, options: &Options) -> Estimate {
        let preview: Preview = serde_json::from_value(json!({"config": config, "steps": steps})).unwrap();
        estimate(&preview, &catalogue(), options)
    }

    fn region_of(e: &Estimate, type_token: &str, name: &str) -> Option<String> {
        let k = key(&urn(type_token, name));
        e.request.resources.iter().find(|r| r.key == k).unwrap().region.clone()
    }

    /// Every source of a region at once, each its own value, so that what a resource gets says where it came from.
    fn every_source() -> (Value, Options) {
        let config = json!({"aws:region": "eu-west-3", "aws:profile": "stack"});
        let options = Options {
            region: Some("ap-south-1".into()),
            aws: AwsEnvironment {
                region: Some("us-west-1".into()),
                default_region: Some("us-west-2".into()),
                profile: Some("shell".into()),
                profile_regions: [
                    ("default", "ca-central-1"),
                    ("stack", "sa-east-1"),
                    ("shell", "af-south-1"),
                    ("provider", "me-south-1"),
                ]
                .into_iter()
                .map(|(p, r)| (p.to_string(), r.to_string()))
                .collect(),
            },
            with_resource_ids: false,
            keep_nothing: false,
        };
        (config, options)
    }

    #[test]
    fn the_resources_own_region_comes_first_from_its_inputs_or_outputs() {
        let (config, options) = every_source();
        let steps = vec![
            aws_provider("p", json!({"region": "eu-north-1"}), json!({})),
            new_volume("own", json!({"size": 10, "region": "us-east-2"}), Some("p")),
            same(
                "v7",
                state(
                    VOLUME,
                    json!({"size": 10}),
                    json!({"size": 10, "region": "ap-northeast-1"}),
                    Some("p"),
                ),
            ),
            new_volume("secret", json!({"size": 10, "region": SECRET}), Some("p")),
        ];
        let e = run(config, steps, &options);
        assert_eq!(region_of(&e, VOLUME, "own").as_deref(), Some("us-east-2"));
        assert_eq!(
            region_of(&e, VOLUME, "v7").as_deref(),
            Some("ap-northeast-1"),
            "AWS provider v7 writes every resource's region in its outputs"
        );
        assert_eq!(
            region_of(&e, VOLUME, "secret").as_deref(),
            Some("eu-north-1"),
            "a region that is not a region name is passed over: its provider's"
        );
    }

    #[test]
    fn the_providers_region_comes_next_from_its_new_state_else_its_old_one() {
        let (config, options) = every_source();
        let provider_step = |name: &str, old: Value, new: Value| {
            json!({"op": "update", "urn": urn("pulumi:providers:aws", name),
                   "oldState": state("pulumi:providers:aws", old.clone(), old, None),
                   "newState": state("pulumi:providers:aws", new, json!({}), None)})
        };
        let steps = vec![
            provider_step(
                "moved",
                json!({"region": "eu-north-1"}),
                json!({"region": "eu-south-1"}),
            ),
            provider_step("unknown", json!({"region": "eu-central-1"}), json!({"region": UNKNOWN})),
            aws_provider("outputs", json!({}), json!({"region": "eu-central-2"})),
            new_volume("moved", json!({"size": 10}), Some("moved")),
            new_volume("unknown", json!({"size": 10}), Some("unknown")),
            new_volume("outputs", json!({"size": 10}), Some("outputs")),
        ];
        let e = run(config, steps, &options);
        assert_eq!(region_of(&e, VOLUME, "moved").as_deref(), Some("eu-south-1"));
        assert_eq!(
            region_of(&e, VOLUME, "unknown").as_deref(),
            Some("eu-central-1"),
            "a provider whose region is known only after apply: the one it had"
        );
        assert_eq!(region_of(&e, VOLUME, "outputs").as_deref(), Some("eu-central-2"));
    }

    #[test]
    fn then_region_flag_then_the_stack_then_the_environment_then_the_profile() {
        let steps = || {
            vec![
                aws_provider("p", json!({}), json!({})),
                new_volume("v", json!({"size": 10}), Some("p")),
            ]
        };
        let (config, mut options) = every_source();
        let mut config = config;
        let next = |config: &Value, options: &Options| region_of(&run(config.clone(), steps(), options), VOLUME, "v");

        assert_eq!(next(&config, &options).as_deref(), Some("ap-south-1"), "--region");
        options.region = None;
        assert_eq!(
            next(&config, &options).as_deref(),
            Some("eu-west-3"),
            "the stack's aws:region"
        );
        config["aws:region"] = json!(SECRET);
        assert_eq!(next(&config, &options).as_deref(), Some("us-west-1"), "AWS_REGION");
        options.aws.region = Some("not-a-region".into());
        assert_eq!(
            next(&config, &options).as_deref(),
            Some("us-west-2"),
            "AWS_DEFAULT_REGION"
        );
        options.aws.default_region = None;
        assert_eq!(
            next(&config, &options).as_deref(),
            Some("sa-east-1"),
            "the aws:profile's region"
        );
        config.as_object_mut().unwrap().remove("aws:profile");
        assert_eq!(
            next(&config, &options).as_deref(),
            Some("af-south-1"),
            "AWS_PROFILE's region"
        );
        options.aws.profile = None;
        assert_eq!(
            next(&config, &options).as_deref(),
            Some("ca-central-1"),
            "the default profile's region"
        );
        options.aws.profile_regions.clear();
        assert_eq!(next(&config, &options), None);
    }

    #[test]
    fn a_providers_profile_comes_before_the_stacks_and_the_shells() {
        let (config, mut options) = every_source();
        options.region = None;
        options.aws.region = None;
        options.aws.default_region = None;
        let config = json!({"aws:profile": config["aws:profile"]});
        let steps = vec![
            aws_provider("p", json!({"profile": "provider"}), json!({})),
            new_volume("v", json!({"size": 10}), Some("p")),
        ];
        let e = run(config, steps, &options);
        assert_eq!(region_of(&e, VOLUME, "v").as_deref(), Some("me-south-1"));
    }

    #[test]
    fn a_resource_without_a_region_is_counted() {
        let steps = vec![
            new_volume("none", json!({"size": 10}), None),
            new_volume("own", json!({"size": 10, "region": "eu-west-1"}), None),
            same(
                "unchanged",
                state(VOLUME, json!({"size": 10}), json!({"size": 10}), None),
            ),
        ];
        let e = run(json!({}), steps, &Options::default());
        assert_eq!(e.no_region, 2, "the unchanged one too: the stack's cost needs it");
        assert_eq!(region_of(&e, VOLUME, "none"), None);
    }

    #[test]
    fn the_aws_config_file_gives_each_profiles_region_and_nothing_else() {
        let text = "\
# a comment
[default]
region = eu-west-1
aws_access_key_id = AKIAEXAMPLE

[profile prod]
output = json
region=us-east-1 ; why
s3 =
  region = ap-south-1

[profile default]
[sso-session corp]
region = eu-north-1
[shell]
region = eu-south-1
[profile broken]
region = europe
";
        let regions = profile_regions(text);
        assert_eq!(
            regions.into_iter().collect::<Vec<_>>(),
            [
                ("default".to_string(), "eu-west-1".to_string()),
                ("prod".to_string(), "us-east-1".to_string())
            ]
        );
    }

    #[test]
    fn an_unchanged_resource_sends_what_its_outputs_hold_and_its_inputs_leave_out() {
        let instance = state(
            INSTANCE,
            json!({"instanceType": "m5.large", "rootBlockDevice": {"volumeSize": 50}}),
            json!({"instanceType": "m5.large", "privateIp": "10.0.0.1",
                   "rootBlockDevice": {"volumeSize": 50, "volumeType": "gp3", "kmsKeyId": "k"},
                   "ebsBlockDevices": [{"volumeSize": 100, "volumeType": "io2"}, {"volumeSize": 200}]}),
            None,
        );
        let volume = state(
            VOLUME,
            json!({"size": 10, "iops": SECRET}),
            json!({"size": 10, "type": "gp2", "iops": SECRET}),
            None,
        );
        let steps = vec![same("web", instance), same("data", volume)];
        let e = run(json!({}), steps, &Options::default());
        let web = &e.request.resources[0];
        assert_eq!(web.change, ChangeKind::Same);
        assert_eq!(
            Value::Object(web.after.clone().unwrap()),
            json!({"instanceType": "m5.large",
                   "rootBlockDevice": {"volumeSize": 50, "volumeType": "gp3"},
                   "ebsBlockDevices": [{"volumeSize": 100, "volumeType": "io2"}, {"volumeSize": 200}]}),
            "only the catalogue's attributes, from the outputs where the inputs have none"
        );
        let data = &e.request.resources[1];
        assert_eq!(
            Value::Object(data.after.clone().unwrap()),
            json!({"size": 10, "type": "gp2"})
        );
        assert_eq!(
            data.unknown,
            ["iops"],
            "a secret is named, not sent, from the outputs as from the inputs"
        );
    }

    #[test]
    fn an_update_takes_from_the_old_outputs_what_neither_side_sets() {
        let old = state(
            VOLUME,
            json!({"size": 10}),
            json!({"size": 10, "type": "gp2", "iops": 100}),
            None,
        );
        let new = state(VOLUME, json!({"size": 20, "iops": 3000}), json!({}), None);
        let steps = vec![json!({"op": "update", "urn": urn(VOLUME, "data"), "oldState": old, "newState": new})];
        let e = run(json!({}), steps, &Options::default());
        let data = &e.request.resources[0];
        assert_eq!(
            Value::Object(data.before.clone().unwrap()),
            json!({"size": 10, "type": "gp2", "iops": 100}),
            "the old side is what exists: its outputs fill in the default type"
        );
        assert_eq!(
            Value::Object(data.after.clone().unwrap()),
            json!({"size": 20, "iops": 3000, "type": "gp2"}),
            "the type neither side sets stays what it was, so the two sides do not differ by it"
        );
    }

    #[test]
    fn an_update_does_not_fill_what_the_old_inputs_set_or_the_new_ones_hide() {
        let old = state(
            VOLUME,
            json!({"size": 10, "type": "io2"}),
            json!({"size": 10, "type": "io2", "iops": 100}),
            None,
        );
        let new = state(VOLUME, json!({"size": UNKNOWN}), json!({}), None);
        let steps = vec![json!({"op": "update", "urn": urn(VOLUME, "data"), "oldState": old, "newState": new})];
        let e = run(json!({}), steps, &Options::default());
        let data = &e.request.resources[0];
        assert_eq!(
            Value::Object(data.after.clone().unwrap()),
            json!({"iops": 100}),
            "a type the change stops setting is the provider's default again, and an unknown size is not the old one"
        );
        assert_eq!(data.unknown, ["size"]);
    }

    #[test]
    fn a_create_sends_its_inputs_only() {
        let new = state(VOLUME, json!({"size": 10}), json!({"size": 10, "type": UNKNOWN}), None);
        let steps = vec![json!({"op": "create", "urn": urn(VOLUME, "data"), "newState": new})];
        let e = run(json!({}), steps, &Options::default());
        let data = &e.request.resources[0];
        assert_eq!(Value::Object(data.after.clone().unwrap()), json!({"size": 10}));
        assert!(
            data.unknown.is_empty(),
            "what the provider plans is not read for a resource that does not exist yet"
        );
    }

    #[test]
    fn lists_are_merged_only_one_item_to_one() {
        let mut one = json!({"d": [{"a": 1}]});
        merge_missing(&mut one, json!({"d": [{"a": 2, "b": 3}]}));
        assert_eq!(one, json!({"d": [{"a": 1, "b": 3}]}));
        let mut two = json!({"d": [{"a": 1}, {"a": 2}]});
        merge_missing(&mut two, json!({"d": [{"b": 3}, {"b": 4}]}));
        assert_eq!(
            two,
            json!({"d": [{"a": 1}, {"a": 2}]}),
            "a set's items may come in another order"
        );
    }

    #[test]
    fn provider_urn_drops_the_id() {
        assert_eq!(
            provider_urn("urn:pulumi:dev::shop::pulumi:providers:aws::default_7_10_0::0a1b"),
            "urn:pulumi:dev::shop::pulumi:providers:aws::default_7_10_0"
        );
    }
}

//! Applies the config's tag and field rules to recorded answers, and carries every changed value into the requests
//! that name it, so a pseudonymized table name is pseudonymized in `DescribeTable` too and the replay still matches.

use crate::body::{Body, Element};
use crate::config::{Config, FieldAction, FieldRule, TagAction};
use crate::pseudonym::Pseudonymizer;
use crate::record::Record;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// What redaction did, for the manifest and `analyze`: tag keys and field paths with counts, never values.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stats {
    /// tag key -> (action, how many tags)
    pub tags: BTreeMap<String, (String, usize)>,
    /// field rule path -> how many values it changed
    pub fields: BTreeMap<String, usize>,
}

/// The list wrappers left out of field paths.
const WRAPPERS: &[&str] = &["item", "member"];

struct CompiledField {
    rule: FieldRule,
    segments: Vec<String>,
    pattern: Option<Regex>,
}

pub struct Redactor<'a> {
    config: &'a Config,
    pseudonyms: &'a Pseudonymizer,
    fields: Vec<CompiledField>,
    tag_patterns: Vec<Option<Regex>>,
    deriver: Option<crate::derive::Deriver>,
}

/// A tag as found in an answer, for [`visit_tags`].
pub struct TagRef<'a> {
    pub key: &'a str,
    pub value: &'a str,
}

impl<'a> Redactor<'a> {
    pub fn new(config: &'a Config, pseudonyms: &'a Pseudonymizer) -> Redactor<'a> {
        let fields = config
            .fields
            .iter()
            .map(|rule| CompiledField {
                segments: rule.path.split('.').map(str::to_string).collect(),
                pattern: rule
                    .pattern
                    .as_deref()
                    .map(|p| Regex::new(p).expect("checked when the config loaded")),
                rule: rule.clone(),
            })
            .collect();
        let tag_patterns = config
            .tags
            .replace
            .iter()
            .map(|r| {
                r.pattern
                    .as_deref()
                    .map(|p| Regex::new(p).expect("checked when the config loaded"))
            })
            .collect();
        let deriver = config
            .environment
            .as_ref()
            .map(|e| crate::derive::Deriver::new(e).expect("checked when the config loaded"));
        Redactor {
            config,
            pseudonyms,
            fields,
            tag_patterns,
            deriver,
        }
    }

    /// Redacts every record: answers first, then the requests that name a changed value.
    pub fn records(&self, records: &mut [Record]) -> Stats {
        let mut stats = Stats::default();
        let mut changed: HashMap<String, String> = HashMap::new();
        let tag_columns = crate::collect::cur::tag_columns(
            self.config
                .cur
                .as_ref()
                .map(|c| c.scope_tag_keys.as_slice())
                .unwrap_or_default(),
        );
        let mut columns_by_query: HashMap<String, Vec<String>> = HashMap::new();
        for record in records.iter_mut() {
            if record.service == "athena" && record.operation == "GetQueryResults" {
                self.bill_tags(record, &tag_columns, &mut columns_by_query, &mut stats);
            }
            let mut body = Body::parse(record.response.body.as_bytes());
            let mut touched = false;
            if let Some(d) = &self.deriver {
                let added = d.apply(&mut body);
                if added > 0 {
                    touched = true;
                    *stats
                        .fields
                        .entry(format!("derived {}", crate::derive::ENV_TAG))
                        .or_default() += added;
                }
            }
            match &mut body {
                Body::Xml(root) => self.xml(root, &mut Vec::new(), &mut stats, &mut changed, &mut touched),
                Body::Json(v) => self.json(v, &mut Vec::new(), &mut stats, &mut changed, &mut touched),
                _ => {}
            }
            if touched {
                record.response.body = body.serialize();
            }
        }
        if !changed.is_empty() {
            for record in records.iter_mut() {
                carry_into_request(record, &changed);
            }
        }
        stats
    }

    /// Tag values in the bill's tag columns (`resource_tags_user_name`, `scope_tag_0`, …) get the same rules as
    /// tags anywhere else; a dropped value becomes an empty cell, which Athena uses for null.
    fn bill_tags(
        &self,
        record: &mut Record,
        tag_columns: &[(String, String)],
        columns_by_query: &mut HashMap<String, Vec<String>>,
        stats: &mut Stats,
    ) {
        let Ok(mut answer) = serde_json::from_str::<Value>(&record.response.body) else {
            return;
        };
        let id = serde_json::from_str::<Value>(&record.request.body)
            .ok()
            .and_then(|v| v["QueryExecutionId"].as_str().map(str::to_string))
            .unwrap_or_default();
        let columns = columns_by_query.entry(id).or_default();
        let mut touched = false;
        crate::collect::cur::for_each_tag_cell(&mut answer, columns, tag_columns, &mut |key, cell| {
            let old = cell["VarCharValue"].as_str().unwrap_or_default().to_string();
            match self.tag_value(key, &old, stats) {
                None => {
                    *cell = Value::Object(Default::default());
                    touched = true;
                }
                Some(new) if new != old => {
                    cell["VarCharValue"] = Value::String(new);
                    touched = true;
                }
                Some(_) => {}
            }
        });
        if touched {
            record.response.body = answer.to_string();
        }
    }

    fn tag_value(&self, key: &str, value: &str, stats: &mut Stats) -> Option<String> {
        let action = self.config.tag_action(key);
        let entry = stats
            .tags
            .entry(key.to_string())
            .or_insert_with(|| (action.label().to_string(), 0));
        entry.1 += 1;
        match action {
            TagAction::Keep => Some(value.to_string()),
            TagAction::Drop => None,
            TagAction::Pseudonymize => Some(self.pseudonyms.pseudonym(value)),
            TagAction::Encrypt => self.pseudonyms.encrypt(value),
            TagAction::Replace(i) => {
                let rule = &self.config.tags.replace[i];
                Some(match &self.tag_patterns[i] {
                    Some(p) => p.replace_all(value, rule.with.as_str()).into_owned(),
                    None => rule.with.clone(),
                })
            }
        }
    }

    /// The field rule for a leaf at `path`, and the leaf's new value (None: drop it).
    fn field_value(&self, path: &[String], value: &str, stats: &mut Stats) -> Option<Option<String>> {
        let field = self.fields.iter().find(|f| path.ends_with(&f.segments))?;
        let new = match field.rule.action {
            FieldAction::Drop => None,
            FieldAction::Pseudonymize => Some(self.pseudonyms.pseudonym(value)),
            FieldAction::Encrypt => self.pseudonyms.encrypt(value),
            FieldAction::Replace => {
                let with = field.rule.with.as_deref().unwrap_or_default();
                Some(match &field.pattern {
                    Some(p) => p.replace_all(value, with).into_owned(),
                    None => with.to_string(),
                })
            }
        };
        *stats.fields.entry(field.rule.path.clone()).or_default() += 1;
        Some(new)
    }

    fn xml(
        &self,
        element: &mut Element,
        path: &mut Vec<String>,
        stats: &mut Stats,
        changed: &mut HashMap<String, String>,
        touched: &mut bool,
    ) {
        let wrapper = WRAPPERS.contains(&element.name.as_str());
        if !wrapper {
            path.push(element.name.clone());
        }
        let mut keep = Vec::with_capacity(element.children.len());
        for mut child in std::mem::take(&mut element.children) {
            if let Some((k, v)) = xml_tag(&child) {
                let (key, value) = (child.children[k].text.clone(), child.children[v].text.clone());
                match self.tag_value(&key, &value, stats) {
                    None => {
                        *touched = true;
                        continue;
                    }
                    Some(new) if new != value => {
                        child.children[v].text = new;
                        *touched = true;
                    }
                    Some(_) => {}
                }
                keep.push(child);
                continue;
            }
            if child.children.is_empty() {
                let mut leaf_path = path.clone();
                if !WRAPPERS.contains(&child.name.as_str()) {
                    leaf_path.push(child.name.clone());
                }
                match self.field_value(&leaf_path, &child.text, stats) {
                    Some(None) => {
                        changed.insert(child.text.clone(), String::new());
                        *touched = true;
                        continue;
                    }
                    Some(Some(new)) if new != child.text => {
                        changed.insert(std::mem::replace(&mut child.text, new.clone()), new);
                        *touched = true;
                    }
                    _ => {}
                }
                keep.push(child);
                continue;
            }
            self.xml(&mut child, path, stats, changed, touched);
            keep.push(child);
        }
        element.children = keep;
        if !wrapper {
            path.pop();
        }
    }

    fn json(
        &self,
        value: &mut Value,
        path: &mut Vec<String>,
        stats: &mut Stats,
        changed: &mut HashMap<String, String>,
        touched: &mut bool,
    ) {
        match value {
            Value::Object(map) => {
                let keys: Vec<String> = map.keys().cloned().collect();
                for key in keys {
                    let is_tag_map = (key == "tags" || key == "Tags")
                        && map[&key].as_object().is_some_and(|m| m.values().all(Value::is_string));
                    if is_tag_map {
                        let tags = map.get_mut(&key).and_then(Value::as_object_mut).expect("checked above");
                        let names: Vec<String> = tags.keys().cloned().collect();
                        for name in names {
                            let old = tags[&name].as_str().unwrap_or_default().to_string();
                            match self.tag_value(&name, &old, stats) {
                                None => {
                                    tags.remove(&name);
                                    *touched = true;
                                }
                                Some(new) if new != old => {
                                    tags[&name] = Value::String(new);
                                    *touched = true;
                                }
                                Some(_) => {}
                            }
                        }
                        continue;
                    }
                    path.push(key.clone());
                    let child = map.get_mut(&key).expect("key from this map");
                    match leaf(child) {
                        Some(old) => match self.field_value(path, &old, stats) {
                            Some(None) => {
                                map.remove(&key);
                                changed.insert(old, String::new());
                                *touched = true;
                            }
                            Some(Some(new)) if new != old => {
                                *child = Value::String(new.clone());
                                changed.insert(old, new);
                                *touched = true;
                            }
                            _ => {}
                        },
                        None => self.json(child, path, stats, changed, touched),
                    }
                    path.pop();
                }
            }
            Value::Array(items) => {
                let mut keep = Vec::with_capacity(items.len());
                for mut item in std::mem::take(items) {
                    if let Some((k, v)) = json_tag(&item) {
                        let key = item[k].as_str().unwrap_or_default().to_string();
                        let old = item[v].as_str().unwrap_or_default().to_string();
                        match self.tag_value(&key, &old, stats) {
                            None => {
                                *touched = true;
                                continue;
                            }
                            Some(new) if new != old => {
                                item[v] = Value::String(new);
                                *touched = true;
                            }
                            Some(_) => {}
                        }
                        keep.push(item);
                        continue;
                    }
                    if let Some(old) = leaf(&item) {
                        match self.field_value(path, &old, stats) {
                            Some(None) => {
                                changed.insert(old, String::new());
                                *touched = true;
                                continue;
                            }
                            Some(Some(new)) if new != old => {
                                changed.insert(old, new.clone());
                                item = Value::String(new);
                                *touched = true;
                            }
                            _ => {}
                        }
                        keep.push(item);
                        continue;
                    }
                    self.json(&mut item, path, stats, changed, touched);
                    keep.push(item);
                }
                *items = keep;
            }
            _ => {}
        }
    }
}

fn leaf(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// An XML tag: leaf children `key`/`Key` and `value`/`Value` (EC2's `tagSet`, RDS's `TagList`, Auto Scaling's
/// `Tags`). Returns the children's positions.
fn xml_tag(element: &Element) -> Option<(usize, usize)> {
    if element.children.iter().any(|c| !c.children.is_empty()) {
        return None;
    }
    let k = element
        .children
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("key"))?;
    let v = element
        .children
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("value"))?;
    Some((k, v))
}

/// A JSON tag inside a list: `{"Key": …, "Value": …}` or `{"key": …, "value": …}` with string values.
fn json_tag(value: &Value) -> Option<(&'static str, &'static str)> {
    let map = value.as_object()?;
    for (k, v) in [("Key", "Value"), ("key", "value")] {
        if map.get(k).is_some_and(Value::is_string) && map.get(v).is_some_and(Value::is_string) {
            return Some((k, v));
        }
    }
    None
}

/// Every tag in an answer, for `analyze`.
pub fn visit_tags(body: &Body, visit: &mut dyn FnMut(TagRef)) {
    fn xml(e: &Element, visit: &mut dyn FnMut(TagRef)) {
        for child in &e.children {
            match xml_tag(child) {
                Some((k, v)) => visit(TagRef {
                    key: &child.children[k].text,
                    value: &child.children[v].text,
                }),
                None => xml(child, visit),
            }
        }
    }
    fn json(value: &Value, visit: &mut dyn FnMut(TagRef)) {
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    match v.as_object() {
                        Some(tags) if (key == "tags" || key == "Tags") && tags.values().all(Value::is_string) => {
                            for (k, val) in tags {
                                visit(TagRef {
                                    key: k,
                                    value: val.as_str().unwrap_or_default(),
                                });
                            }
                        }
                        _ => json(v, visit),
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    match json_tag(item) {
                        Some((k, v)) => visit(TagRef {
                            key: item[k].as_str().unwrap_or_default(),
                            value: item[v].as_str().unwrap_or_default(),
                        }),
                        None => json(item, visit),
                    }
                }
            }
            _ => {}
        }
    }
    match body {
        Body::Xml(root) => xml(root, visit),
        Body::Json(v) => json(v, visit),
        _ => {}
    }
}

/// Replaces values a rule changed wherever a request names them exactly: form fields, JSON bodies, query
/// parameters and path labels.
fn carry_into_request(record: &mut Record, changed: &HashMap<String, String>) {
    let request = &mut record.request;
    let swap = |s: &str| changed.get(s).cloned();
    let form = request
        .headers
        .iter()
        .any(|(k, v)| k == "content-type" && v.starts_with("application/x-www-form-urlencoded"));
    if form {
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(request.body.as_bytes())
            .into_owned()
            .collect();
        if pairs.iter().any(|(_, v)| changed.contains_key(v)) {
            let mut out = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in pairs {
                out.append_pair(&k, &swap(&v).unwrap_or(v));
            }
            request.body = out.finish();
        }
    } else if let Ok(mut json) = serde_json::from_str::<Value>(&request.body)
        && swap_json(&mut json, changed)
    {
        request.body = json.to_string();
    }
    for (_, v) in request.query.iter_mut() {
        if let Some(new) = swap(v) {
            *v = new;
        }
    }
    if request.path.contains('%') || request.path.split('/').any(|s| changed.contains_key(s)) {
        let segments: Vec<String> = request
            .path
            .split('/')
            .map(|seg| {
                let decoded = percent_decode(seg);
                match swap(&decoded) {
                    Some(new) => crate::aws::protocol::encode_label(&new),
                    None => seg.to_string(),
                }
            })
            .collect();
        request.path = segments.join("/");
    }
}

fn swap_json(value: &mut Value, changed: &HashMap<String, String>) -> bool {
    match value {
        Value::String(s) => match changed.get(s.as_str()) {
            Some(new) => {
                *s = new.clone();
                true
            }
            None => false,
        },
        Value::Array(items) => items.iter_mut().fold(false, |any, i| swap_json(i, changed) | any),
        Value::Object(map) => map.values_mut().fold(false, |any, v| swap_json(v, changed) | any),
        _ => false,
    }
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::protocol::WireRequest;
    use crate::record::RecordedResponse;

    fn config(extra: &str) -> Config {
        let config: Config = toml::from_str(&format!("source_id = \"t\"\n{extra}")).unwrap();
        config.check().unwrap();
        config
    }

    fn record(body: &str, request_body: &str, content_type: &str) -> Record {
        Record {
            collector: "aws-cost-collect".into(),
            account: "111122223333".into(),
            region: "eu-west-1".into(),
            service: "x".into(),
            operation: "Y".into(),
            page: 1,
            request: WireRequest {
                method: "POST".into(),
                host: "h".into(),
                path: "/".into(),
                query: vec![],
                headers: vec![("content-type".into(), content_type.into())],
                body: request_body.into(),
            },
            response: RecordedResponse {
                status: 200,
                headers: vec![],
                body: body.into(),
            },
            error_code: None,
        }
    }

    #[test]
    fn ec2_tags_follow_the_rules() {
        let config = config("[tags]\ndefault = \"deny\"\nkeep = [\"Name\"]\npseudonymize = [\"Owner\"]\n");
        let pseudonyms = Pseudonymizer::new(vec![7; 32], false);
        let xml = "<R><tagSet><item><key>Name</key><value>web</value></item><item><key>Owner</key>\
                   <value>alice@corp</value></item><item><key>Secret</key><value>s3cr3t</value></item></tagSet></R>";
        let mut records = vec![record(xml, "", "x")];
        let stats = Redactor::new(&config, &pseudonyms).records(&mut records);
        let out = &records[0].response.body;
        assert!(out.contains("<value>web</value>"), "{out}");
        assert!(!out.contains("alice"), "{out}");
        assert!(out.contains(&pseudonyms.pseudonym("alice@corp")), "{out}");
        assert!(!out.contains("Secret") && !out.contains("s3cr3t"), "{out}");
        assert_eq!(stats.tags["Secret"], ("drop".to_string(), 1));
    }

    #[test]
    fn json_tag_lists_and_maps() {
        let config = config("[tags]\ndefault = \"allow\"\ndrop = [\"team\"]\n");
        let pseudonyms = Pseudonymizer::new(vec![7; 32], false);
        let json = r#"{"cluster":{"name":"c","tags":{"team":"a","Name":"n"}},"tagList":[{"key":"team","value":"b"},{"key":"x","value":"y"}]}"#;
        let mut records = vec![record(json, "{}", "application/x-amz-json-1.1")];
        Redactor::new(&config, &pseudonyms).records(&mut records);
        let v: Value = serde_json::from_str(&records[0].response.body).unwrap();
        assert_eq!(v["cluster"]["tags"], serde_json::json!({"Name": "n"}));
        assert_eq!(v["tagList"], serde_json::json!([{"key": "x", "value": "y"}]));
    }

    #[test]
    fn pseudonymized_fields_carry_into_later_requests() {
        let config = config("[[fields]]\npath = \"TableNames\"\naction = \"pseudonymize\"\n");
        let pseudonyms = Pseudonymizer::new(vec![7; 32], false);
        let mut records = vec![
            record(r#"{"TableNames":["orders"]}"#, "{}", "application/x-amz-json-1.0"),
            record("{}", r#"{"TableName":"orders"}"#, "application/x-amz-json-1.0"),
        ];
        let stats = Redactor::new(&config, &pseudonyms).records(&mut records);
        let p = pseudonyms.pseudonym("orders");
        assert_eq!(records[0].response.body, format!(r#"{{"TableNames":["{p}"]}}"#));
        assert_eq!(records[1].request.body, format!(r#"{{"TableName":"{p}"}}"#));
        assert_eq!(stats.fields["TableNames"], 1);
    }

    #[test]
    fn bill_tag_columns_follow_the_tag_rules() {
        let config = config(
            "[tags]\ndefault = \"deny\"\npseudonymize = [\"Name\"]\n[cur]\naccount = \"111122223333\"\nscope_tag_keys = [\"Team\"]\n",
        );
        let pseudonyms = Pseudonymizer::new(vec![7; 32], false);
        let answer = serde_json::json!({"ResultSet": {
        "ResultSetMetadata": {"ColumnInfo": [{"Name": "resource_id"}, {"Name": "resource_tags_user_name"}, {"Name": "scope_tag_0"}]},
        "Rows": [
            {"Data": [{"VarCharValue": "resource_id"}, {"VarCharValue": "resource_tags_user_name"}, {"VarCharValue": "scope_tag_0"}]},
            {"Data": [{"VarCharValue": "i-1"}, {"VarCharValue": "web-prod"}, {"VarCharValue": "payments"}]},
            {"Data": [{"VarCharValue": "i-2"}, {}, {"VarCharValue": "search"}]}
        ]}});
        let mut r = record(
            &answer.to_string(),
            r#"{"QueryExecutionId":"q1","MaxResults":1000}"#,
            "application/x-amz-json-1.1",
        );
        r.service = "athena".into();
        r.operation = "GetQueryResults".into();
        let mut records = vec![r];
        let stats = Redactor::new(&config, &pseudonyms).records(&mut records);
        let v: Value = serde_json::from_str(&records[0].response.body).unwrap();
        let rows = &v["ResultSet"]["Rows"];
        assert_eq!(
            rows[0]["Data"][1]["VarCharValue"], "resource_tags_user_name",
            "the header row stays"
        );
        assert_eq!(rows[1]["Data"][1]["VarCharValue"], pseudonyms.pseudonym("web-prod"));
        assert_eq!(
            rows[1]["Data"][2],
            serde_json::json!({}),
            "Team is denied: the cell becomes null"
        );
        assert_eq!(rows[2]["Data"][1], serde_json::json!({}), "a null stays null");
        assert_eq!(stats.tags["Team"].1, 2);
    }

    #[test]
    fn the_environment_is_derived_before_the_name_is_pseudonymized() {
        let config = config(
            "[tags]\ndefault = \"deny\"\npseudonymize = [\"Name\"]\n[environment]\n[[environment.rules]]\npattern = \"(?i)prod\"\nvalue = \"production\"\n",
        );
        let pseudonyms = Pseudonymizer::new(vec![7; 32], false);
        let xml = "<R><tagSet><item><key>Name</key><value>web-prod-1</value></item></tagSet></R>";
        let mut records = vec![record(xml, "", "x")];
        let stats = Redactor::new(&config, &pseudonyms).records(&mut records);
        let out = &records[0].response.body;
        assert!(!out.contains("web-prod-1"), "{out}");
        assert!(
            out.contains("<key>qloudpulse:environment</key><value>production</value>"),
            "kept under deny: {out}"
        );
        assert_eq!(stats.fields["derived qloudpulse:environment"], 1);
    }

    #[test]
    fn encrypted_tags_and_fields_open_with_the_view_token_only() {
        let config = config(
            "[tags]\ndefault = \"deny\"\nencrypt = [\"Owner\"]\n[[fields]]\npath = \"TableNames\"\naction = \"encrypt\"\n",
        );
        assert!(config.encrypts());
        let token = crate::view_token::ViewToken::generate();
        let pseudonyms = Pseudonymizer::new(vec![7; 32], false).with_view_token(&token);
        let xml = "<R><tagSet><item><key>Owner</key><value>alice@corp</value></item></tagSet></R>";
        let mut records = vec![
            record(xml, "", "x"),
            record(r#"{"TableNames":["orders"]}"#, "{}", "application/x-amz-json-1.0"),
            record("{}", r#"{"TableName":"orders"}"#, "application/x-amz-json-1.0"),
        ];
        let stats = Redactor::new(&config, &pseudonyms).records(&mut records);
        let cipher = token.cipher();
        let (owner, table) = (cipher.seal("alice@corp"), cipher.seal("orders"));
        assert!(records[0].response.body.contains(&format!("<value>{owner}</value>")));
        assert!(!records[0].response.body.contains("alice"));
        assert_eq!(cipher.open(&owner).unwrap(), "alice@corp");
        assert_eq!(records[1].response.body, format!(r#"{{"TableNames":["{table}"]}}"#));
        assert_eq!(
            records[2].request.body,
            format!(r#"{{"TableName":"{table}"}}"#),
            "carried into the request, so the replay matches"
        );
        assert_eq!(stats.tags["Owner"], ("encrypt".to_string(), 1));
        assert_eq!(pseudonyms.view_key_id(), Some(token.key_id()));
    }

    #[test]
    fn without_a_view_token_an_encrypted_value_is_dropped_not_sent() {
        let config = config("[tags]\ndefault = \"allow\"\nencrypt = [\"Owner\"]\n");
        let pseudonyms = Pseudonymizer::new(vec![7; 32], false);
        let xml = "<R><tagSet><item><key>Owner</key><value>alice@corp</value></item></tagSet></R>";
        let mut records = vec![record(xml, "", "x")];
        Redactor::new(&config, &pseudonyms).records(&mut records);
        assert!(
            !records[0].response.body.contains("alice"),
            "{}",
            records[0].response.body
        );
    }

    #[test]
    fn untouched_answers_keep_their_exact_bytes() {
        let config = config("[tags]\ndefault = \"allow\"\n");
        let pseudonyms = Pseudonymizer::new(vec![7; 32], false);
        let xml = "<R>\n  <a>1</a>\n</R>";
        let mut records = vec![record(xml, "", "x")];
        Redactor::new(&config, &pseudonyms).records(&mut records);
        assert_eq!(records[0].response.body, xml);
    }
}

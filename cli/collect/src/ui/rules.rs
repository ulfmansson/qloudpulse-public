//! The rules editor's model: the form becomes a config through the same `check()` as the CLI, the file is rewritten
//! with `toml_edit` so its comments and every other section stay as they were, and the preview runs the redactor
//! `pack` runs.

use crate::collect::Outcome;
use crate::config::{Config, FieldAction, FieldRule, TagAction, TagDefault, TagReplace, TagRules};
use crate::identifiers::Identifiers;
use crate::pseudonym::Pseudonymizer;
use crate::record::{Record, dedupe};
use crate::redact::{Redactor, Stats};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value, value};

pub const TAG_ACTIONS: &[&str] = &["default", "keep", "drop", "pseudonymize", "encrypt", "replace"];
pub const FIELD_ACTIONS: &[&str] = &["drop", "replace", "pseudonymize", "encrypt"];

/// One tag key as the form shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRow {
    pub key: String,
    /// One of [`TAG_ACTIONS`]: `default` means no list names the key.
    pub action: String,
    pub pattern: String,
    pub with: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRow {
    pub path: String,
    pub action: String,
    pub pattern: String,
    pub with: String,
}

/// The editor's state: what the form says, before it is a config.
#[derive(Debug, Clone)]
pub struct Draft {
    pub default: TagDefault,
    pub tags: Vec<TagRow>,
    pub fields: Vec<FieldRow>,
}

impl Draft {
    /// The rows for a config: every key it names, then `extra` keys (found in the sample, or read by qloudpulse).
    pub fn of(config: &Config, extra: &[String]) -> Draft {
        let t = &config.tags;
        let mut keys: Vec<String> = Vec::new();
        for k in t
            .keep
            .iter()
            .chain(&t.drop)
            .chain(&t.pseudonymize)
            .chain(&t.encrypt)
            .chain(t.replace.iter().map(|r| &r.key))
            .chain(extra)
        {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
        let tags = keys
            .into_iter()
            .map(|key| {
                let (action, pattern, with) = match config.tag_action(&key) {
                    TagAction::Replace(i) => (
                        "replace",
                        t.replace[i].pattern.clone().unwrap_or_default(),
                        t.replace[i].with.clone(),
                    ),
                    _ if !named(t, &key) => ("default", String::new(), String::new()),
                    a => (a.label(), String::new(), String::new()),
                };
                TagRow {
                    key,
                    action: action.to_string(),
                    pattern,
                    with,
                }
            })
            .collect();
        let fields = config
            .fields
            .iter()
            .map(|f| FieldRow {
                path: f.path.clone(),
                action: action_name(f.action).to_string(),
                pattern: f.pattern.clone().unwrap_or_default(),
                with: f.with.clone().unwrap_or_default(),
            })
            .collect();
        Draft {
            default: t.default,
            tags,
            fields,
        }
    }

    /// The form's fields: `default`, `k0`/`a0`/`p0`/`w0` per tag key, `newkey`/`newaction`, and
    /// `fp0`/`fa0`/`fpat0`/`fw0` per field rule (an empty path is no rule).
    pub fn from_form(form: &HashMap<String, String>) -> Draft {
        let get = |k: &str| form.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
        let raw = |k: &str| form.get(k).cloned().unwrap_or_default();
        let mut tags = Vec::new();
        let mut i = 0;
        while let Some(key) = form.get(&format!("k{i}")) {
            tags.push(TagRow {
                key: key.clone(),
                action: get(&format!("a{i}")),
                pattern: raw(&format!("p{i}")),
                with: raw(&format!("w{i}")),
            });
            i += 1;
        }
        let new = get("newkey");
        if !new.is_empty() && !tags.iter().any(|t| t.key == new) {
            let action = get("newaction");
            tags.push(TagRow {
                key: new,
                action: if action.is_empty() { "keep".into() } else { action },
                pattern: String::new(),
                with: String::new(),
            });
        }
        let mut fields = Vec::new();
        let mut j = 0;
        while let Some(path) = form.get(&format!("fp{j}")) {
            if !path.trim().is_empty() {
                fields.push(FieldRow {
                    path: path.trim().to_string(),
                    action: get(&format!("fa{j}")),
                    pattern: raw(&format!("fpat{j}")),
                    with: raw(&format!("fw{j}")),
                });
            }
            j += 1;
        }
        Draft {
            default: if get("default") == "allow" {
                TagDefault::Allow
            } else {
                TagDefault::Deny
            },
            tags,
            fields,
        }
    }

    /// `base` with this draft's `[tags]` and `[[fields]]`, checked as the CLI checks a config. A list keeps the
    /// order it had in the file; keys new to it go at its end.
    pub fn apply(&self, base: &Config) -> Result<Config> {
        let mut config = base.clone();
        let old = &base.tags;
        let order = |old: &[String], action: &str| -> Vec<String> {
            let wanted = |k: &String| self.tags.iter().any(|t| &t.key == k && t.action == action);
            let mut out: Vec<String> = Vec::new();
            for k in old.iter().filter(|k| wanted(k)) {
                if !out.contains(k) {
                    out.push(k.clone());
                }
            }
            for t in self.tags.iter().filter(|t| t.action == action) {
                if !out.contains(&t.key) {
                    out.push(t.key.clone());
                }
            }
            out
        };
        let mut replace: Vec<TagReplace> = Vec::new();
        for t in self.tags.iter().filter(|t| t.action == "replace") {
            if replace.iter().any(|r| r.key == t.key) {
                continue;
            }
            replace.push(TagReplace {
                key: t.key.clone(),
                pattern: (!t.pattern.is_empty()).then(|| t.pattern.clone()),
                with: t.with.clone(),
            });
        }
        for t in &self.tags {
            if !TAG_ACTIONS.contains(&t.action.as_str()) {
                bail!("tag {}: unknown action {}", t.key, t.action);
            }
        }
        config.tags = TagRules {
            default: self.default,
            keep: order(&old.keep, "keep"),
            drop: order(&old.drop, "drop"),
            pseudonymize: order(&old.pseudonymize, "pseudonymize"),
            encrypt: order(&old.encrypt, "encrypt"),
            replace,
        };
        config.fields = self
            .fields
            .iter()
            .map(|f| {
                let action = match f.action.as_str() {
                    "drop" => FieldAction::Drop,
                    "replace" => FieldAction::Replace,
                    "pseudonymize" => FieldAction::Pseudonymize,
                    "encrypt" => FieldAction::Encrypt,
                    other => bail!("the rule for {}: unknown action {other}", f.path),
                };
                Ok(FieldRule {
                    path: f.path.clone(),
                    action,
                    pattern: (!f.pattern.is_empty()).then(|| f.pattern.clone()),
                    with: (action == FieldAction::Replace && !f.with.is_empty()).then(|| f.with.clone()),
                })
            })
            .collect::<Result<_>>()?;
        config.check()?;
        Ok(config)
    }
}

fn named(t: &TagRules, key: &str) -> bool {
    t.keep
        .iter()
        .chain(&t.drop)
        .chain(&t.pseudonymize)
        .chain(&t.encrypt)
        .any(|k| k == key)
        || t.replace.iter().any(|r| r.key == key)
}

pub fn action_name(a: FieldAction) -> &'static str {
    match a {
        FieldAction::Drop => "drop",
        FieldAction::Replace => "replace",
        FieldAction::Pseudonymize => "pseudonymize",
        FieldAction::Encrypt => "encrypt",
    }
}

/// Keys the file names in more than one list. Only one action applies (drop, pseudonymize, encrypt, replace, keep,
/// in that order); saving from the editor keeps the key in that list only.
pub fn conflicts(t: &TagRules) -> Vec<String> {
    let mut out = Vec::new();
    let lists: [(&str, Vec<&String>); 5] = [
        ("drop", t.drop.iter().collect()),
        ("pseudonymize", t.pseudonymize.iter().collect()),
        ("encrypt", t.encrypt.iter().collect()),
        ("replace", t.replace.iter().map(|r| &r.key).collect()),
        ("keep", t.keep.iter().collect()),
    ];
    let mut seen: Vec<&String> = Vec::new();
    for (_, keys) in &lists {
        for k in keys {
            if seen.contains(k) {
                continue;
            }
            let found: Vec<&str> = lists.iter().filter(|(_, ks)| ks.contains(k)).map(|(n, _)| *n).collect();
            if found.len() > 1 {
                out.push(format!(
                    "{k} is in {}: {} applies, and saving here keeps it in {} only",
                    found.join(" and "),
                    found[0],
                    found[0]
                ));
            }
            seen.push(k);
        }
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The file with `new`'s `[tags]` and `[[fields]]` written in: what is unchanged is left exactly as it was, comments
/// included. A list that changes is rewritten, so comments inside that list are lost.
pub fn edited_toml(original: &str, old: &Config, new: &Config) -> Result<String> {
    let mut doc: DocumentMut = original.parse().context("the config is not valid TOML")?;
    if !same(&old.tags, &new.tags) {
        if !doc.contains_key("tags") {
            doc["tags"] = Item::Table(Table::new());
        }
        let t = doc["tags"]
            .as_table_mut()
            .context("[tags] is not a plain table; edit it by hand")?;
        let default = match new.tags.default {
            TagDefault::Deny => "deny",
            TagDefault::Allow => "allow",
        };
        if old.tags.default != new.tags.default || !t.contains_key("default") {
            set_keeping_decor(t, "default", Value::from(default));
        }
        set_list(t, "keep", &old.tags.keep, &new.tags.keep);
        set_list(t, "drop", &old.tags.drop, &new.tags.drop);
        set_list(t, "pseudonymize", &old.tags.pseudonymize, &new.tags.pseudonymize);
        set_list(t, "encrypt", &old.tags.encrypt, &new.tags.encrypt);
        if !same(&old.tags.replace, &new.tags.replace) {
            if new.tags.replace.is_empty() {
                t.remove("replace");
            } else {
                let mut aot = ArrayOfTables::new();
                for r in &new.tags.replace {
                    let mut row = Table::new();
                    row["key"] = value(r.key.as_str());
                    if let Some(p) = &r.pattern {
                        row["pattern"] = value(p.as_str());
                    }
                    row["with"] = value(r.with.as_str());
                    aot.push(row);
                }
                t["replace"] = Item::ArrayOfTables(aot);
            }
        }
    }
    if !same(&old.fields, &new.fields) {
        if new.fields.is_empty() {
            doc.remove("fields");
        } else {
            let mut aot = ArrayOfTables::new();
            for f in &new.fields {
                let mut row = Table::new();
                row["path"] = value(f.path.as_str());
                row["action"] = value(action_name(f.action));
                if let Some(p) = &f.pattern {
                    row["pattern"] = value(p.as_str());
                }
                if let Some(w) = &f.with {
                    row["with"] = value(w.as_str());
                }
                aot.push(row);
            }
            doc["fields"] = Item::ArrayOfTables(aot);
        }
    }
    Ok(doc.to_string())
}

/// Compares config parts that do not implement `PartialEq`.
fn same<T: serde::Serialize>(a: &T, b: &T) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

fn set_keeping_decor(t: &mut Table, key: &str, mut v: Value) {
    if let Some(Item::Value(old)) = t.get(key) {
        *v.decor_mut() = old.decor().clone();
    }
    t[key] = Item::Value(v);
}

fn set_list(t: &mut Table, key: &str, old: &[String], new: &[String]) {
    if old == new && t.contains_key(key) {
        return;
    }
    let multiline = t.get(key).is_some_and(|i| i.to_string().contains('\n'));
    let mut arr = Array::new();
    for v in new {
        arr.push(v.as_str());
    }
    if multiline && !new.is_empty() {
        for v in arr.iter_mut() {
            v.decor_mut().set_prefix("\n  ");
            v.decor_mut().set_suffix("");
        }
        arr.set_trailing("\n");
        arr.set_trailing_comma(true);
    } else {
        arr.fmt();
    }
    set_keeping_decor(t, key, Value::Array(arr));
}

/// Writes `text` over the config, unless the file changed since the page read it (`base_sha`). The new file is
/// loaded again before the call returns; if it does not load, the old one is put back.
pub fn save(path: &Path, base_sha: &str, text: &str) -> Result<()> {
    let current = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if sha256_hex(&current) != base_sha {
        bail!(
            "{} changed on disk since this page read it; nothing was saved. Reload the page to edit the new file.",
            path.display()
        );
    }
    let tmp = path.with_extension("toml.ui-partial");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, meta.permissions())?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    if let Err(e) = Config::load(path) {
        std::fs::write(path, &current)?;
        return Err(e.context("the saved file did not load, so the old one is back"));
    }
    Ok(())
}

/// What `pack` does to records before it writes them: drop the planning calls, redact, rewrite identifiers.
pub fn redact(
    config: &Config,
    pseudonyms: &Pseudonymizer,
    records: Vec<Record>,
    outcome: &Outcome,
) -> (Vec<Record>, Stats) {
    let mut records: Vec<Record> = dedupe(records)
        .into_iter()
        .filter(|r| r.collector != "client")
        .collect();
    let stats = Redactor::new(config, pseudonyms).records(&mut records);
    let mut outcome = outcome.clone();
    Identifiers {
        accounts: config.identifiers.account_ids,
        resources: config.identifiers.resource_ids,
        pseudonyms,
    }
    .apply(&mut records, &mut outcome);
    (records, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::starter;

    fn parse(text: &str) -> Config {
        let c: Config = toml::from_str(text).unwrap();
        c.check().unwrap();
        c
    }

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn edits_keep_comments_and_other_sections() {
        let text = starter();
        let old = parse(&text);
        let mut draft = Draft::of(&old, &["Owner".to_string()]);
        for t in &mut draft.tags {
            if t.key == "Team" {
                t.action = "drop".into();
            }
            if t.key == "Owner" {
                t.action = "pseudonymize".into();
            }
        }
        draft.fields.push(FieldRow {
            path: "privateDnsName".into(),
            action: "pseudonymize".into(),
            pattern: String::new(),
            with: String::new(),
        });
        let new = draft.apply(&old).unwrap();
        let out = edited_toml(&text, &old, &new).unwrap();
        let reread = parse(&out);
        assert_eq!(reread.tag_action("Team"), TagAction::Drop);
        assert_eq!(reread.tag_action("Owner"), TagAction::Pseudonymize);
        assert_eq!(reread.fields.len(), 1);
        // Everything else, comments included, is as it was.
        assert!(
            out.contains("# Names this client. A company may run several; each AWS account belongs to one source.")
        );
        assert!(out.contains("[run]\nevery_hours = 1\nminute = 5"));
        assert!(out.contains("# A tag no rule names is dropped (\"deny\") or kept (\"allow\")."));
        assert!(
            out.contains("the same value always gives the same pseudonym"),
            "the comment after the list stays"
        );
        // Saving the same rules again changes nothing.
        let again = edited_toml(&out, &reread, &reread).unwrap();
        assert_eq!(again, out);
    }

    #[test]
    fn a_rule_on_a_priced_field_is_refused() {
        let old = parse(&starter());
        let draft = Draft::from_form(&form(&[
            ("default", "deny"),
            ("fp0", "instancesSet.instanceType"),
            ("fa0", "drop"),
        ]));
        let err = draft.apply(&old).unwrap_err().to_string();
        assert!(err.contains("prices from"), "{err}");
    }

    #[test]
    fn form_round_trip_and_replace() {
        let old = parse(&starter());
        let draft = Draft::from_form(&form(&[
            ("default", "allow"),
            ("k0", "CostCenter"),
            ("a0", "replace"),
            ("p0", "^(\\d+)-.*$"),
            ("w0", "$1"),
            ("k1", "Name"),
            ("a1", "default"),
            ("newkey", "Owner"),
            ("newaction", "drop"),
            ("fp0", "keyName"),
            ("fa0", "replace"),
            ("fw0", "k"),
            ("fp1", ""),
        ]));
        let c = draft.apply(&old).unwrap();
        assert_eq!(c.tags.default, TagDefault::Allow);
        assert_eq!(c.tags.replace[0].with, "$1");
        assert_eq!(c.tag_action("Owner"), TagAction::Drop);
        assert!(
            !c.tags.keep.contains(&"Name".to_string()),
            "default takes it out of keep"
        );
        assert_eq!(c.fields.len(), 1);
        let replace_without_with = Draft::from_form(&form(&[("fp0", "keyName"), ("fa0", "replace")]));
        assert!(replace_without_with.apply(&old).is_err());
    }

    #[test]
    fn a_key_in_two_lists_is_reported() {
        let c = parse("source_id = \"a\"\n[tags]\nkeep = [\"Team\"]\ndrop = [\"Team\"]\n");
        let found = conflicts(&c.tags);
        assert_eq!(found.len(), 1);
        assert!(
            found[0].starts_with("Team is in drop and keep: drop applies"),
            "{found:?}"
        );
        let draft = Draft::of(&c, &[]);
        assert_eq!(draft.tags[0].action, "drop");
    }

    #[test]
    fn save_refuses_a_file_that_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, starter()).unwrap();
        let sha = sha256_hex(starter().as_bytes());
        std::fs::write(&path, format!("{}\n# edited elsewhere\n", starter())).unwrap();
        let err = save(&path, &sha, &starter()).unwrap_err().to_string();
        assert!(err.contains("changed on disk"), "{err}");
        let sha = sha256_hex(&std::fs::read(&path).unwrap());
        save(&path, &sha, &starter()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), starter());
        let err = save(&path, &sha256_hex(starter().as_bytes()), "source_id = \"bad id!\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("old one is back"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), starter());
    }
}

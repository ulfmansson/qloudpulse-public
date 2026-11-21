//! The four views. Each page reads the config and the files afresh, so what it shows is what is on disk now; the
//! only thing held between requests is the raw sample, in memory.

use super::diff::{self, Row};
use super::rules::{self, Draft, FIELD_ACTIONS, TAG_ACTIONS, sha256_hex};
use super::state::{Sample, Shared, lock};
use crate::analyze;
use crate::bundle::{self, Manifest};
use crate::config::{Config, FEATURE_TAGS, REQUIRED_FIELDS};
use crate::pseudonym::Pseudonymizer;
use crate::record::Record;
use crate::scan::{ScanReport, Severity};
use anyhow::{Context, Result, bail};
use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RECORDS_PER_PAGE: usize = 50;

// ---------------------------------------------------------------- shared parts

pub struct Head {
    pub title: &'static str,
    pub active: &'static str,
    pub version: &'static str,
    pub bind: String,
    pub token_tail: String,
    pub source_id: String,
    pub fingerprint: String,
    pub config_path: String,
    pub run_line: String,
    pub csrf: String,
    /// Reload the page every few seconds, while a sample is being collected.
    pub refresh: bool,
    /// A newer release of the client, when one is out.
    pub newer: Option<String>,
}

fn head(s: &Shared, title: &'static str, active: &'static str, config: Option<&Config>) -> Head {
    let fingerprint = match config {
        Some(c) => Pseudonymizer::load(&c.key_file, false)
            .map(|p| p.fingerprint())
            .unwrap_or_else(|_| "no key yet: run init".into()),
        None => "unknown".into(),
    };
    let run_line = match &s.run {
        Some(run) => {
            let r = lock(run);
            match (&r.next, r.cycles.front()) {
                (None, Some(c)) if c.finished.is_none() => {
                    format!(
                        "under run --every {}h · a cycle is running since {}",
                        r.every_hours,
                        hm(c.started)
                    )
                }
                (Some(next), _) => format!("under run --every {}h · next run {} UTC", r.every_hours, hm(*next)),
                _ => format!("under run --every {}h", r.every_hours),
            }
        }
        None => "ui only: this process runs no collection".into(),
    };
    Head {
        title,
        active,
        version: VERSION,
        bind: s.bind.to_string(),
        token_tail: s.token[s.token.len() - 4..].to_string(),
        source_id: config.map(|c| c.source_id.clone()).unwrap_or_else(|| "?".into()),
        fingerprint,
        config_path: s.config_path.display().to_string(),
        run_line,
        csrf: s.csrf.clone(),
        refresh: false,
        newer: lock(&s.newer).clone(),
    }
}

#[derive(Template)]
#[template(path = "message.html")]
struct MessageView<'a> {
    title: &'a str,
    text: &'a str,
}

/// A page with no header, for answers before a session exists.
pub fn message(title: &str, text: &str) -> String {
    MessageView { title, text }
        .render()
        .unwrap_or_else(|_| text.to_string())
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorView {
    head: Head,
    error: String,
}

fn html(t: impl Template) -> Response {
    match t.render() {
        Ok(body) => Html(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(message("The page did not render", &e.to_string())),
        )
            .into_response(),
    }
}

/// A page, or the reason it could not be shown, inside the usual header.
fn respond(s: &Shared, active: &'static str, page: Result<Response>) -> Response {
    match page {
        Ok(r) => r,
        Err(e) => {
            let config = Config::load(&s.config_path).ok().map(|(c, _)| c);
            let mut h = head(s, "Error", active, config.as_ref());
            h.title = "Error";
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                html(ErrorView {
                    head: h,
                    error: format!("{e:#}"),
                }),
            )
                .into_response()
        }
    }
}

fn load(s: &Shared) -> Result<(Config, Vec<u8>)> {
    Config::load(&s.config_path)
}

fn num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn hm(t: Timestamp) -> String {
    t.strftime("%H:%M").to_string()
}

fn when(t: Timestamp) -> String {
    t.strftime("%Y-%m-%d %H:%M UTC").to_string()
}

fn window(m: &crate::window::Window) -> String {
    format!(
        "{} – {} UTC",
        m.start.strftime("%Y-%m-%d %H:%M"),
        m.end.strftime("%H:%M")
    )
}

fn ago(t: Timestamp) -> String {
    let secs = Timestamp::now().duration_since(t).as_secs().max(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        _ => format!("{} h {} min ago", secs / 3600, (secs % 3600) / 60),
    }
}

fn action_class(action: &str) -> &'static str {
    match action {
        "keep" => "p-keep",
        "drop" => "p-drop",
        "pseudonymize" => "p-pseudonymize",
        "replace" => "p-replace",
        _ => "p-default",
    }
}

// ---------------------------------------------------------------- bundles on disk

/// Which target has which bundle, from `<output_dir>/.delivered.json` (written by `deliver`).
pub fn delivered(output_dir: &Path) -> BTreeMap<String, BTreeSet<String>> {
    std::fs::read(output_dir.join(".delivered.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Bundle file names, newest first.
fn bundle_names(config: &Config) -> Result<Vec<String>> {
    let mut names: Vec<String> = crate::deliver::bundles(&config.output_dir)?
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    names.reverse();
    Ok(names)
}

/// A bundle named by a page: a plain `.tgz` file name in `output_dir`, nothing else.
fn bundle_path(config: &Config, name: &str) -> Result<PathBuf> {
    if name.contains(['/', '\\']) || name.starts_with('.') || !name.ends_with(".tgz") {
        bail!("{name} is not a bundle name");
    }
    let path = config.output_dir.join(name);
    if !path.is_file() {
        bail!("no bundle {name} in {}", config.output_dir.display());
    }
    Ok(path)
}

/// The manifest alone, which `pack` writes first: enough for a list, without reading the whole bundle. Its sums are
/// checked when the bundle is opened.
fn read_manifest(path: &Path) -> Result<Manifest> {
    let file = std::fs::File::open(path)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut entry = archive.entries()?.next().context("an empty bundle")??;
    if entry.path()?.to_string_lossy() != "manifest.json" {
        bail!("the bundle does not start with its manifest");
    }
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub struct Delivery {
    pub target: String,
    pub done: bool,
}

fn deliveries(config: &Config, state: &BTreeMap<String, BTreeSet<String>>, name: &str) -> Vec<Delivery> {
    let done = state.get(name).cloned().unwrap_or_default();
    crate::deliver::targets(config)
        .into_iter()
        .map(|t| Delivery {
            target: t.to_string(),
            done: done.contains(t),
        })
        .collect()
}

// ---------------------------------------------------------------- Analyze

pub struct Card {
    pub n: String,
    pub label: String,
    pub class: &'static str,
}

pub struct TagRowView {
    pub key: String,
    pub count: String,
    pub distinct: String,
    pub samples: String,
    pub action: String,
    pub action_class: &'static str,
    pub feature: String,
    pub operations: String,
}

pub struct ImpactRow {
    pub text: String,
    pub absent: bool,
}

pub struct ScanRow {
    pub kind: String,
    pub count: String,
    pub state: &'static str,
    pub class: &'static str,
    pub operations: String,
    pub samples: String,
}

pub struct CountRow {
    pub name: String,
    pub count: String,
    pub width: u32,
}

pub struct ReportView {
    pub cards: Vec<Card>,
    pub tags: Vec<TagRowView>,
    pub impact: Vec<ImpactRow>,
    pub scan: Vec<ScanRow>,
    pub scan_label: String,
    pub scan_after: Option<Vec<ScanRow>>,
    pub scan_after_note: String,
    pub errors: Vec<CountRow>,
    pub operations: Vec<CountRow>,
}

fn scan_rows(r: &ScanReport) -> Vec<ScanRow> {
    r.kinds
        .iter()
        .map(|(kind, k)| {
            let (state, class) = match (k.severity, k.allowed) {
                (_, true) => ("allowed in [scan]", "muted"),
                (Some(Severity::Block), _) => ("blocks pack", "bad"),
                _ => ("warning", "warn"),
            };
            ScanRow {
                kind: kind.clone(),
                count: num(k.count),
                state,
                class,
                operations: k.operations.iter().cloned().collect::<Vec<_>>().join(", "),
                samples: k.samples.join(", "),
            }
        })
        .collect()
}

fn report_view(
    config: &Config,
    records: &[Record],
    accounts: usize,
    regions: usize,
    show_values: bool,
    raw: bool,
) -> ReportView {
    let r = analyze::analyze("", records, Some(config), show_values);
    let present: BTreeSet<&str> = r.tags.iter().map(|t| t.key.as_str()).collect();
    let errors: usize = r.errors.values().sum();
    let blocking = r.scan.blocking().len();
    let max = r.operations.values().copied().max().unwrap_or(1).max(1);
    let mut operations: Vec<(&String, &usize)> = r.operations.iter().collect();
    operations.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    ReportView {
        cards: vec![
            Card {
                n: num(r.records),
                label: "recorded calls".into(),
                class: "",
            },
            Card {
                n: num(accounts),
                label: "accounts".into(),
                class: "",
            },
            Card {
                n: num(regions),
                label: "regions".into(),
                class: "",
            },
            Card {
                n: num(r.operations.len()),
                label: "operations".into(),
                class: "",
            },
            Card {
                n: num(r.tags.len()),
                label: "tag keys".into(),
                class: "",
            },
            Card {
                n: num(errors),
                label: "errors recorded".into(),
                class: if errors > 0 { "warn" } else { "" },
            },
            Card {
                n: num(blocking),
                label: if raw {
                    "scan kinds that block, before rules"
                } else {
                    "scan kinds that block"
                }
                .into(),
                class: if blocking > 0 { "bad" } else { "" },
            },
            Card {
                n: num(r.feature_impact.len()),
                label: "features the rules affect".into(),
                class: if r.feature_impact.is_empty() { "" } else { "warn" },
            },
        ],
        tags: r
            .tags
            .iter()
            .map(|t| {
                let action = t.action.clone().unwrap_or_default();
                TagRowView {
                    key: t.key.clone(),
                    count: num(t.count),
                    distinct: num(t.distinct_values),
                    samples: t.samples.join(", "),
                    action_class: action_class(&action),
                    action,
                    feature: t.feature.clone().unwrap_or_default(),
                    operations: t.operations.iter().cloned().collect::<Vec<_>>().join(", "),
                }
            })
            .collect(),
        impact: r
            .feature_impact
            .iter()
            .map(|text| ImpactRow {
                absent: !FEATURE_TAGS
                    .iter()
                    .any(|(k, _)| text.contains(&format!(" tag {k} ")) && present.contains(k)),
                text: text.clone(),
            })
            .collect(),
        scan: scan_rows(&r.scan),
        scan_label: if raw {
            "In the raw sample, before your rules".into()
        } else {
            "In the bundle, as it left".into()
        },
        scan_after: None,
        scan_after_note: String::new(),
        errors: r
            .errors
            .iter()
            .map(|(k, n)| CountRow {
                name: k.clone(),
                count: num(*n),
                width: 0,
            })
            .collect(),
        operations: operations
            .into_iter()
            .map(|(k, n)| CountRow {
                name: k.clone(),
                count: num(*n),
                width: ((*n * 100) / max).max(1) as u32,
            })
            .collect(),
    }
}

pub struct SourceOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// Raw snapshots in `work_dir/raw`, newest first: labels only.
fn raw_snapshots(config: &Config) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(config.work_dir.join("raw"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.path().join("snapshot.json").is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out.reverse();
    out
}

#[derive(Template)]
#[template(path = "analyze.html")]
struct AnalyzeView {
    head: Head,
    snapshots: Vec<String>,
    sources: Vec<SourceOption>,
    source: String,
    sample_state: &'static str,
    sample_line: String,
    report: Option<ReportView>,
    show_values: bool,
    note: String,
}

fn sample_status(s: &Shared) -> (&'static str, String) {
    match &*lock(&s.sample) {
        Sample::None => (
            "none",
            "No sample in memory. Collect one to see raw tag values and to preview rules on real answers.".into(),
        ),
        Sample::Collecting { since } => (
            "collecting",
            format!(
                "Collecting into memory since {} UTC. Under run, a running cycle finishes first.",
                hm(*since)
            ),
        ),
        Sample::Ready {
            at,
            origin,
            records,
            meta,
        } => (
            "ready",
            format!(
                "In memory since {} ({}), {origin}: {} records from {} accounts in {} regions. Held in memory only; \
                 gone when the UI stops or you press Forget.",
                when(*at),
                ago(*at),
                num(records.len()),
                meta.outcome.accounts.len(),
                meta.outcome.regions.len()
            ),
        ),
        Sample::Failed { at, error } => ("failed", format!("The sample at {} failed: {error}", when(*at))),
    }
}

pub fn analyze(s: &Shared, q: &HashMap<String, String>) -> Response {
    respond(s, "analyze", analyze_page(s, q))
}

fn analyze_page(s: &Shared, q: &HashMap<String, String>) -> Result<Response> {
    let (config, _) = load(s)?;
    let source = q.get("source").cloned().unwrap_or_else(|| "sample".into());
    let show_values = q.get("values").is_none_or(|v| v != "0");
    let mut sources = vec![SourceOption {
        value: "sample".into(),
        label: "Raw sample, in memory".into(),
        selected: source == "sample",
    }];
    for name in bundle_names(&config)? {
        sources.push(SourceOption {
            selected: source == name,
            label: format!("{name}, as it left"),
            value: name,
        });
    }
    let (sample_state, sample_line) = sample_status(s);
    let mut note = String::new();
    let report = if source == "sample" {
        match &*lock(&s.sample) {
            Sample::Ready { meta, records, .. } => {
                let mut view = report_view(
                    &config,
                    records,
                    meta.outcome.accounts.len(),
                    meta.outcome.regions.len(),
                    show_values,
                    true,
                );
                match Pseudonymizer::load(&config.key_file, false) {
                    Ok(p) => {
                        let (redacted, _) = rules::redact(&config, &p, records.clone(), &meta.outcome);
                        let after = crate::scan::scan(&redacted, &config.scan);
                        view.scan_after_note = if after.blocking().is_empty() {
                            "Nothing that blocks pack is left after your rules.".into()
                        } else {
                            "pack refuses until each of these is redacted by a rule or allowed in [scan] \
                             (or --allow-findings)."
                                .into()
                        };
                        view.scan_after = Some(scan_rows(&after));
                    }
                    Err(e) => view.scan_after_note = format!("Not checked after your rules: {e:#}"),
                }
                Some(view)
            }
            _ => None,
        }
    } else {
        let opened = bundle::open(&bundle_path(&config, &source)?)?;
        note = format!(
            "Reading {source}: every file matched its manifest SHA-256. These are the records exactly as they left."
        );
        Some(report_view(
            &config,
            &opened.records,
            opened.manifest.accounts.len(),
            opened.manifest.regions.len(),
            show_values,
            false,
        ))
    };
    let mut h = head(s, "Analyze", "analyze", Some(&config));
    h.refresh = sample_state == "collecting";
    Ok(html(AnalyzeView {
        head: h,
        snapshots: raw_snapshots(&config),
        sources,
        source,
        sample_state,
        sample_line,
        report,
        show_values,
        note,
    }))
}

// ---------------------------------------------------------------- Rules

/// An `<option>`: value, label, selected.
pub struct Choice {
    pub value: &'static str,
    pub label: String,
    pub selected: bool,
}

fn choices(all: &[&'static str], selected: &str, default_label: &str) -> Vec<Choice> {
    all.iter()
        .map(|a| Choice {
            value: a,
            label: if *a == "default" {
                default_label.to_string()
            } else {
                a.to_string()
            },
            selected: *a == selected,
        })
        .collect()
}

pub struct TagRuleView {
    pub i: usize,
    pub key: String,
    pub in_sample: String,
    pub replace: bool,
    pub actions: Vec<Choice>,
    pub pattern: String,
    pub with: String,
    pub cost: String,
    pub cost_class: &'static str,
}

pub struct FieldRuleView {
    pub j: usize,
    pub path: String,
    pub actions: Vec<Choice>,
    pub pattern: String,
    pub with: String,
}

pub struct RecordOption {
    pub i: usize,
    pub label: String,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "preview.html")]
pub struct PreviewView {
    pub label: String,
    pub note: String,
    pub rows: Vec<Row>,
    pub stats: String,
}

#[derive(Template)]
#[template(path = "toml.html")]
pub struct TomlView {
    pub lines: Vec<(String, bool)>,
    pub changed: bool,
}

#[derive(Template)]
#[template(path = "rule_errors.html")]
pub struct RuleErrorsView {
    pub errors: Vec<String>,
    pub impact: Vec<String>,
}

#[derive(Template)]
#[template(path = "rules.html")]
struct RulesView {
    head: Head,
    base_sha: String,
    allow: bool,
    tags: Vec<TagRuleView>,
    fields: Vec<FieldRuleView>,
    blank_field: usize,
    blank_actions: Vec<Choice>,
    new_actions: Vec<Choice>,
    required: String,
    conflicts: Vec<String>,
    saved: bool,
    save_error: String,
    can_save: bool,
    /// The draft checks out: it can be downloaded or copied, changed or not.
    valid: bool,
    /// The config's directory takes a new file. Not on a Kubernetes ConfigMap, which is mounted read-only.
    writable: bool,
    file_name: String,
    has_sample: bool,
    records: Vec<RecordOption>,
    preview: String,
    toml: String,
    errors: String,
}

/// What the rules page shows for a draft: its config (when it checks out), the preview, the file and the errors.
struct Parts {
    preview: String,
    toml: String,
    errors: String,
    can_save: bool,
    valid: bool,
}

fn parts(s: &Shared, config: &Config, text: &str, draft: &Draft, record: Option<usize>) -> Parts {
    let (new, mut errors) = match draft.apply(config) {
        Ok(c) => (Some(c), vec![]),
        Err(e) => (None, vec![format!("{e:#}")]),
    };
    let mut changed = false;
    let toml = match &new {
        Some(n) => match rules::edited_toml(text, config, n) {
            Ok(out) => {
                changed = out != text;
                let lines = diff::side_by_side(text, &out)
                    .into_iter()
                    .filter_map(|r| r.right.clone().map(|l| (l, r.kind != diff::Kind::Same)))
                    .collect();
                TomlView { lines, changed }.render().unwrap_or_default()
            }
            Err(e) => {
                errors.push(format!("{e:#}"));
                String::new()
            }
        },
        None => String::new(),
    };
    let preview = preview(s, new.as_ref(), record).render().unwrap_or_default();
    let impact = new.as_ref().map(Config::feature_impact).unwrap_or_default();
    Parts {
        preview,
        toml,
        can_save: errors.is_empty() && changed && s.config_writable(),
        valid: errors.is_empty() && new.is_some(),
        errors: RuleErrorsView { errors, impact }.render().unwrap_or_default(),
    }
}

/// The record the preview shows by default: the first answer that carries tags.
fn default_record(records: &[Record]) -> usize {
    records
        .iter()
        .position(|r| r.response.body.contains("<tagSet>") || r.response.body.contains("\"Tags\""))
        .unwrap_or(0)
}

fn record_label(i: usize, r: &Record) -> String {
    format!(
        "{} · {}:{} · {} · {} · page {}",
        i + 1,
        r.service,
        r.operation,
        r.account,
        r.region,
        r.page
    )
}

fn preview(s: &Shared, config: Option<&Config>, record: Option<usize>) -> PreviewView {
    let empty = |note: &str| PreviewView {
        label: String::new(),
        note: note.to_string(),
        rows: vec![],
        stats: String::new(),
    };
    let sample = lock(&s.sample);
    let Sample::Ready { meta, records, .. } = &*sample else {
        return empty("Collect a sample on the Analyze page to preview the rules on a real answer.");
    };
    if records.is_empty() {
        return empty("The sample holds no records.");
    }
    let i = record.unwrap_or_else(|| default_record(records)).min(records.len() - 1);
    let raw = &records[i];
    let label = record_label(i, raw);
    let Some(config) = config else {
        return PreviewView {
            label,
            ..empty("Fix the rules above to see the preview.")
        };
    };
    let pseudonyms = match Pseudonymizer::load(&config.key_file, false) {
        Ok(p) => p,
        Err(e) => {
            return PreviewView {
                label,
                ..empty(&format!("No preview without the pseudonym key: {e:#}"))
            };
        }
    };
    let (out, stats) = rules::redact(config, &pseudonyms, vec![raw.clone()], &meta.outcome);
    let Some(red) = out.first() else {
        return PreviewView {
            label,
            ..empty("A planning call (the client's own, such as sts:GetCallerIdentity): never packed.")
        };
    };
    let rows = diff::side_by_side(&diff::pretty(&raw.response.body), &diff::pretty(&red.response.body));
    let mut by_action: BTreeMap<&str, usize> = BTreeMap::new();
    for (action, n) in stats.tags.values() {
        *by_action.entry(action.as_str()).or_insert(0) += n;
    }
    let tags: Vec<String> = by_action.iter().map(|(a, n)| format!("{a} {n}")).collect();
    let fields: usize = stats.fields.values().sum();
    PreviewView {
        label,
        note: String::new(),
        rows,
        stats: format!(
            "This record: tags {}; field values changed {fields}.",
            if tags.is_empty() {
                "none".into()
            } else {
                tags.join(", ")
            }
        ),
    }
}

/// Tag keys found in the sample, with how often.
fn sample_keys(s: &Shared, config: &Config) -> Vec<(String, usize)> {
    match &*lock(&s.sample) {
        Sample::Ready { records, .. } => analyze::analyze("", records, Some(config), false)
            .tags
            .into_iter()
            .map(|t| (t.key, t.count))
            .collect(),
        _ => vec![],
    }
}

pub fn rules(s: &Shared, form: Option<&HashMap<String, String>>, q: &HashMap<String, String>) -> Response {
    respond(s, "rules", rules_page(s, form, q, String::new()))
}

fn rules_page(
    s: &Shared,
    form: Option<&HashMap<String, String>>,
    q: &HashMap<String, String>,
    save_error: String,
) -> Result<Response> {
    let (config, bytes) = load(s)?;
    let text = String::from_utf8(bytes.clone()).context("the config is not UTF-8")?;
    let base_sha = form
        .and_then(|f| f.get("base_sha").cloned())
        .unwrap_or_else(|| sha256_hex(&bytes));
    let found = sample_keys(s, &config);
    let mut extra: Vec<String> = found.iter().map(|(k, _)| k.clone()).collect();
    extra.extend(FEATURE_TAGS.iter().map(|(k, _)| k.to_string()));
    let draft = match form {
        Some(f) => Draft::from_form(f),
        None => Draft::of(&config, &extra),
    };
    let record = form
        .and_then(|f| f.get("record"))
        .or(q.get("record"))
        .and_then(|r| r.parse().ok());
    let p = parts(s, &config, &text, &draft, record);
    let counts: HashMap<&str, usize> = found.iter().map(|(k, n)| (k.as_str(), *n)).collect();
    let default_label = format!(
        "default ({})",
        if draft.default == crate::config::TagDefault::Allow {
            "keep"
        } else {
            "drop"
        }
    );
    let tags = draft
        .tags
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let (cost, cost_class) = cost(&draft, t);
            TagRuleView {
                i,
                key: t.key.clone(),
                in_sample: counts.get(t.key.as_str()).map(|n| num(*n)).unwrap_or_default(),
                replace: t.action == "replace",
                actions: choices(TAG_ACTIONS, &t.action, &default_label),
                pattern: t.pattern.clone(),
                with: t.with.clone(),
                cost,
                cost_class,
            }
        })
        .collect();
    let fields: Vec<FieldRuleView> = draft
        .fields
        .iter()
        .enumerate()
        .map(|(j, f)| FieldRuleView {
            j,
            path: f.path.clone(),
            actions: choices(FIELD_ACTIONS, &f.action, ""),
            pattern: f.pattern.clone(),
            with: f.with.clone(),
        })
        .collect();
    let (has_sample, records) = match &*lock(&s.sample) {
        Sample::Ready { records, .. } => {
            let chosen = record
                .unwrap_or_else(|| default_record(records))
                .min(records.len().saturating_sub(1));
            (
                true,
                records
                    .iter()
                    .enumerate()
                    .take(500)
                    .map(|(i, r)| RecordOption {
                        i,
                        label: record_label(i, r),
                        selected: i == chosen,
                    })
                    .collect(),
            )
        }
        _ => (false, vec![]),
    };
    Ok(html(RulesView {
        head: head(s, "Rules", "rules", Some(&config)),
        base_sha,
        allow: draft.default == crate::config::TagDefault::Allow,
        blank_field: fields.len(),
        blank_actions: choices(FIELD_ACTIONS, "pseudonymize", ""),
        new_actions: choices(&TAG_ACTIONS[1..], "keep", ""),
        tags,
        fields,
        required: REQUIRED_FIELDS.join(", "),
        conflicts: rules::conflicts(&config.tags),
        saved: q.contains_key("saved"),
        save_error,
        can_save: p.can_save,
        valid: p.valid,
        writable: s.config_writable(),
        file_name: config_file_name(s),
        has_sample,
        records,
        preview: p.preview,
        toml: p.toml,
        errors: p.errors,
    }))
}

/// What a tag row's action costs in qloudpulse, when qloudpulse reads that key.
fn cost(draft: &Draft, t: &rules::TagRow) -> (String, &'static str) {
    let effective = match t.action.as_str() {
        "default" if draft.default == crate::config::TagDefault::Allow => "keep",
        "default" => "drop",
        a => a,
    };
    match FEATURE_TAGS.iter().find(|(k, _)| *k == t.key) {
        Some((_, f)) => match effective {
            "drop" => (format!("disables {f}"), "bad"),
            "pseudonymize" => (format!("{f} show pseudonyms"), "warn"),
            "replace" => (format!("{f} see replaced values"), "warn"),
            _ => (format!("feeds {f}"), "ok"),
        },
        None => (String::new(), ""),
    }
}

/// The live preview: the parts of the rules page that follow the form, as JSON for the page's script.
pub fn rules_preview(s: &Shared, form: &HashMap<String, String>) -> Response {
    let result = (|| -> Result<Response> {
        let (config, bytes) = load(s)?;
        let text = String::from_utf8(bytes).context("the config is not UTF-8")?;
        let record = form.get("record").and_then(|r| r.parse().ok());
        let draft = Draft::from_form(form);
        let p = parts(s, &config, &text, &draft, record);
        let costs: Vec<serde_json::Value> = draft
            .tags
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (text, class) = cost(&draft, t);
                serde_json::json!({ "i": i, "text": text, "class": class })
            })
            .collect();
        let body = serde_json::json!({
            "preview": p.preview,
            "toml": p.toml,
            "errors": p.errors,
            "can_save": p.can_save,
            "valid": p.valid,
            "costs": costs,
        });
        Ok((
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response())
    })();
    result.unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response())
}

pub fn rules_save(s: &Shared, form: &HashMap<String, String>) -> Response {
    let result = (|| -> Result<Response> {
        let (config, bytes) = load(s)?;
        let text = String::from_utf8(bytes).context("the config is not UTF-8")?;
        let draft = Draft::from_form(form);
        let new = match draft.apply(&config) {
            Ok(n) => n,
            Err(e) => return rules_page(s, Some(form), &HashMap::new(), format!("Not saved: {e:#}")),
        };
        let out = rules::edited_toml(&text, &config, &new)?;
        let base = form.get("base_sha").map(String::as_str).unwrap_or_default();
        match rules::save(&s.config_path, base, &out) {
            Ok(()) => {
                crate::log::info(
                    "config saved from the UI",
                    &[("path", s.config_path.display().to_string())],
                );
                Ok(Redirect::to("/rules?saved=1").into_response())
            }
            Err(e) => {
                if super::state::refused_write(&e) {
                    s.config_read_only();
                    return rules_page(
                        s,
                        Some(form),
                        &HashMap::new(),
                        format!("Not saved: the file system refused the write ({e:#}). Download the file instead."),
                    );
                }
                rules_page(s, Some(form), &HashMap::new(), format!("{e:#}"))
            }
        }
    })();
    respond(s, "rules", result)
}

fn config_file_name(s: &Shared) -> String {
    s.config_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)))
        .unwrap_or_else(|| "qloudpulse-collect.toml".into())
}

/// The whole file with the draft's rules, as a download: for a config this UI cannot write, such as a Kubernetes
/// ConfigMap, and to keep a copy. Nothing on disk changes.
pub fn rules_download(s: &Shared, form: &HashMap<String, String>) -> Response {
    let result = (|| -> Result<Response> {
        let (config, bytes) = load(s)?;
        let text = String::from_utf8(bytes).context("the config is not UTF-8")?;
        let new = match Draft::from_form(form).apply(&config) {
            Ok(n) => n,
            Err(e) => return rules_page(s, Some(form), &HashMap::new(), format!("Not downloaded: {e:#}")),
        };
        let out = rules::edited_toml(&text, &config, &new)?;
        let disposition = format!("attachment; filename=\"{}\"", config_file_name(s));
        Ok((
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/toml; charset=utf-8".to_string(),
                ),
                (axum::http::header::CONTENT_DISPOSITION, disposition),
            ],
            out,
        )
            .into_response())
    })();
    respond(s, "rules", result)
}

// ---------------------------------------------------------------- Bundles

pub struct TargetView {
    pub name: &'static str,
    pub detail: String,
    /// For qloudpulse: how an upload would sign in. Never the token.
    pub state: String,
    pub state_class: &'static str,
}

/// How an upload to `endpoint` would sign in, found the way the upload finds its token: `QLOUDPULSE_TOKEN`, else
/// the token `login` kept. Says where the token comes from and for which company, never the token.
#[cfg(feature = "upload")]
fn sign_in_state(endpoint: &str) -> (String, &'static str) {
    use qloudpulse_stack::{credentials, login};
    if std::env::var(crate::deliver::qloudpulse::TOKEN_ENV).is_ok_and(|t| !t.trim().is_empty()) {
        return ("signed in via QLOUDPULSE_TOKEN".into(), "ok");
    }
    let origin = match login::origin(endpoint) {
        Ok(o) => o,
        Err(e) => return (format!("the endpoint is not usable: {e:#}"), "bad"),
    };
    match credentials::system().load(&origin, credentials::now()) {
        Some(saved) => {
            let company = login::plain(&saved.company);
            let until = saved
                .expires_at
                .as_deref()
                .and_then(credentials::day)
                .map(|d| format!(", until {d}"))
                .unwrap_or_default();
            let to = if company.is_empty() {
                String::new()
            } else {
                format!(" to {company}")
            };
            (format!("signed in{to} via `qloudpulse login`{until}"), "ok")
        }
        None => (
            "not signed in: run `qloudpulse-collect login` (or `qloudpulse login`), or set QLOUDPULSE_TOKEN. Until \
             then the qloudpulse delivery fails and is retried at each cycle."
                .into(),
            "bad",
        ),
    }
}

#[cfg(not(feature = "upload"))]
fn sign_in_state(_endpoint: &str) -> (String, &'static str) {
    (
        "this build has no qloudpulse upload: the delivery fails and is retried".into(),
        "bad",
    )
}

pub struct BundleRow {
    pub name: String,
    pub window: String,
    pub size: String,
    pub records: String,
    pub accounts: String,
    pub deliveries: Vec<Delivery>,
    pub encrypted: bool,
    pub note: String,
    pub note_class: &'static str,
}

#[derive(Template)]
#[template(path = "bundles.html")]
struct BundlesView {
    head: Head,
    output_dir: String,
    keep: usize,
    targets: Vec<TargetView>,
    rows: Vec<BundleRow>,
}

fn targets(config: &Config) -> Vec<TargetView> {
    let d = &config.deliver;
    let mut out = Vec::new();
    if let Some(s3) = &d.s3 {
        out.push(TargetView {
            state: String::new(),
            state_class: "",
            name: "s3",
            detail: format!(
                "s3://{}/{} ({}{})",
                s3.bucket,
                s3.prefix,
                s3.region,
                s3.kms_key_id
                    .as_ref()
                    .map(|k| format!(", SSE-KMS {k}"))
                    .unwrap_or_default()
            ),
        });
    }
    if let Some(q) = &d.qloudpulse {
        let (state, state_class) = sign_in_state(&q.endpoint);
        out.push(TargetView {
            name: "qloudpulse",
            detail: q.endpoint.clone(),
            state,
            state_class,
        });
    }
    if let Some(c) = &d.command {
        out.push(TargetView {
            state: String::new(),
            state_class: "",
            name: "command",
            detail: c.argv.join(" "),
        });
    }
    out
}

fn size(path: &Path) -> String {
    let b = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if b >= 1 << 20 {
        format!("{:.1} MB", b as f64 / (1 << 20) as f64)
    } else {
        format!("{} KB", num((b as usize).div_ceil(1024)))
    }
}

pub fn bundles(s: &Shared) -> Response {
    respond(s, "bundles", bundles_page(s))
}

fn bundles_page(s: &Shared) -> Result<Response> {
    let (config, _) = load(s)?;
    let state = delivered(&config.output_dir);
    let rows = bundle_names(&config)?
        .into_iter()
        .map(|name| {
            let path = config.output_dir.join(&name);
            let manifest = read_manifest(&path);
            let (window_, records, accounts, note, note_class) = match &manifest {
                Ok(m) => (
                    window(&m.window),
                    num(m.files.iter().map(|f| f.records).sum()),
                    num(m.accounts.len()),
                    if m.failures.is_empty() {
                        String::new()
                    } else {
                        format!("{} collector failure(s)", m.failures.len())
                    },
                    "warn",
                ),
                Err(e) => (
                    String::new(),
                    String::new(),
                    String::new(),
                    format!("unreadable: {e:#}"),
                    "bad",
                ),
            };
            BundleRow {
                deliveries: deliveries(&config, &state, &name),
                encrypted: crate::encrypt::jwe_path(&path).exists(),
                size: size(&path),
                name,
                window: window_,
                records,
                accounts,
                note,
                note_class,
            }
        })
        .collect();
    Ok(html(BundlesView {
        head: head(s, "Bundles", "bundles", Some(&config)),
        output_dir: config.output_dir.display().to_string(),
        keep: config.run.keep_bundles,
        targets: targets(&config),
        rows,
    }))
}

pub struct FileRow {
    pub path: String,
    pub sha256: String,
    pub bytes: String,
    pub records: String,
}

pub struct KeyCount {
    pub key: String,
    pub action: String,
    pub action_class: &'static str,
    pub count: String,
}

pub struct RecordRow {
    pub n: usize,
    pub account: String,
    pub region: String,
    pub operation: String,
    pub page: u32,
    pub status: String,
    pub error: bool,
    pub href: String,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "bundle.html")]
struct BundleView {
    head: Head,
    name: String,
    error: String,
    facts: Vec<(&'static str, String)>,
    config_same: bool,
    key_same: bool,
    failures: Vec<String>,
    files: Vec<FileRow>,
    rules_: Vec<(&'static str, String)>,
    redaction: Vec<KeyCount>,
    fields: Vec<KeyCount>,
    ids: String,
    impact: Vec<String>,
    scan: Vec<ScanRow>,
    deliveries: Vec<Delivery>,
    operations: Vec<(String, bool)>,
    op: String,
    records: Vec<RecordRow>,
    total: String,
    page_line: String,
    prev: String,
    next: String,
    record: String,
    body: String,
}

pub fn bundle(s: &Shared, name: &str, q: &HashMap<String, String>) -> Response {
    respond(s, "bundles", bundle_page(s, name, q))
}

fn link(name: &str, op: &str, page: usize, rec: Option<usize>) -> String {
    let mut qs = url::form_urlencoded::Serializer::new(String::new());
    if !op.is_empty() {
        qs.append_pair("op", op);
    }
    if page > 0 {
        qs.append_pair("page", &page.to_string());
    }
    if let Some(r) = rec {
        qs.append_pair("rec", &r.to_string());
    }
    let qs = qs.finish();
    if qs.is_empty() {
        format!("/bundles/{name}")
    } else {
        format!("/bundles/{name}?{qs}")
    }
}

fn bundle_page(s: &Shared, name: &str, q: &HashMap<String, String>) -> Result<Response> {
    let (config, bytes) = load(s)?;
    let path = bundle_path(&config, name)?;
    let state = delivered(&config.output_dir);
    let mut view = BundleView {
        head: head(s, "Bundle", "bundles", Some(&config)),
        name: name.to_string(),
        error: String::new(),
        facts: vec![],
        config_same: false,
        key_same: false,
        failures: vec![],
        files: vec![],
        rules_: vec![],
        redaction: vec![],
        fields: vec![],
        ids: String::new(),
        impact: vec![],
        scan: vec![],
        deliveries: deliveries(&config, &state, name),
        operations: vec![],
        op: String::new(),
        records: vec![],
        total: String::new(),
        page_line: String::new(),
        prev: String::new(),
        next: String::new(),
        record: String::new(),
        body: String::new(),
    };
    let opened = match bundle::open(&path) {
        Ok(o) => o,
        Err(e) => {
            view.error = format!("{e:#}");
            return Ok(html(view));
        }
    };
    let m = &opened.manifest;
    view.config_same = m.config_sha256 == sha256_hex(&bytes);
    view.key_same = Pseudonymizer::load(&config.key_file, false).is_ok_and(|p| p.fingerprint() == m.key_fingerprint);
    view.facts = vec![
        ("window", window(&m.window)),
        ("collected_at", when(m.collected_at)),
        ("source_id", m.source_id.clone()),
        ("schema_version", m.schema_version.clone()),
        ("tool", format!("{} {}", m.tool, m.tool_version)),
        (
            "call_plan_version",
            format!(
                "{}{}",
                m.call_plan_version,
                if m.call_plan_seed { " (hand-made seed plan)" } else { "" }
            ),
        ),
        ("accounts", m.accounts.join(", ")),
        ("regions", m.regions.join(", ")),
        ("config_sha256", m.config_sha256.clone()),
        ("key_fingerprint", m.key_fingerprint.clone()),
    ];
    if let Some(cur) = &m.cur {
        view.facts
            .push(("bill", serde_json::to_string(cur).unwrap_or_default()));
    }
    view.failures = m.failures.clone();
    view.files = m
        .files
        .iter()
        .map(|f| FileRow {
            path: f.path.clone(),
            sha256: f.sha256.clone(),
            bytes: num(f.bytes as usize),
            records: num(f.records),
        })
        .collect();
    let r = &m.rules;
    let list = |v: &[String]| if v.is_empty() { "—".to_string() } else { v.join(", ") };
    view.rules_ = vec![
        ("tag default", r.tag_default.clone()),
        ("kept", list(&r.tags_kept)),
        ("dropped", list(&r.tags_dropped)),
        ("pseudonymized", list(&r.tags_pseudonymized)),
        ("replaced", list(&r.tags_replaced)),
        ("fields", list(&r.fields)),
    ];
    let mut tags: Vec<(&String, &(String, usize))> = m.redaction.tags.iter().collect();
    tags.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
    view.redaction = tags
        .into_iter()
        .map(|(k, (a, n))| KeyCount {
            key: k.clone(),
            action_class: action_class(a),
            action: a.clone(),
            count: num(*n),
        })
        .collect();
    view.fields = m
        .redaction
        .fields
        .iter()
        .map(|(p, n)| KeyCount {
            key: p.clone(),
            action: String::new(),
            action_class: "",
            count: num(*n),
        })
        .collect();
    view.ids = format!(
        "{} account IDs and {} resource IDs pseudonymized",
        m.identifiers.account_ids, m.identifiers.resource_ids
    );
    view.impact = m.feature_impact.clone();
    view.scan = scan_rows(&m.scan);

    let op = q.get("op").cloned().unwrap_or_default();
    let ops: BTreeSet<String> = opened
        .records
        .iter()
        .map(|r| format!("{}:{}", r.service, r.operation))
        .collect();
    view.operations = ops.into_iter().map(|o| (o.clone(), o == op)).collect();
    let shown: Vec<(usize, &Record)> = opened
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| op.is_empty() || format!("{}:{}", r.service, r.operation) == op)
        .collect();
    let pages = shown.len().div_ceil(RECORDS_PER_PAGE).max(1);
    let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(0).min(pages - 1);
    let rec: Option<usize> = q.get("rec").and_then(|r| r.parse().ok());
    let chosen = rec.or_else(|| shown.get(page * RECORDS_PER_PAGE).map(|(i, _)| *i));
    view.records = shown
        .iter()
        .skip(page * RECORDS_PER_PAGE)
        .take(RECORDS_PER_PAGE)
        .map(|(i, r)| RecordRow {
            n: i + 1,
            account: r.account.clone(),
            region: r.region.clone(),
            operation: format!("{}:{}", r.service, r.operation),
            page: r.page,
            status: format!(
                "{}{}",
                r.response.status,
                r.error_code.as_ref().map(|c| format!(" {c}")).unwrap_or_default()
            ),
            error: r.response.status >= 400,
            href: link(name, &op, page, Some(*i)),
            selected: Some(*i) == chosen,
        })
        .collect();
    view.op = op.clone();
    view.total = num(opened.records.len());
    view.page_line = format!(
        "{} records{}, page {} of {pages}",
        num(shown.len()),
        if op.is_empty() {
            String::new()
        } else {
            format!(" of {op}")
        },
        page + 1
    );
    if page > 0 {
        view.prev = link(name, &op, page - 1, None);
    }
    if page + 1 < pages {
        view.next = link(name, &op, page + 1, None);
    }
    if let Some(r) = chosen.and_then(|i| opened.records.get(i)) {
        let mut head = r.clone();
        head.response.body = format!("‹{} bytes, below›", num(r.response.body.len()));
        view.record = serde_json::to_string_pretty(&head)?;
        view.body = diff::pretty(&r.response.body);
    }
    Ok(html(view))
}

// ---------------------------------------------------------------- Run history

pub struct CycleRow {
    pub started: String,
    pub took: String,
    pub result: &'static str,
    pub class: &'static str,
    pub bundle: String,
    pub error: String,
    pub pending: Vec<String>,
}

pub struct HistoryRow {
    pub name: String,
    pub window: String,
    pub collected: String,
    pub records: String,
    pub accounts: String,
    pub deliveries: Vec<Delivery>,
    pub failures: Vec<String>,
    pub impact: Vec<String>,
    pub class: &'static str,
    pub result: &'static str,
}

#[derive(Template)]
#[template(path = "runs.html")]
struct RunsView {
    head: Head,
    under_run: bool,
    every: u32,
    since: String,
    next: String,
    cycles: Vec<CycleRow>,
    history: Vec<HistoryRow>,
    minute: u32,
}

pub fn runs(s: &Shared) -> Response {
    respond(s, "runs", runs_page(s))
}

fn runs_page(s: &Shared) -> Result<Response> {
    let (config, _) = load(s)?;
    let (under_run, every, since, next, cycles) = match &s.run {
        Some(run) => {
            let r = lock(run);
            let cycles = r
                .cycles
                .iter()
                .map(|c| {
                    let (result, class) = match (&c.finished, &c.error) {
                        (None, _) => ("running", "muted"),
                        (Some(_), Some(_)) => ("failed", "bad"),
                        (Some(_), None) if !c.pending.is_empty() => ("delivery pending", "warn"),
                        _ => ("ok", "ok"),
                    };
                    CycleRow {
                        started: when(c.started),
                        took: c
                            .finished
                            .map(|f| format!("{}s", f.duration_since(c.started).as_secs()))
                            .unwrap_or_default(),
                        result,
                        class,
                        bundle: c.bundle.clone().unwrap_or_default(),
                        error: c.error.clone().unwrap_or_default(),
                        pending: c.pending.clone(),
                    }
                })
                .collect();
            (
                true,
                r.every_hours,
                when(r.since),
                r.next.map(when).unwrap_or_else(|| "now: a cycle is running".into()),
                cycles,
            )
        }
        None => (false, config.run.every_hours, String::new(), String::new(), vec![]),
    };
    let state = delivered(&config.output_dir);
    let history = bundle_names(&config)?
        .into_iter()
        .take(168)
        .map(|name| {
            let path = config.output_dir.join(&name);
            let deliveries = deliveries(&config, &state, &name);
            match read_manifest(&path) {
                Ok(m) => {
                    let pending = deliveries.iter().any(|d| !d.done);
                    let (result, class) = if !m.failures.is_empty() {
                        ("warnings", "warn")
                    } else if pending {
                        ("delivery pending", "warn")
                    } else {
                        ("ok", "ok")
                    };
                    HistoryRow {
                        window: window(&m.window),
                        collected: when(m.collected_at),
                        records: num(m.files.iter().map(|f| f.records).sum()),
                        accounts: num(m.accounts.len()),
                        failures: m.failures.clone(),
                        impact: m.feature_impact.clone(),
                        deliveries,
                        name,
                        result,
                        class,
                    }
                }
                Err(e) => HistoryRow {
                    window: String::new(),
                    collected: String::new(),
                    records: String::new(),
                    accounts: String::new(),
                    failures: vec![format!("unreadable: {e:#}")],
                    impact: vec![],
                    deliveries,
                    name,
                    result: "unreadable",
                    class: "bad",
                },
            }
        })
        .collect();
    Ok(html(RunsView {
        head: head(s, "Run history", "runs", Some(&config)),
        under_run,
        every,
        since,
        next,
        cycles,
        history,
        minute: config.run.minute,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_names() {
        assert_eq!(num(0), "0");
        assert_eq!(num(1284), "1,284");
        assert_eq!(num(1234567), "1,234,567");
        let mut c: Config = toml::from_str("source_id = \"a\"").unwrap();
        c.output_dir = std::env::temp_dir();
        assert!(bundle_path(&c, "../etc/passwd.tgz").is_err());
        assert!(bundle_path(&c, ".delivered.json").is_err());
        assert!(bundle_path(&c, "x.txt").is_err());
    }
}

//! The bundle: a `.tgz` of redacted records as NDJSON, one file per collector, and `manifest.json`.
//!
//! Plain text throughout, so `tar -xzf` and `jq` show exactly what leaves. Written deterministically (sorted
//! entries, fixed times and modes): the same snapshot and config give the same bytes.

use crate::callplan::CallPlan;
use crate::collect::Outcome;
use crate::config::{Config, TagDefault};
use crate::identifiers::{IdStats, Identifiers};
use crate::pseudonym::Pseudonymizer;
use crate::record::{Record, dedupe};
use crate::redact::{Redactor, Stats};
use crate::snapshot::{Snapshot, read_ndjson};
use crate::window::Window;
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: &str = "1.0.0";
const MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: String,
    pub tool: String,
    pub tool_version: String,
    pub call_plan_version: String,
    /// True while the call plan is hand-made, not generated from qloudpulse's collectors.
    pub call_plan_seed: bool,
    pub source_id: String,
    pub window: Window,
    pub collected_at: jiff::Timestamp,
    pub accounts: Vec<String>,
    pub regions: Vec<String>,
    /// Collectors that stopped early, by account and region.
    pub failures: Vec<String>,
    /// The bill queries this bundle answers: qloudpulse replays them with these periods and scope tag keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cur: Option<crate::collect::cur::CurOutcome>,
    /// The forecast this bundle answers, read on `day` with `prediction_interval`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<crate::collect::forecast::ForecastOutcome>,
    pub files: Vec<FileEntry>,
    /// SHA-256 of the config file as it was when packing.
    pub config_sha256: String,
    pub rules: RulesSummary,
    pub redaction: Stats,
    /// How many distinct account and resource IDs were pseudonymized (0 when kept).
    #[serde(default)]
    pub identifiers: IdStats,
    /// Names the pseudonym key without revealing it. When it changes, every pseudonym changed.
    pub key_fingerprint: String,
    /// What the rules cost in qloudpulse's reports.
    pub feature_impact: Vec<String>,
    /// What the last look before packing found in what leaves: kinds, counts, masked samples.
    #[serde(default)]
    pub scan: crate::scan::ScanReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub records: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesSummary {
    pub tag_default: String,
    pub tags_kept: Vec<String>,
    pub tags_dropped: Vec<String>,
    pub tags_pseudonymized: Vec<String>,
    pub tags_replaced: Vec<String>,
    /// `path action`
    pub fields: Vec<String>,
}

impl RulesSummary {
    fn of(config: &Config) -> RulesSummary {
        let t = &config.tags;
        RulesSummary {
            tag_default: match t.default {
                TagDefault::Deny => "deny".into(),
                TagDefault::Allow => "allow".into(),
            },
            tags_kept: t.keep.clone(),
            tags_dropped: t.drop.clone(),
            tags_pseudonymized: t.pseudonymize.clone(),
            tags_replaced: t.replace.iter().map(|r| r.key.clone()).collect(),
            fields: config
                .fields
                .iter()
                .map(|f| format!("{} {:?}", f.path, f.action).to_lowercase())
                .collect(),
        }
    }
}

pub fn file_name(source_id: &str, window: &Window) -> String {
    format!("qloudpulse-{source_id}-{}.tgz", window.label())
}

/// Redacts the snapshot by `config` and writes the bundle into `config.output_dir`.
/// Stops before writing when the scan finds a blocking kind, unless `allow_findings`.
pub fn pack(
    config: &Config,
    config_bytes: &[u8],
    pseudonyms: &Pseudonymizer,
    snapshot: &Snapshot,
    allow_findings: bool,
) -> Result<PathBuf> {
    let mut records: Vec<Record> = dedupe(snapshot.records.clone())
        .into_iter()
        .filter(|r| r.collector != "client")
        .collect();
    let stats = Redactor::new(config, pseudonyms).records(&mut records);
    let mut outcome = snapshot.meta.outcome.clone();
    let id_stats = Identifiers {
        accounts: config.identifiers.account_ids,
        resources: config.identifiers.resource_ids,
        pseudonyms,
    }
    .apply(&mut records, &mut outcome);
    let scan = crate::scan::scan(&records, &config.scan);
    let blocking = scan.blocking();
    if !blocking.is_empty() && !allow_findings {
        let kinds: Vec<String> = blocking.iter().map(|(k, s)| format!("{k} ({})", s.count)).collect();
        bail!(
            "not packed: the bundle would carry {}.\n{}Redact them with a [tags] or [[fields]] rule, allow them in [scan], \
             or pack with --allow-findings. `analyze` shows where they are.",
            kinds.join(", "),
            scan.text()
        );
    }

    let mut by_collector: BTreeMap<String, Vec<&Record>> = BTreeMap::new();
    for r in &records {
        by_collector.entry(r.collector.clone()).or_default().push(r);
    }
    let mut files = Vec::new();
    let mut contents = Vec::new();
    for (collector, rs) in &by_collector {
        let mut text = String::new();
        for r in rs {
            text.push_str(&serde_json::to_string(r)?);
            text.push('\n');
        }
        let path = format!("calls/{collector}.ndjson");
        files.push(FileEntry {
            path: path.clone(),
            sha256: hex::encode(Sha256::digest(text.as_bytes())),
            bytes: text.len() as u64,
            records: rs.len(),
        });
        contents.push((path, text.into_bytes()));
    }

    let plan = CallPlan::embedded();
    let Outcome {
        accounts,
        regions,
        failures,
        cur,
        forecast,
    } = outcome;
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION.into(),
        tool: "qloudpulse-collect".into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        call_plan_version: plan.version.clone(),
        call_plan_seed: plan.seed,
        source_id: config.source_id.clone(),
        window: snapshot.meta.window,
        collected_at: snapshot.meta.collected_at,
        accounts,
        regions,
        failures,
        cur,
        forecast,
        files,
        config_sha256: hex::encode(Sha256::digest(config_bytes)),
        rules: RulesSummary::of(config),
        redaction: stats,
        identifiers: id_stats,
        key_fingerprint: pseudonyms.fingerprint(),
        feature_impact: config.feature_impact(),
        scan,
    };
    contents.insert(0, (MANIFEST.to_string(), serde_json::to_vec_pretty(&manifest)?));

    std::fs::create_dir_all(&config.output_dir)?;
    let path = config
        .output_dir
        .join(file_name(&config.source_id, &snapshot.meta.window));
    let partial = path.with_extension("tgz.partial");
    write_tgz(&partial, &contents, snapshot.meta.window.end.as_second() as u64)?;
    std::fs::rename(&partial, &path)?;
    if let Some(e) = &config.encrypt {
        let recipient = crate::encrypt::Recipient::load(&e.public_key_file)?;
        crate::encrypt::encrypt_file(&path, &recipient)?;
    }
    if config.keep_pseudonym_map {
        pseudonyms.save_map(&config.work_dir.join("pseudonyms.tsv"))?;
    }
    Ok(path)
}

fn write_tgz(path: &Path, contents: &[(String, Vec<u8>)], mtime: u64) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.mode(tar::HeaderMode::Deterministic);
    for (name, bytes) in contents {
        let mut header = tar::Header::new_ustar();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(mtime);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes.as_slice())?;
    }
    tar.into_inner()?.finish()?.sync_all()?;
    Ok(())
}

/// A bundle read back, with every file checked against the manifest.
pub struct Opened {
    pub manifest: Manifest,
    pub records: Vec<Record>,
}

pub fn open(path: &Path) -> Result<Opened> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        entries.insert(name, bytes);
    }
    let manifest: Manifest = serde_json::from_slice(entries.get(MANIFEST).context("the bundle has no manifest.json")?)?;
    let mut records = Vec::new();
    for f in &manifest.files {
        let bytes = entries
            .get(&f.path)
            .with_context(|| format!("the bundle lacks {}", f.path))?;
        if hex::encode(Sha256::digest(bytes)) != f.sha256 {
            bail!("{} does not match its SHA-256 in the manifest", f.path);
        }
        records.extend(read_ndjson(bytes.as_slice())?);
    }
    let listed: Vec<&String> = manifest.files.iter().map(|f| &f.path).collect();
    if let Some(extra) = entries.keys().find(|k| *k != MANIFEST && !listed.contains(k)) {
        bail!("the bundle holds {extra}, which the manifest does not list");
    }
    Ok(Opened { manifest, records })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::protocol::WireRequest;
    use crate::record::RecordedResponse;
    use crate::snapshot::SnapshotMeta;

    fn record(collector: &str, body: &str) -> Record {
        Record {
            collector: collector.into(),
            account: "111122223333".into(),
            region: "eu-west-1".into(),
            service: "ec2".into(),
            operation: "DescribeInstances".into(),
            page: 1,
            request: WireRequest {
                method: "POST".into(),
                host: "ec2.eu-west-1.amazonaws.com".into(),
                path: "/".into(),
                query: vec![],
                headers: vec![],
                body: format!("Action=DescribeInstances&n={}", body.len()),
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
    fn encrypts_next_to_the_plaintext_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let secret = p256::SecretKey::from_slice(&[7u8; 32]).unwrap();
        let point = {
            use p256::elliptic_curve::sec1::ToSec1Point;
            secret.public_key().to_sec1_point(false)
        };
        let b64 = |b: &[u8]| {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
        };
        let jwk = serde_json::json!({"kty": "EC", "crv": "P-256", "x": b64(point.x().unwrap()), "y": b64(point.y().unwrap())})
            .to_string();
        std::fs::write(dir.path().join("qp.jwk"), jwk).unwrap();
        let text = format!(
            "source_id = \"main\"\noutput_dir = \"{}\"\n[encrypt]\npublic_key_file = \"{}\"\n",
            dir.path().join("out").display(),
            dir.path().join("qp.jwk").display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        let now: jiff::Timestamp = "2026-11-21T07:05:00Z".parse().unwrap();
        let meta = SnapshotMeta {
            tool_version: "t".into(),
            call_plan_version: "p".into(),
            window: Window::last_full_hour(now),
            collected_at: now,
            outcome: Outcome::default(),
        };
        let snapshot = Snapshot::write(dir.path().join("raw"), meta, vec![record("aws-cost-collect", "<R/>")]).unwrap();
        let path = pack(
            &config,
            text.as_bytes(),
            &Pseudonymizer::new(vec![9; 32], false),
            &snapshot,
            false,
        )
        .unwrap();
        let jwe = std::fs::read_to_string(crate::encrypt::jwe_path(&path)).unwrap();
        assert_eq!(jwe.split('.').count(), 5);
        assert!(open(&path).is_ok(), "the plaintext stays for you to inspect");
    }

    #[test]
    fn refuses_to_pack_an_email_until_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "source_id = \"main\"\noutput_dir = \"{}\"\n[tags]\ndefault = \"allow\"\n",
            dir.path().join("out").display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        let pseudonyms = Pseudonymizer::new(vec![9; 32], false);
        let now: jiff::Timestamp = "2026-11-21T07:05:00Z".parse().unwrap();
        let meta = SnapshotMeta {
            tool_version: "t".into(),
            call_plan_version: "p".into(),
            window: Window::last_full_hour(now),
            collected_at: now,
            outcome: Outcome::default(),
        };
        let tagged = "<R><tagSet><item><key>Owner</key><value>alice@corp.example</value></item></tagSet></R>";
        let snapshot = Snapshot::write(dir.path().join("raw"), meta, vec![record("aws-cost-collect", tagged)]).unwrap();
        let err = pack(&config, text.as_bytes(), &pseudonyms, &snapshot, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("email (1)") && err.contains("al…le") && !err.contains("alice@"),
            "{err}"
        );
        assert!(!dir.path().join("out").exists(), "nothing written");
        let path = pack(&config, text.as_bytes(), &pseudonyms, &snapshot, true).unwrap();
        assert_eq!(
            open(&path).unwrap().manifest.scan.kinds["email"].count,
            1,
            "the manifest says what was let through"
        );
    }

    #[test]
    fn packs_redacted_deterministic_and_verifiable() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "source_id = \"main\"\noutput_dir = \"{}\"\nwork_dir = \"{}\"\n",
            dir.path().join("out").display(),
            dir.path().join("work").display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        let pseudonyms = Pseudonymizer::new(vec![9; 32], false);
        let now: jiff::Timestamp = "2026-11-21T07:05:00Z".parse().unwrap();
        let meta = SnapshotMeta {
            tool_version: "t".into(),
            call_plan_version: "p".into(),
            window: Window::last_full_hour(now),
            collected_at: now,
            outcome: Outcome {
                accounts: vec!["111122223333".into()],
                regions: vec!["eu-west-1".into()],
                failures: vec![],
                cur: None,
                forecast: None,
            },
        };
        let tagged = "<R><tagSet><item><key>Owner</key><value>alice</value></item></tagSet></R>";
        let records = vec![
            record("aws-cost-collect", tagged),
            record("aws-cost-collect", tagged),
            record("client", "<x/>"),
        ];
        let snapshot = Snapshot::write(dir.path().join("raw"), meta, records).unwrap();

        let first = pack(&config, text.as_bytes(), &pseudonyms, &snapshot, false).unwrap();
        let first_bytes = std::fs::read(&first).unwrap();
        let again = pack(&config, text.as_bytes(), &pseudonyms, &snapshot, false).unwrap();
        assert_eq!(first_bytes, std::fs::read(&again).unwrap(), "same input, same bytes");
        assert!(first.ends_with("qloudpulse-main-20261121T0600Z.tgz"));

        let opened = open(&first).unwrap();
        assert_eq!(
            opened.records.len(),
            1,
            "the repeat and the client's own call are left out"
        );
        assert!(!opened.records[0].response.body.contains("alice"));
        assert_eq!(opened.manifest.redaction.tags["Owner"], ("drop".to_string(), 1));
        assert_eq!(opened.manifest.key_fingerprint, pseudonyms.fingerprint());
    }
}

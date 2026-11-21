//! The TOML config. It holds no secrets: AWS credentials come from the standard chain, the HMAC key and the upload
//! token from files it names.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Names this client among several a company may run. An AWS account belongs to one source only.
    pub source_id: String,
    /// An AWS profile; the standard chain's default when absent.
    #[serde(default)]
    pub profile: Option<String>,
    /// Where STS and the planning calls go.
    #[serde(default = "default_region")]
    pub home_region: String,
    /// Regions to read. Absent: every region enabled in the account.
    #[serde(default)]
    pub regions: Option<Vec<String>>,
    /// Accounts to read, each through a role. Empty: the credentials' own account.
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default = "default_key_file")]
    pub key_file: PathBuf,
    /// Raw snapshots and the client's own state. Never delivered.
    #[serde(default = "default_work_dir")]
    pub work_dir: PathBuf,
    #[serde(default = "default_output_dir")]
    pub output_dir: PathBuf,
    #[serde(default)]
    pub tags: TagRules,
    #[serde(default)]
    pub fields: Vec<FieldRule>,
    /// Account IDs and resource IDs, kept or pseudonymized with their format.
    #[serde(default)]
    pub identifiers: IdentifierConfig,
    /// Derive `qloudpulse:environment` from raw tags before redaction, so names can be redacted.
    #[serde(default)]
    pub environment: Option<crate::derive::EnvironmentConfig>,
    /// The last look before packing: what to allow among the secrets and personal data it finds.
    #[serde(default)]
    pub scan: crate::scan::ScanConfig,
    /// The bill: CUR 2.0 through Athena, read once a day. Absent: no bill is read.
    #[serde(default)]
    pub cur: Option<CurConfig>,
    #[serde(default)]
    pub run: RunConfig,
    #[serde(default)]
    pub deliver: DeliverConfig,
    /// Writes `pseudonyms.tsv` in `work_dir`: each pseudonym next to its real value, so you can read qloudpulse's
    /// reports. It holds the real values, so it stays with you.
    #[serde(default)]
    pub keep_pseudonym_map: bool,
    /// Read AWS's carbon estimate of each account once a day. Needs `sustainability:GetEstimatedCarbonEmissions`.
    #[serde(default = "yes")]
    pub carbon: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub id: String,
    pub role_arn: String,
    #[serde(default)]
    pub external_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TagDefault {
    Deny,
    Allow,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TagRules {
    /// What happens to a tag no rule names.
    #[serde(default = "default_tag_default")]
    pub default: TagDefault,
    #[serde(default)]
    pub keep: Vec<String>,
    #[serde(default)]
    pub drop: Vec<String>,
    #[serde(default)]
    pub pseudonymize: Vec<String>,
    #[serde(default)]
    pub replace: Vec<TagReplace>,
}

impl Default for TagRules {
    fn default() -> Self {
        TagRules {
            default: TagDefault::Deny,
            keep: FEATURE_TAGS.iter().map(|(k, _)| k.to_string()).collect(),
            drop: vec![],
            pseudonymize: vec![],
            replace: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TagReplace {
    pub key: String,
    /// A regex the value must match; the whole value when absent.
    #[serde(default)]
    pub pattern: Option<String>,
    /// The replacement; `$1` refers to the pattern's groups.
    pub with: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldAction {
    Drop,
    Replace,
    Pseudonymize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldRule {
    /// Field names separated by dots, matched against the end of a field's path in the answer. List wrappers
    /// (`item`, `member`) are left out: `instancesSet.privateDnsName`, or just `privateDnsName`.
    pub path: String,
    pub action: FieldAction,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub with: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CurConfig {
    /// The payer account, where the CUR 2.0 export and its Athena table are.
    pub account: String,
    /// A role to read the bill through; the payer's credentials from `accounts` when absent.
    #[serde(default)]
    pub role_arn: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default = "default_workgroup")]
    pub workgroup: String,
    #[serde(default = "default_database")]
    pub database: String,
    #[serde(default = "default_table")]
    pub table: String,
    /// Your scope tag keys as set in qloudpulse. They are part of the query, so they must match what qloudpulse has.
    #[serde(default)]
    pub scope_tag_keys: Vec<String>,
    /// Months before the current one to read, 1 to 3. 1: the previous and the current month.
    #[serde(default = "one")]
    pub periods_back: u32,
    /// Also read Cost Explorer's month-to-date spend and forecast per account. AWS bills $0.01 a request.
    #[serde(default)]
    pub forecast: bool,
    /// The forecast's prediction interval, as set in qloudpulse (51-99): it is part of the request.
    #[serde(default = "eighty")]
    pub prediction_interval: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IdAction {
    #[default]
    Keep,
    Pseudonymize,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdentifierConfig {
    /// 12-digit account IDs, wherever they appear (ARNs included): another 12-digit number when pseudonymized.
    #[serde(default)]
    pub account_ids: IdAction,
    /// EC2-style resource IDs (`i-`, `vol-`, `eni-`, …): the same prefix and length when pseudonymized.
    #[serde(default)]
    pub resource_ids: IdAction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    #[serde(default = "one")]
    pub every_hours: u32,
    /// Minutes past the hour to start, so the hour just ended is complete.
    #[serde(default = "five")]
    pub minute: u32,
    /// The UTC hour whose run also reads the daily parts: the bill and the 90-day spot price history.
    #[serde(default)]
    pub daily_hour: u32,
    /// Keep raw snapshots after packing. Off: they are deleted once the bundle is written.
    #[serde(default)]
    pub keep_raw: bool,
    /// Bundles kept in `output_dir`; an undelivered one is never deleted.
    #[serde(default = "week_of_hours")]
    pub keep_bundles: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            every_hours: 1,
            minute: 5,
            daily_hour: 0,
            keep_raw: false,
            keep_bundles: 168,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverConfig {
    #[serde(default)]
    pub s3: Option<S3Target>,
    #[serde(default)]
    pub qloudpulse: Option<QloudpulseTarget>,
    #[serde(default)]
    pub command: Option<CommandTarget>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct S3Target {
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    pub region: String,
    /// SSE-KMS with this key; S3's default encryption when absent.
    #[serde(default)]
    pub kms_key_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QloudpulseTarget {
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// A file holding the bundle-upload token. `QLOUDPULSE_UPLOAD_TOKEN` wins when set.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTarget {
    /// The program and its arguments; `{bundle}` is replaced by the bundle's path.
    pub argv: Vec<String>,
}

/// Tag keys qloudpulse features read, and what dropping or pseudonymizing them costs.
pub const FEATURE_TAGS: &[(&str, &str)] = &[
    ("Name", "resource names in reports"),
    ("Environment", "the Environment column and environment rules"),
    ("Team", "cost by team and tag rules"),
    ("Company", "the Company column and tag rules"),
    ("eks:cluster-name", "matching EKS nodes to their instances"),
    ("aws:eks:cluster-name", "matching EKS nodes to their instances"),
    ("karpenter.sh/nodepool", "Kubernetes node pools"),
    ("eks:nodegroup-name", "Kubernetes node groups"),
    ("aws:autoscaling:groupName", "Auto Scaling group cost"),
];

/// Fields qloudpulse prices from. A rule may not drop, replace or pseudonymize them.
pub const REQUIRED_FIELDS: &[&str] = &[
    "instanceType",
    "InstanceType",
    "platformDetails",
    "instanceLifecycle",
    "volumeType",
    "size",
    "iops",
    "throughput",
    "DBInstanceClass",
    "Engine",
    "AllocatedStorage",
    "StorageType",
    "MultiAZ",
    "CacheNodeType",
    "NumCacheNodes",
    "spotPrice",
    "availabilityZone",
    "AvailabilityZone",
    "BillingMode",
    "ProvisionedThroughput",
    "TableSizeBytes",
];

fn default_region() -> String {
    "eu-west-1".into()
}
fn yes() -> bool {
    true
}
fn eighty() -> u32 {
    80
}
fn default_workgroup() -> String {
    "qloudpulse-cost".into()
}
fn default_database() -> String {
    "qloudpulse_cur".into()
}
fn default_table() -> String {
    "cur2".into()
}
fn default_key_file() -> PathBuf {
    "qloudpulse-collect.key".into()
}
fn default_work_dir() -> PathBuf {
    "qloudpulse-collect-work".into()
}
fn default_output_dir() -> PathBuf {
    "bundles".into()
}
fn default_tag_default() -> TagDefault {
    TagDefault::Deny
}
/// The qloudpulse API. Empty in a build without `upload`, which holds no qloudpulse host at all.
pub const QLOUDPULSE_ENDPOINT: &str = if cfg!(feature = "upload") {
    "https://api.qloudpulse.com"
} else {
    ""
};

fn default_endpoint() -> String {
    QLOUDPULSE_ENDPOINT.into()
}
fn one() -> u32 {
    1
}
fn five() -> u32 {
    5
}
fn week_of_hours() -> usize {
    168
}

impl Config {
    pub fn load(path: &Path) -> Result<(Config, Vec<u8>)> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Config = toml::from_str(std::str::from_utf8(&bytes).context("the config is not UTF-8")?)
            .with_context(|| format!("reading {}", path.display()))?;
        config.resolve_paths(path.parent().unwrap_or(Path::new(".")));
        config.check()?;
        Ok((config, bytes))
    }

    /// Relative paths are relative to the config file, so a run from any directory finds the same files.
    fn resolve_paths(&mut self, base: &Path) {
        for p in [&mut self.key_file, &mut self.work_dir, &mut self.output_dir] {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        }
        if let Some(q) = &mut self.deliver.qloudpulse
            && let Some(t) = &mut q.token_file
            && t.is_relative()
        {
            *t = base.join(&*t);
        }
    }

    pub fn check(&self) -> Result<()> {
        if self.source_id.is_empty()
            || !self
                .source_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("source_id must be letters, digits, - and _");
        }
        for rule in &self.fields {
            let last = rule.path.rsplit('.').next().unwrap_or(&rule.path);
            if REQUIRED_FIELDS.contains(&last) {
                bail!(
                    "the rule for {} would change a field qloudpulse prices from; remove it",
                    rule.path
                );
            }
            if rule.action == FieldAction::Replace && rule.with.is_none() {
                bail!("the replace rule for {} needs `with`", rule.path);
            }
            if let Some(p) = &rule.pattern {
                regex::Regex::new(p).with_context(|| format!("the pattern for {}", rule.path))?;
            }
        }
        for r in &self.tags.replace {
            if let Some(p) = &r.pattern {
                regex::Regex::new(p).with_context(|| format!("the pattern for tag {}", r.key))?;
            }
        }
        if let Some(c) = &self.cur
            && (c.periods_back == 0
                || c.periods_back > 3
                || c.account.len() != 12
                || !(51..=99).contains(&c.prediction_interval))
        {
            bail!("[cur] needs a 12-digit account, periods_back from 1 to 3 and prediction_interval from 51 to 99");
        }
        if let Some(e) = &self.environment {
            crate::derive::Deriver::new(e).context("an [environment] rule pattern")?;
        }
        if self.run.every_hours == 0 || self.run.minute > 59 || self.run.daily_hour > 23 {
            bail!("[run] every_hours must be at least 1, minute 0-59 and daily_hour 0-23");
        }
        if let Some(c) = &self.deliver.command
            && c.argv.is_empty()
        {
            bail!("[deliver.command] argv is empty");
        }
        Ok(())
    }

    /// What a rule costs, for `analyze` and `pack`: one line per feature it disables.
    pub fn feature_impact(&self) -> Vec<String> {
        FEATURE_TAGS
            .iter()
            .filter_map(|(key, feature)| match self.tag_action(key) {
                TagAction::Keep | TagAction::Replace(_) => None,
                TagAction::Drop => Some(format!("dropping tag {key} disables {feature}")),
                TagAction::Pseudonymize => Some(format!("pseudonymizing tag {key} makes {feature} show pseudonyms")),
            })
            .collect()
    }

    /// The action for a tag key. Order: drop, pseudonymize, replace, keep, then the default.
    pub fn tag_action(&self, key: &str) -> TagAction {
        // Derived from your own rules before redaction: kept, whatever the default.
        if key == crate::derive::ENV_TAG {
            return TagAction::Keep;
        }
        let t = &self.tags;
        if t.drop.iter().any(|k| k == key) {
            TagAction::Drop
        } else if t.pseudonymize.iter().any(|k| k == key) {
            TagAction::Pseudonymize
        } else if let Some(i) = t.replace.iter().position(|r| r.key == key) {
            TagAction::Replace(i)
        } else if t.keep.iter().any(|k| k == key) || t.default == TagDefault::Allow {
            TagAction::Keep
        } else {
            TagAction::Drop
        }
    }

    /// The call plan's opt-in options this config turns on.
    pub fn options(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.cur.as_ref().is_some_and(|c| c.forecast) {
            out.push("forecast".to_string());
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagAction {
    Keep,
    Drop,
    Pseudonymize,
    /// Index into `tags.replace`.
    Replace(usize),
}

impl TagAction {
    pub fn label(&self) -> &'static str {
        match self {
            TagAction::Keep => "keep",
            TagAction::Drop => "drop",
            TagAction::Pseudonymize => "pseudonymize",
            TagAction::Replace(_) => "replace",
        }
    }
}

/// The starter config `init` writes.
const STARTER_TEMPLATE: &str = r#"# qloudpulse-collect config. It holds no secrets: AWS credentials come from the standard AWS chain
# (environment, profile, SSO, instance or container role), the pseudonym key and the upload token from files.

# Names this client. A company may run several; each AWS account belongs to one source.
source_id = "main"

# profile = "my-aws-profile"
home_region = "eu-west-1"

# Regions to read. Leave it out to read every region enabled in the account.
# regions = ["eu-west-1", "us-east-1"]

# Accounts to read through a role you created in each. Leave them out to read the credentials' own account.
# [[accounts]]
# id = "111122223333"
# role_arn = "arn:aws:iam::111122223333:role/qloudpulse-collect"

key_file = "qloudpulse-collect.key"      # created by `init`; back it up: losing it changes every pseudonym
work_dir = "qloudpulse-collect-work"     # raw snapshots and state; never delivered
output_dir = "bundles"

# Keep a local table of pseudonym -> real value, to read qloudpulse's reports. It stays on this machine.
keep_pseudonym_map = false

# AWS's carbon estimate per account, once a day (sustainability:GetEstimatedCarbonEmissions).
carbon = true

[tags]
# A tag no rule names is dropped ("deny") or kept ("allow").
default = "deny"
# The keys qloudpulse features read. `analyze` lists every key found and what each rule costs.
keep = ["Name", "Environment", "Team", "Company", "eks:cluster-name", "aws:eks:cluster-name",
        "karpenter.sh/nodepool", "eks:nodegroup-name", "aws:autoscaling:groupName"]
drop = []
pseudonymize = []   # e.g. ["Owner"]: the same value always gives the same pseudonym, which only you can reverse

# [[tags.replace]]
# key = "CostCenter"
# pattern = "^(\\d+)-.*$"
# with = "$1"

# Rules for any field, by the end of its path in the answer (list wrappers `item`/`member` left out).
# Actions: drop, replace (with, optional pattern), pseudonymize. Fields qloudpulse prices from are refused.
# [[fields]]
# path = "privateDnsName"
# action = "pseudonymize"

# The bill: CUR 2.0 through Athena in the payer account, once a day (the daily_hour run). Only query results are
# recorded, never the CUR files. The role needs Athena on the workgroup, Glue on the database and table, read on the
# CUR export bucket and write on the workgroup's results location.
# [cur]
# account = "111122223333"
# role_arn = "arn:aws:iam::111122223333:role/qloudpulse-collect-bill"
# region = "eu-west-1"
# workgroup = "qloudpulse-cost"
# database = "qloudpulse_cur"
# table = "cur2"
# scope_tag_keys = []     # the scope tag keys set in qloudpulse: they are part of the query
# periods_back = 1
# forecast = false        # Cost Explorer's forecast per account; AWS bills $0.01 a request
# prediction_interval = 80

# Account IDs and resource IDs: "keep", or "pseudonymize" with their format kept (12 digits; i- and 17 hex digits),
# the same everywhere they appear, ARNs and requests included, so the bill and the inventory still join.
[identifiers]
account_ids = "keep"
resource_ids = "keep"

# Derive the environment before redaction and send it as the tag qloudpulse:environment, so resource names can be
# pseudonymized and costs still group by environment. From a tag you have, else from rules on the Name tag.
# [environment]
# from_tag = "Environment"
# [[environment.rules]]
# pattern = "(?i)\\bprod"
# value = "production"
# [[environment.rules]]
# pattern = "(?i)\\b(dev|test)"
# value = "development"

# Before a bundle is written, what would leave is scanned for keys, tokens, passwords and email addresses: pack stops
# until each is redacted by a rule above, or allowed here. Public IP addresses are reported, never blocking.
[scan]
allow = []          # kinds to report without blocking, e.g. ["email"]
allow_values = []   # whole-value regexes to skip, e.g. ["^noreply@example\\.com$"]

[run]
every_hours = 1
minute = 5          # start 5 minutes past the hour, so the hour just ended is complete
daily_hour = 0      # this UTC hour's run also reads the 90-day spot price history
keep_raw = false
keep_bundles = 168

# Where bundles go after `run` packs them. Any combination; all are off until you set them.
# [deliver.s3]
# bucket = "my-bucket"
# prefix = "qloudpulse/"
# region = "eu-west-1"
# kms_key_id = "alias/my-key"

{qloudpulse}# [deliver.command]
# argv = ["/usr/local/bin/ship-bundle", "{bundle}"]
"#;

#[cfg(feature = "upload")]
const STARTER_UPLOAD: &str = r#"# [deliver.qloudpulse]
# endpoint = "https://api.qloudpulse.com"
# token_file = "upload-token"   # or set QLOUDPULSE_UPLOAD_TOKEN

"#;

/// The starter config `init` writes. A build without `upload` leaves the qloudpulse target out.
pub fn starter() -> String {
    #[cfg(feature = "upload")]
    let upload = STARTER_UPLOAD;
    #[cfg(not(feature = "upload"))]
    let upload = "";
    STARTER_TEMPLATE.replace("{qloudpulse}", upload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.check()?;
        Ok(config)
    }

    #[test]
    fn starter_parses_with_feature_tags_kept() {
        let config = parse(&starter()).unwrap();
        assert_eq!(config.tag_action("Team"), TagAction::Keep);
        assert_eq!(config.tag_action("Owner"), TagAction::Drop);
        assert!(config.feature_impact().is_empty());
    }

    #[test]
    fn refuses_rules_on_priced_fields() {
        let err = parse("source_id = \"a\"\n[[fields]]\npath = \"instancesSet.instanceType\"\naction = \"drop\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("prices from"), "{err}");
    }

    #[test]
    fn rule_order_and_impact() {
        let config =
            parse("source_id = \"a\"\n[tags]\ndefault = \"allow\"\ndrop = [\"Team\"]\npseudonymize = [\"Name\"]\n")
                .unwrap();
        assert_eq!(config.tag_action("Team"), TagAction::Drop);
        assert_eq!(config.tag_action("Anything"), TagAction::Keep);
        let impact = config.feature_impact();
        assert!(impact.iter().any(|l| l.contains("dropping tag Team")), "{impact:?}");
        assert!(
            impact.iter().any(|l| l.contains("pseudonymizing tag Name")),
            "{impact:?}"
        );
    }
}

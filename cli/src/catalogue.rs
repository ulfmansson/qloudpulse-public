//! The resource types qloudpulse prices and the attributes the client sends for each: the catalogue. On every run the
//! client sends qloudpulse the types it found in the preview (`POST …/estimates/catalogue`, type tokens only) and gets
//! back the entries of those it prices; `--catalogue FILE` reads a saved answer, or the whole list, instead.
//!
//! The list lives with the pricing, so qloudpulse prices a new type without a new client. Nothing outside it is read
//! from a resource: an attribute that does not set a price stays in CI. A path `a.b` takes `b` from the object `a`,
//! or from every object in the list `a`. `--dry-run` prints exactly what the list makes the client send.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::request::FORMAT_VERSION;

/// How many steps an attribute path may take, such as MSK's `brokerNodeGroupInfo.storageInfo.ebsStorageInfo.volumeSize`.
pub const MAX_DEPTH: usize = 4;

/// The list as qloudpulse publishes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalogue {
    pub format_version: u32,
    /// Pulumi packages, such as `aws`, whose types outside the list are counted as not priced.
    #[serde(default)]
    pub providers: Vec<String>,
    pub types: Vec<CatalogueType>,
    /// The client releases qloudpulse expects; absent from a catalogue saved before it named them.
    #[serde(default)]
    pub client: Option<ClientVersions>,
}

/// The newest client release, the oldest qloudpulse takes, and where to get the newest.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientVersions {
    pub latest: String,
    pub minimum: String,
    pub download: String,
}

/// One type qloudpulse prices.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogueType {
    /// The Pulumi type token, as `pulumi preview --json` writes it.
    #[serde(rename = "type")]
    pub token: String,
    /// What the comment calls it.
    pub label: String,
    /// The attributes that set its price. Empty for a type priced from usage alone, such as an S3 bucket.
    #[serde(default)]
    pub attributes: Vec<String>,
    /// Attributes whose value is another resource, such as an ECS service's task definition. The client sends that
    /// resource's key, and the resource itself when the change leaves it the same.
    #[serde(default)]
    pub references: Vec<String>,
}

impl Catalogue {
    /// Reads and checks a catalogue.
    pub fn parse(json: &str) -> Result<Catalogue> {
        let catalogue: Catalogue = serde_json::from_str(json).context("the catalogue is not qloudpulse's format")?;
        if catalogue.format_version != FORMAT_VERSION {
            bail!(
                "the catalogue is format {}, and this client reads {FORMAT_VERSION}; update the client",
                catalogue.format_version
            );
        }
        let mut seen = HashSet::new();
        for t in &catalogue.types {
            if !seen.insert(t.token.as_str()) {
                bail!("the catalogue lists {} twice", t.token);
            }
            for a in &t.attributes {
                if a.split('.').count() > MAX_DEPTH || a.split('.').any(str::is_empty) {
                    bail!(
                        "the catalogue's attribute {a:?} of {} is empty or deeper than {MAX_DEPTH}",
                        t.token
                    );
                }
            }
            for r in &t.references {
                if r.is_empty() || r.contains('.') {
                    bail!("the catalogue's reference {r:?} of {} is empty or a path", t.token);
                }
            }
        }
        Ok(catalogue)
    }

    /// The type with this token, if qloudpulse prices it.
    pub fn priced(&self, token: &str) -> Option<&CatalogueType> {
        self.types.iter().find(|t| t.token == token)
    }

    /// Whether a type of one of the catalogue's providers, so that one outside the list is counted as not priced.
    pub fn counts(&self, token: &str) -> bool {
        token
            .split_once(':')
            .is_some_and(|(package, _)| self.providers.iter().any(|p| p == package))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: &str = r#"{"formatVersion": 1, "providers": ["aws"], "types": [
        {"type": "aws:ebs/volume:Volume", "label": "EBS volume", "attributes": ["size", "type"]},
        {"type": "aws:ecs/service:Service", "label": "ECS service", "attributes": ["desiredCount"],
         "references": ["taskDefinition"]}]}"#;

    #[test]
    fn reads_types_and_counts_by_provider() {
        let c = Catalogue::parse(SMALL).unwrap();
        assert_eq!(c.priced("aws:ebs/volume:Volume").unwrap().label, "EBS volume");
        assert_eq!(
            c.priced("aws:ecs/service:Service").unwrap().references,
            ["taskDefinition"]
        );
        assert!(c.priced("aws:s3/bucket:Bucket").is_none());
        assert!(c.counts("aws:s3/bucket:Bucket"));
        assert!(!c.counts("random:index/randomString:RandomString"));
        assert!(!c.counts("awsx:ec2:Vpc"));
    }

    #[test]
    fn a_list_that_breaks_its_rules_is_refused() {
        let twice = r#"{"formatVersion": 1, "types": [{"type": "t", "label": "T"}, {"type": "t", "label": "T"}]}"#;
        assert!(Catalogue::parse(twice).unwrap_err().to_string().contains("twice"));
        let deep = r#"{"formatVersion": 1, "types": [{"type": "t", "label": "T", "attributes": ["a.b.c.d.e"]}]}"#;
        assert!(Catalogue::parse(deep).unwrap_err().to_string().contains("deeper"));
        let newer = r#"{"formatVersion": 2, "types": []}"#;
        assert!(
            Catalogue::parse(newer)
                .unwrap_err()
                .to_string()
                .contains("update the client")
        );
        assert!(Catalogue::parse("[]").is_err());
    }
}

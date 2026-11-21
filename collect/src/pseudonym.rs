//! Pseudonyms: HMAC-SHA256 with a key only the customer holds. The same value always gives the same pseudonym, so
//! grouping survives across bundles, and without the key nobody can reverse it.

use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

pub struct Pseudonymizer {
    key: Vec<u8>,
    /// pseudonym -> real value, kept only when the customer asked for the local map.
    seen: Option<Mutex<BTreeMap<String, String>>>,
}

impl Pseudonymizer {
    pub fn new(key: Vec<u8>, keep_map: bool) -> Pseudonymizer {
        Pseudonymizer {
            key,
            seen: keep_map.then(|| Mutex::new(BTreeMap::new())),
        }
    }

    /// Creates a key file with 32 random bytes, readable by its owner only. Refuses to overwrite one.
    pub fn create_key(path: &Path) -> Result<()> {
        if path.exists() {
            bail!("{} exists; a new key would change every pseudonym", path.display());
        }
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        write_private(path, format!("{}\n", hex::encode(key)).as_bytes())
    }

    pub fn load(path: &Path, keep_map: bool) -> Result<Pseudonymizer> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading the key {} (create it with `qloudpulse-collect init`)",
                path.display()
            )
        })?;
        let key = hex::decode(text.trim()).context("the key file must hold hex")?;
        if key.len() < 32 {
            bail!("the key in {} is shorter than 32 bytes", path.display());
        }
        Ok(Pseudonymizer::new(key, keep_map))
    }

    fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC takes any key length");
        for part in parts {
            mac.update(part);
        }
        mac.finalize().into_bytes().into()
    }

    /// `p_` and 16 hex digits. One domain for every field and tag, so a value pseudonymized in a tag and in a
    /// field gives the same pseudonym and still joins.
    pub fn pseudonym(&self, value: &str) -> String {
        let out = format!(
            "p_{}",
            &hex::encode(self.mac(&[b"qloudpulse-collect/v1\0value\0", value.as_bytes()]))[..16]
        );
        if let Some(seen) = &self.seen {
            seen.lock()
                .expect("pseudonym map")
                .insert(out.clone(), value.to_string());
        }
        out
    }

    /// Names the key without revealing it, for the manifest: a changed fingerprint means every pseudonym changed.
    pub fn fingerprint(&self) -> String {
        hex::encode(self.mac(&[b"qloudpulse-collect/v1\0fingerprint"]))[..16].to_string()
    }

    /// Merges this run's pseudonyms into the local map file.
    pub fn save_map(&self, path: &Path) -> Result<()> {
        let Some(seen) = &self.seen else { return Ok(()) };
        let mut all = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Some((p, v)) = line.split_once('\t') {
                    all.insert(p.to_string(), v.to_string());
                }
            }
        }
        all.extend(seen.lock().expect("pseudonym map").clone());
        let text: String = all.iter().map(|(p, v)| format!("{p}\t{v}\n")).collect();
        write_private(path, text.as_bytes())
    }
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    use std::io::Write;
    options
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?
        .write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_value_same_pseudonym_and_key_changes_everything() {
        let a = Pseudonymizer::new(vec![1; 32], false);
        let b = Pseudonymizer::new(vec![2; 32], false);
        assert_eq!(a.pseudonym("alice@corp"), a.pseudonym("alice@corp"));
        assert_ne!(a.pseudonym("alice@corp"), b.pseudonym("alice@corp"));
        assert!(a.pseudonym("x").starts_with("p_"));
        assert_eq!(a.pseudonym("x").len(), 18);
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn key_file_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        Pseudonymizer::create_key(&path).unwrap();
        assert!(Pseudonymizer::create_key(&path).is_err());
        Pseudonymizer::load(&path, false).unwrap();
    }
}

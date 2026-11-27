//! Pseudonyms: HMAC-SHA256 with a key only the customer holds. The same value always gives the same pseudonym, so
//! grouping survives across bundles, and without the key nobody can reverse it.

use crate::config::Config;
use crate::view_token::{ValueCipher, ViewToken};
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
    /// The `encrypt` action's cipher, from the view token. Loaded by [`Pseudonymizer::for_config`] when a rule
    /// encrypts.
    cipher: Option<ValueCipher>,
}

impl Pseudonymizer {
    pub fn new(key: Vec<u8>, keep_map: bool) -> Pseudonymizer {
        Pseudonymizer {
            key,
            seen: keep_map.then(|| Mutex::new(BTreeMap::new())),
            cipher: None,
        }
    }

    /// Encrypts `encrypt` rules' values under this token.
    pub fn with_view_token(mut self, token: &ViewToken) -> Pseudonymizer {
        self.cipher = Some(token.cipher());
        self
    }

    /// The key a config names, with its view token when a rule encrypts: what redaction needs.
    pub fn for_config(config: &Config, keep_map: bool) -> Result<Pseudonymizer> {
        let p = Pseudonymizer::load(&config.key_file, keep_map)?;
        if !config.encrypts() {
            return Ok(p);
        }
        let token = ViewToken::load(&config.view_token_file).context("a rule encrypts")?;
        Ok(p.with_view_token(&token))
    }

    /// The `encrypt` action: `qpe1_…`, which only the view token reads. None without a token, which drops the
    /// value rather than send it as it is.
    pub fn encrypt(&self, value: &str) -> Option<String> {
        self.cipher.as_ref().map(|c| c.seal(value))
    }

    /// Names the view token without revealing it, when values are encrypted.
    pub fn view_key_id(&self) -> Option<String> {
        self.cipher.as_ref().map(|c| c.key_id().to_string())
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

    /// An account ID as another 12-digit number. Its own domain, so it never equals a value's pseudonym.
    pub fn account_id(&self, id: &str) -> String {
        let mac = self.mac(&[b"qloudpulse-collect/v1\0account\0", id.as_bytes()]);
        let n = u64::from_be_bytes(mac[..8].try_into().expect("8 bytes")) % 1_000_000_000_000;
        let out = format!("{n:012}");
        if let Some(seen) = &self.seen {
            seen.lock().expect("pseudonym map").insert(out.clone(), id.to_string());
        }
        out
    }

    /// A resource ID with its prefix and as many hex digits as the original: `i-0abc…` → `i-` + 17 hex digits.
    pub fn resource_id(&self, prefix: &str, hex: &str) -> String {
        let full = format!("{prefix}-{hex}");
        let mac = hex::encode(self.mac(&[b"qloudpulse-collect/v1\0resource\0", full.as_bytes()]));
        let out = format!("{prefix}-{}", &mac[..hex.len()]);
        if let Some(seen) = &self.seen {
            seen.lock().expect("pseudonym map").insert(out.clone(), full);
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

/// What `rotate-key` did.
#[derive(Debug)]
pub struct Rotated {
    pub old_fingerprint: String,
    pub new_fingerprint: String,
    /// Where the old key now is.
    pub retired: std::path::PathBuf,
    /// The old-to-new pseudonym file and how many it relinks, when a local map existed.
    pub relink: Option<(std::path::PathBuf, usize)>,
}

/// Replaces the key at `key_file` with a new one. With the local map (`map_file`), writes `old<TAB>new` for every
/// pseudonym it knows, which qloudpulse can take to relink history (it holds no real values), and rewrites the map
/// under the new key. Without the map no relinking is possible, so it refuses unless `force`.
pub fn rotate(key_file: &Path, map_file: &Path, force: bool) -> Result<Rotated> {
    let old = Pseudonymizer::load(key_file, false)?;
    let known: Vec<(String, String)> = match std::fs::read_to_string(map_file) {
        Ok(text) => text
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(p, v)| (p.to_string(), v.to_string()))
            .collect(),
        Err(_) if force => Vec::new(),
        Err(_) => bail!(
            "no pseudonym map at {}: history could not be relinked after the rotation. Set keep_pseudonym_map = true \
             and pack once to build it, or rotate with --force to accept the break",
            map_file.display()
        ),
    };
    let retired = key_file.with_extension(format!("key.retired-{}", old.fingerprint()));
    std::fs::rename(key_file, &retired).with_context(|| format!("retiring {}", key_file.display()))?;
    Pseudonymizer::create_key(key_file)?;
    let new = Pseudonymizer::load(key_file, true)?;
    let mut relink = String::new();
    for (old_p, real) in &known {
        let new_p = if old_p.starts_with("p_") {
            new.pseudonym(real)
        } else if old_p.len() == 12 && old_p.bytes().all(|b| b.is_ascii_digit()) {
            new.account_id(real)
        } else if let Some((prefix, hex)) = real.rsplit_once('-') {
            new.resource_id(prefix, hex)
        } else {
            continue;
        };
        relink.push_str(&format!("{old_p}\t{new_p}\n"));
    }
    let relink = if known.is_empty() {
        None
    } else {
        let dir = map_file.parent().unwrap_or(Path::new("."));
        let path = dir.join(format!(
            "pseudonym-relink-{}-to-{}.tsv",
            old.fingerprint(),
            new.fingerprint()
        ));
        write_private(&path, relink.as_bytes())?;
        std::fs::remove_file(map_file)?;
        new.save_map(map_file)?;
        Some((path, known.len()))
    };
    Ok(Rotated {
        old_fingerprint: old.fingerprint(),
        new_fingerprint: new.fingerprint(),
        retired,
        relink,
    })
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
    fn rotation_relinks_every_known_pseudonym_without_real_values() {
        let dir = tempfile::tempdir().unwrap();
        let (key, map) = (dir.path().join("k"), dir.path().join("pseudonyms.tsv"));
        Pseudonymizer::create_key(&key).unwrap();
        let old = Pseudonymizer::load(&key, true).unwrap();
        let (v, a, r) = (
            old.pseudonym("alice@corp"),
            old.account_id("111122223333"),
            old.resource_id("i", "0123456789abcdef0"),
        );
        old.save_map(&map).unwrap();
        assert!(rotate(&dir.path().join("missing"), &map, false).is_err());

        let rotated = rotate(&key, &map, false).unwrap();
        let new = Pseudonymizer::load(&key, false).unwrap();
        assert_ne!(rotated.old_fingerprint, rotated.new_fingerprint);
        assert!(rotated.retired.exists());
        let (path, n) = rotated.relink.unwrap();
        assert_eq!(n, 3);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains(&format!("{v}\t{}", new.pseudonym("alice@corp"))));
        assert!(text.contains(&format!("{a}\t{}", new.account_id("111122223333"))));
        assert!(text.contains(&format!("{r}\t{}", new.resource_id("i", "0123456789abcdef0"))));
        assert!(
            !text.contains("alice") && !text.contains("111122223333"),
            "no real values"
        );
        assert!(
            std::fs::read_to_string(&map)
                .unwrap()
                .contains(&new.pseudonym("alice@corp")),
            "the map follows the new key"
        );
    }

    #[test]
    fn an_encrypt_rule_needs_the_view_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, "source_id = \"a\"\n[tags]\nencrypt = [\"Owner\"]\n").unwrap();
        let (config, _) = Config::load(&path).unwrap();
        Pseudonymizer::create_key(&config.key_file).unwrap();
        let err = Pseudonymizer::for_config(&config, false).err().unwrap();
        assert!(format!("{err:#}").contains("view-token --create"), "{err:#}");
        let token = ViewToken::create(&config.view_token_file).unwrap();
        let p = Pseudonymizer::for_config(&config, false).unwrap();
        assert_eq!(p.view_key_id(), Some(token.key_id()));

        std::fs::write(&path, "source_id = \"a\"\n").unwrap();
        let (plain, _) = Config::load(&path).unwrap();
        std::fs::remove_file(&plain.view_token_file).unwrap();
        assert_eq!(
            Pseudonymizer::for_config(&plain, false).unwrap().view_key_id(),
            None,
            "no encrypt rule, no token needed"
        );
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

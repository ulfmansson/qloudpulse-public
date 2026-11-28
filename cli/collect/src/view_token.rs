//! Encrypted values and the view token.
//!
//! A tag or field with the `encrypt` action leaves as `qpe1_<key id>_<data>`: AES-256-GCM under a key derived from
//! the **view token**, `qp_` and 43 base64url characters (32 random bytes) that the client creates and only the
//! customer holds. qloudpulse stores and groups the encrypted value but cannot read it. A person who has the token
//! enters it in qloudpulse's web UI, and the page decrypts the values in the browser with WebCrypto: the token and
//! the real values never reach qloudpulse. `web/qloudpulse-values.js` is that browser code.
//!
//! The encryption is deterministic, which grouping needs: the same value always gives the same `qpe1_…`, across
//! bundles, so costs still add up per value and a name carried into a later request still matches on replay. Like a
//! pseudonym it shows which resources share a value, and nothing else. The nonce is an HMAC of the value (a
//! synthetic IV), so two different values never share a nonce; the value is padded to a multiple of 16 bytes so
//! short values do not give away their exact length.
//!
//! Every key is derived from the token with HMAC-SHA256 under a fixed label:
//!
//! | key | label |
//! |---|---|
//! | encryption key | `qloudpulse-collect/v1\0value-key` |
//! | nonce key | `qloudpulse-collect/v1\0value-nonce` |
//! | key id (first 4 bytes, hex) | `qloudpulse-collect/v1\0key-id` |
//!
//! `data` is base64url (no padding) of `nonce (12) || ciphertext || tag (16)`, and the additional data is the
//! value's prefix, `qpe1_<key id>_`, so a value cannot be moved under another key id.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::path::Path;

type HmacSha256 = Hmac<Sha256>;

/// What every view token starts with.
pub const TOKEN_PREFIX: &str = "qp_";
/// What every encrypted value starts with; the digit is the format's version.
pub const VALUE_PREFIX: &str = "qpe1_";

/// The secret behind encrypted values. Never written into a bundle, a log or the manifest.
pub struct ViewToken {
    secret: [u8; 32],
}

impl std::fmt::Debug for ViewToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ViewToken({})", self.key_id())
    }
}

impl ViewToken {
    pub fn generate() -> ViewToken {
        let mut secret = [0u8; 32];
        rand::rng().fill_bytes(&mut secret);
        ViewToken { secret }
    }

    pub fn from_secret(secret: [u8; 32]) -> ViewToken {
        ViewToken { secret }
    }

    /// `qp_` and 43 base64url characters. Surrounding white space is ignored, anything else wrong is refused.
    pub fn parse(text: &str) -> Result<ViewToken> {
        let body = text
            .trim()
            .strip_prefix(TOKEN_PREFIX)
            .with_context(|| format!("a view token starts with {TOKEN_PREFIX}"))?;
        if body.len() != 43 {
            bail!("a view token is {TOKEN_PREFIX} and 43 characters");
        }
        let bytes = B64.decode(body).context("a view token's characters are base64url")?;
        let secret: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("a view token holds 32 bytes"))?;
        Ok(ViewToken { secret })
    }

    /// The token as the customer types it into qloudpulse.
    pub fn text(&self) -> String {
        format!("{TOKEN_PREFIX}{}", B64.encode(self.secret))
    }

    fn derive(&self, label: &[u8]) -> [u8; 32] {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.secret).expect("HMAC takes any key length");
        mac.update(b"qloudpulse-collect/v1\0");
        mac.update(label);
        mac.finalize().into_bytes().into()
    }

    /// Eight hex digits that name the token without revealing it. In every value it encrypts, and in the manifest,
    /// so qloudpulse can say which token a value needs.
    pub fn key_id(&self) -> String {
        hex::encode(&self.derive(b"key-id")[..4])
    }

    /// Creates the token file, readable by its owner only. Refuses to overwrite one: values encrypted under the old
    /// token could no longer be read with the new one.
    pub fn create(path: &Path) -> Result<ViewToken> {
        if path.exists() {
            bail!(
                "{} exists; a new view token could not read the values the old one encrypted",
                path.display()
            );
        }
        let token = ViewToken::generate();
        crate::pseudonym::write_private(path, format!("{}\n", token.text()).as_bytes())?;
        Ok(token)
    }

    pub fn load(path: &Path) -> Result<ViewToken> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading the view token {} (create it with `qloudpulse-collect view-token --create`)",
                path.display()
            )
        })?;
        ViewToken::parse(&text).with_context(|| format!("the view token in {}", path.display()))
    }

    /// The cipher for values, with its derived keys.
    pub fn cipher(&self) -> ValueCipher {
        let key_id = self.key_id();
        ValueCipher {
            aead: Aes256Gcm::new_from_slice(&self.derive(b"value-key")).expect("a 32-byte key"),
            nonce_key: self.derive(b"value-nonce"),
            prefix: format!("{VALUE_PREFIX}{key_id}_"),
            key_id,
        }
    }
}

/// Encrypts and decrypts values under one view token.
pub struct ValueCipher {
    aead: Aes256Gcm,
    nonce_key: [u8; 32],
    key_id: String,
    /// `qpe1_<key id>_`: the start of every value, and the additional data.
    prefix: String,
}

impl ValueCipher {
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    fn nonce(&self, value: &[u8]) -> [u8; 12] {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.nonce_key).expect("HMAC takes any key length");
        mac.update(value);
        mac.finalize().into_bytes()[..12].try_into().expect("12 bytes")
    }

    /// `qpe1_<key id>_<data>`. The same value always gives the same output.
    pub fn seal(&self, value: &str) -> String {
        let nonce = self.nonce(value.as_bytes());
        let sealed = self
            .aead
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &pad(value.as_bytes()),
                    aad: self.prefix.as_bytes(),
                },
            )
            .expect("AES-GCM encrypts any length a tag can have");
        let mut data = nonce.to_vec();
        data.extend(sealed);
        format!("{}{}", self.prefix, B64.encode(data))
    }

    /// The real value of a `qpe1_…` this token encrypted. Refuses one under another key id, and one that was
    /// changed.
    pub fn open(&self, sealed: &str) -> Result<String> {
        let Some(data) = sealed.strip_prefix(&self.prefix) else {
            match sealed.strip_prefix(VALUE_PREFIX).and_then(|r| r.get(..8)) {
                Some(id) => bail!("encrypted with the view token {id}, not {}", self.key_id),
                None => bail!("not an encrypted value"),
            }
        };
        let data = B64.decode(data).context("not an encrypted value")?;
        if data.len() < 12 + 16 + 16 {
            bail!("not an encrypted value");
        }
        let (nonce, sealed) = data.split_at(12);
        let padded = self
            .aead
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: self.prefix.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("the value was changed, or the token is not the one it was encrypted with"))?;
        let value = unpad(&padded).context("the value's padding")?;
        if self.nonce(value) != nonce {
            bail!("the value's nonce does not match it");
        }
        String::from_utf8(value.to_vec()).context("the value is not UTF-8")
    }
}

/// ISO/IEC 7816-4: 0x80, then zeros up to a multiple of 16 bytes. Always adds at least one byte.
fn pad(value: &[u8]) -> Vec<u8> {
    let mut out = value.to_vec();
    out.push(0x80);
    while !out.len().is_multiple_of(16) {
        out.push(0);
    }
    out
}

fn unpad(padded: &[u8]) -> Result<&[u8]> {
    let end = padded.iter().rposition(|&b| b != 0).context("no padding marker")?;
    if padded[end] != 0x80 || padded.len() - end > 16 {
        bail!("bad padding");
    }
    Ok(&padded[..end])
}

/// Test vectors shared with the browser code: `web/test-vectors.json` must be exactly what this produces.
pub fn test_vectors() -> serde_json::Value {
    let token = ViewToken::from_secret(std::array::from_fn(|i| i as u8));
    let cipher = token.cipher();
    let values = [
        "",
        "web-prod-1",
        "alice@example.com",
        "exactly sixteen!",
        "Ünïcødé ✓ 東京",
        "a value long enough to need several blocks of AES, as a description tag might be",
    ];
    serde_json::json!({
        "comment": "Generated by `cargo test` in cli/collect (view_token::tests::browser_vectors_are_current); \
                    the browser code's tests decrypt every value with the token.",
        "token": token.text(),
        "key_id": token.key_id(),
        "values": values
            .iter()
            .map(|v| serde_json::json!({"plain": v, "sealed": cipher.seal(v)}))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_deterministic() {
        let token = ViewToken::generate();
        let c = token.cipher();
        for v in ["", "x", "web-prod-1", "exactly sixteen!", "Ünïcødé ✓ 東京"] {
            let sealed = c.seal(v);
            assert_eq!(sealed, c.seal(v), "same value, same output");
            assert!(sealed.starts_with(&format!("qpe1_{}_", token.key_id())), "{sealed}");
            assert!(!sealed.contains(v) || v.len() < 2, "{sealed}");
            assert_eq!(c.open(&sealed).unwrap(), v);
        }
        assert_ne!(c.seal("a"), c.seal("b"));
    }

    #[test]
    fn short_values_share_a_length() {
        let c = ViewToken::generate().cipher();
        assert_eq!(c.seal("a").len(), c.seal("fifteen chars!!").len());
        assert!(c.seal("sixteen chars!!!").len() > c.seal("a").len());
    }

    #[test]
    fn another_token_or_a_changed_value_is_refused() {
        let (a, b) = (ViewToken::generate().cipher(), ViewToken::generate().cipher());
        let sealed = a.seal("payments");
        assert!(b.open(&sealed).unwrap_err().to_string().contains(a.key_id()));
        let mut changed = sealed.clone();
        let last = changed.pop().unwrap();
        changed.push(if last == 'A' { 'B' } else { 'A' });
        assert!(a.open(&changed).is_err());
        // The same data under another key id's prefix fails too: the prefix is the additional data.
        let moved = sealed.replacen(a.key_id(), "00000000", 1);
        assert!(a.open(&moved).is_err());
        assert!(a.open("p_0123456789abcdef").is_err());
    }

    #[test]
    fn token_text_round_trips_and_bad_tokens_are_refused() {
        let token = ViewToken::generate();
        let text = token.text();
        assert!(text.starts_with("qp_") && text.len() == 46, "{text}");
        assert_eq!(
            ViewToken::parse(&format!("  {text}\n")).unwrap().key_id(),
            token.key_id()
        );
        for bad in [
            "",
            "qp_",
            &text[1..],
            &text[..45],
            &format!("{text}A"),
            &text.replace("qp_", "qpev_"),
        ] {
            assert!(ViewToken::parse(bad).is_err(), "{bad}");
        }
        assert!(
            !format!("{token:?}").contains(&text[3..]),
            "Debug never prints the secret"
        );
    }

    #[test]
    fn token_file_is_created_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        let made = ViewToken::create(&path).unwrap();
        assert!(ViewToken::create(&path).is_err());
        assert_eq!(ViewToken::load(&path).unwrap().key_id(), made.key_id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    /// The browser code is tested against `web/test-vectors.json`; this keeps that file what the client produces.
    #[test]
    fn browser_vectors_are_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/test-vectors.json");
        let want = serde_json::to_string_pretty(&test_vectors()).unwrap() + "\n";
        if std::env::var_os("QLOUDPULSE_WRITE_VECTORS").is_some() {
            std::fs::write(&path, &want).unwrap();
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            have,
            want,
            "{} is stale: run QLOUDPULSE_WRITE_VECTORS=1 cargo test browser_vectors",
            path.display()
        );
    }
}

//! Encrypted bundles: a JWE (RFC 7516, compact) with `ECDH-ES+A256KW` and `A256GCM` to a P-256 public key, the
//! algorithms qloudpulse's support bundles already use, so the backend decrypts both the same way.
//!
//! The plaintext `.tgz` stays local next to its `.tgz.jwe`: you inspect the plaintext, the JWE is what is delivered.
//! Its protected header carries the plaintext's SHA-256 (`qp_sha256`), so the two can be matched.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::{PublicKey, SecretKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const ALG: &str = "ECDH-ES+A256KW";
pub const ENC: &str = "A256GCM";

/// The key bundles are encrypted to.
#[derive(Debug)]
pub struct Recipient {
    pub kid: String,
    pub key: PublicKey,
}

impl Recipient {
    /// A public JWK (`{"kty":"EC","crv":"P-256","kid":…,"x":…,"y":…}`). A file holding a private part is refused:
    /// the client must never hold qloudpulse's private key.
    pub fn load(path: &Path) -> Result<Recipient> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading the public key {}", path.display()))?;
        let jwk: Value = serde_json::from_str(&text).context("the public key must be a JWK")?;
        if jwk.get("d").is_some() {
            bail!("{} holds a private key; give the public part only", path.display());
        }
        if jwk["kty"] != "EC" || jwk["crv"] != "P-256" {
            bail!("the public key must be an EC P-256 JWK");
        }
        let key = public_from_jwk(&jwk).context("the public key's x and y")?;
        Ok(Recipient {
            kid: jwk["kid"].as_str().unwrap_or_default().to_string(),
            key,
        })
    }
}

/// A P-256 public key from a JWK's `x` and `y`.
pub fn public_from_jwk(jwk: &Value) -> Result<PublicKey> {
    let coord = |name: &str| -> Result<Vec<u8>> {
        let b = B64.decode(jwk[name].as_str().with_context(|| format!("no {name}"))?)?;
        anyhow::ensure!(b.len() == 32, "{name} is not 32 bytes");
        Ok(b)
    };
    let mut sec1 = vec![4u8];
    sec1.extend(coord("x")?);
    sec1.extend(coord("y")?);
    PublicKey::from_sec1_bytes(&sec1).map_err(|_| anyhow::anyhow!("x and y are not a P-256 point"))
}

/// RFC 7518 §4.6.2: Concat KDF with SHA-256, one round, for a key of `bits`.
pub fn concat_kdf(z: &[u8], alg: &str, apu: &[u8], apv: &[u8], bits: u32) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(1u32.to_be_bytes());
    h.update(z);
    for part in [alg.as_bytes(), apu, apv] {
        h.update((part.len() as u32).to_be_bytes());
        h.update(part);
    }
    h.update(bits.to_be_bytes());
    h.finalize()[..(bits / 8) as usize].to_vec()
}

fn random_secret() -> SecretKey {
    loop {
        let bytes: [u8; 32] = rand::random();
        if let Ok(k) = SecretKey::from_slice(&bytes) {
            return k;
        }
    }
}

fn shared(secret: &SecretKey, public: &PublicKey) -> Vec<u8> {
    p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), public.as_affine())
        .raw_secret_bytes()
        .to_vec()
}

/// The compact JWE of `plaintext`.
pub fn encrypt(plaintext: &[u8], to: &Recipient) -> Result<String> {
    let ephemeral = random_secret();
    let point = ephemeral.public_key().to_sec1_point(false);
    let (x, y) = (
        point.x().context("an uncompressed point")?,
        point.y().context("an uncompressed point")?,
    );
    let header = json!({
        "alg": ALG,
        "enc": ENC,
        "kid": to.kid,
        "cty": "application/gzip",
        "epk": {"kty": "EC", "crv": "P-256", "x": B64.encode(x), "y": B64.encode(y)},
        "qp_sha256": hex::encode(Sha256::digest(plaintext)),
    });
    let protected = B64.encode(header.to_string());
    let kek: [u8; 32] = concat_kdf(&shared(&ephemeral, &to.key), ALG, b"", b"", 256)
        .try_into()
        .expect("32 bytes");
    let cek: [u8; 32] = rand::random();
    let wrapped = aes_kw::KekAes256::from(kek)
        .wrap_vec(&cek)
        .map_err(|e| anyhow::anyhow!("key wrap: {e}"))?;
    let iv: [u8; 12] = rand::random();
    let sealed = Aes256Gcm::new_from_slice(&cek)
        .expect("a 32-byte key")
        .encrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: plaintext,
                aad: protected.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("encrypting the bundle"))?;
    let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);
    Ok(format!(
        "{protected}.{}.{}.{}.{}",
        B64.encode(wrapped),
        B64.encode(iv),
        B64.encode(ciphertext),
        B64.encode(tag)
    ))
}

/// Writes `<bundle>.jwe` next to `bundle`.
pub fn encrypt_file(bundle: &Path, to: &Recipient) -> Result<std::path::PathBuf> {
    let jwe = encrypt(&std::fs::read(bundle)?, to)?;
    let path = jwe_path(bundle);
    std::fs::write(&path, jwe)?;
    Ok(path)
}

pub fn jwe_path(bundle: &Path) -> std::path::PathBuf {
    let mut name = bundle.file_name().expect("a bundle file").to_os_string();
    name.push(".jwe");
    bundle.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decrypt(jwe: &str, secret: &SecretKey) -> Vec<u8> {
        let parts: Vec<&str> = jwe.split('.').collect();
        let header: Value = serde_json::from_slice(&B64.decode(parts[0]).unwrap()).unwrap();
        let epk = public_from_jwk(&header["epk"]).unwrap();
        let kek: [u8; 32] = concat_kdf(&shared(secret, &epk), ALG, b"", b"", 256)
            .try_into()
            .unwrap();
        let cek = aes_kw::KekAes256::from(kek)
            .unwrap_vec(&B64.decode(parts[1]).unwrap())
            .unwrap();
        let mut sealed = B64.decode(parts[3]).unwrap();
        sealed.extend(B64.decode(parts[4]).unwrap());
        Aes256Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(
                Nonce::from_slice(&B64.decode(parts[2]).unwrap()),
                Payload {
                    msg: &sealed,
                    aad: parts[0].as_bytes(),
                },
            )
            .unwrap()
    }

    #[test]
    fn round_trips_to_the_recipient_only() {
        let secret = random_secret();
        let to = Recipient {
            kid: "qp-bundles-1".into(),
            key: secret.public_key(),
        };
        let jwe = encrypt(b"bundle bytes", &to).unwrap();
        assert_eq!(jwe.split('.').count(), 5);
        let header: Value = serde_json::from_slice(&B64.decode(jwe.split('.').next().unwrap()).unwrap()).unwrap();
        assert_eq!(
            (header["alg"].as_str(), header["enc"].as_str(), header["kid"].as_str()),
            (Some(ALG), Some(ENC), Some("qp-bundles-1"))
        );
        assert_eq!(decrypt(&jwe, &secret), b"bundle bytes");
    }

    /// RFC 7518 Appendix C: ECDH-ES between Alice's ephemeral key and Bob's, for A128GCM with apu "Alice", apv "Bob".
    #[test]
    fn concat_kdf_matches_rfc_7518_appendix_c() {
        let alice = SecretKey::from_slice(&B64.decode("0_NxaRPUMQoAJt50Gz8YiTr8gRTwyEaCumd-MToTmIo").unwrap()).unwrap();
        let bob = public_from_jwk(&json!({
            "x": "weNJy2HscCSM6AEDTDg04biOvhFhyyWvOHQfeF_PxMQ",
            "y": "e8lnCO-AlStT-NJVX-crhB7QRYhiix03illJOVAOyck",
        }))
        .unwrap();
        let key = concat_kdf(&shared(&alice, &bob), "A128GCM", b"Alice", b"Bob", 128);
        assert_eq!(B64.encode(key), "VqqN6vgjbSBcIijNcacQGg");
    }

    #[test]
    fn refuses_a_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.jwk");
        std::fs::write(&path, r#"{"kty":"EC","crv":"P-256","x":"a","y":"b","d":"c"}"#).unwrap();
        assert!(Recipient::load(&path).unwrap_err().to_string().contains("private"));
    }
}

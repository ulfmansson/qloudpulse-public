//! The one piece of qloudpulse code in the client: uploading a finished bundle. Built only with the `upload`
//! feature; `cargo build --no-default-features` leaves it out, and that build cannot contact qloudpulse at all.
//!
//! It sends the bundle's bytes and reads back only the status and a reference. The token is a bundle-upload token:
//! one company, upload only.

use crate::aws::{HttpRequest, Transport};
use crate::bundle::SCHEMA_VERSION;
use crate::config::QloudpulseTarget;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub const TOKEN_ENV: &str = "QLOUDPULSE_UPLOAD_TOKEN";

/// An encrypted bundle is a JWE; a plain one a gzip.
pub fn content_type(name: &str) -> &'static str {
    if name.ends_with(".jwe") {
        "application/jose"
    } else {
        "application/gzip"
    }
}

pub fn token(target: &QloudpulseTarget) -> Result<String> {
    if let Ok(t) = std::env::var(TOKEN_ENV)
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let file = target
        .token_file
        .as_ref()
        .with_context(|| format!("set {TOKEN_ENV} or [deliver.qloudpulse] token_file"))?;
    let t = std::fs::read_to_string(file).with_context(|| format!("reading the upload token {}", file.display()))?;
    Ok(t.trim().to_string())
}

pub fn request(target: &QloudpulseTarget, token: &str, name: &str, bytes: &[u8]) -> Result<HttpRequest> {
    let endpoint = target.endpoint.trim_end_matches('/');
    if !endpoint.starts_with("https://")
        && !endpoint.starts_with("http://127.0.0.1")
        && !endpoint.starts_with("http://localhost")
    {
        bail!("the qloudpulse endpoint must be https");
    }
    Ok(HttpRequest {
        method: "POST".into(),
        url: format!("{endpoint}/v1/collection-bundles"),
        headers: vec![
            ("authorization".into(), format!("Bearer {token}")),
            ("content-type".into(), content_type(name).into()),
            ("x-qloudpulse-bundle-schema".into(), SCHEMA_VERSION.into()),
            ("x-qloudpulse-bundle-sha256".into(), hex::encode(Sha256::digest(bytes))),
        ],
        body: bytes.to_vec(),
    })
}

pub fn upload(transport: &dyn Transport, target: &QloudpulseTarget, name: &str, bytes: &[u8]) -> Result<String> {
    let token = token(target)?;
    let response = transport.send(&request(target, &token, name, bytes)?)?;
    let text: String = String::from_utf8_lossy(&response.body).chars().take(300).collect();
    match response.status {
        200 | 201 => {
            let reference = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("reference").and_then(|r| r.as_str()).map(str::to_string))
                .unwrap_or_default();
            Ok(format!("{name} accepted, reference {reference}"))
        }
        409 => Ok(format!("{name} was already delivered")),
        401 | 403 => bail!("qloudpulse refused the upload token ({})", response.status),
        status => bail!("qloudpulse answered {status}: {text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::HttpResponse;
    use std::sync::Mutex;

    struct Answer(u16, &'static str, Mutex<Vec<HttpRequest>>);
    impl Transport for Answer {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            self.2.lock().unwrap().push(request.clone());
            Ok(HttpResponse {
                status: self.0,
                headers: vec![],
                body: self.1.into(),
            })
        }
    }

    fn target(dir: &std::path::Path) -> QloudpulseTarget {
        let file = dir.join("token");
        std::fs::write(&file, "qpbu_test\n").unwrap();
        QloudpulseTarget {
            endpoint: "https://api.example.test".into(),
            token_file: Some(file),
        }
    }

    #[test]
    fn posts_the_bundle_with_the_token_and_accepts_409() {
        let dir = tempfile::tempdir().unwrap();
        let ok = Answer(201, r#"{"reference":"r1"}"#, Mutex::new(vec![]));
        let detail = upload(&ok, &target(dir.path()), "b.tgz", b"bytes").unwrap();
        assert!(detail.contains("r1"));
        let sent = &ok.2.lock().unwrap()[0];
        assert_eq!(sent.url, "https://api.example.test/v1/collection-bundles");
        assert!(
            sent.headers
                .contains(&("authorization".into(), "Bearer qpbu_test".into()))
        );
        assert!(upload(&Answer(409, "", Mutex::new(vec![])), &target(dir.path()), "b.tgz", b"x").is_ok());
        assert!(
            upload(
                &Answer(422, "bad schema", Mutex::new(vec![])),
                &target(dir.path()),
                "b.tgz",
                b"x"
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_plain_http() {
        let t = QloudpulseTarget {
            endpoint: "http://api.example.test".into(),
            token_file: None,
        };
        assert!(request(&t, "x", "b.tgz", b"").is_err());
    }
}

//! The one piece of qloudpulse code in the client: uploading a finished bundle. Built only with the `upload`
//! feature; `cargo build --no-default-features` leaves it out, and that build cannot contact qloudpulse at all.
//!
//! It signs in exactly as the qloudpulse CLI does, with the CLI's own code: the token is `QLOUDPULSE_TOKEN` when set;
//! else the one `qloudpulse login` (or `qloudpulse-collect login`) kept on this machine, in the system keychain or the
//! credentials file; else, at a terminal, a sign-in with the browser. An unattended run without either stops with
//! the error that says where a token is made. It sends the bundle's bytes and reads back only the status and a
//! reference.

use crate::aws::{HttpRequest, Transport};
use crate::bundle::SCHEMA_VERSION;
use crate::config::QloudpulseTarget;
use anyhow::{Context, Result};
use qloudpulse_stack::{credentials, login, send, token};
use sha2::{Digest, Sha256};

pub const TOKEN_ENV: &str = "QLOUDPULSE_TOKEN";

/// An encrypted bundle is a JWE; a plain one a gzip.
pub fn content_type(name: &str) -> &'static str {
    if name.ends_with(".jwe") {
        "application/jose"
    } else {
        "application/gzip"
    }
}

/// Signs in with the browser and keeps the token, as `qloudpulse login` does. The token is never said.
pub fn sign_in(endpoint: &str, terminal: &mut impl token::Terminal) -> Result<String> {
    let origin = login::origin(endpoint)?;
    let signed_in = login::sign_in(
        &origin,
        &mut login::Network,
        &mut login::SystemClock::default(),
        &mut login::SystemBrowser,
        terminal,
    )?;
    let saved = credentials::Saved {
        token: signed_in.token.clone(),
        company: signed_in.company.clone(),
        expires_at: signed_in.expires_at.clone(),
    };
    let kept = credentials::system().save(&origin, &saved)?;
    let company = login::plain(&signed_in.company);
    terminal.say(&format!(
        "qloudpulse-collect: signed in to {company}; remove the token under Your tokens ({}).",
        login::tokens_page(&origin)
    ));
    terminal.say(&match kept {
        credentials::Kept::Keychain => "qloudpulse-collect: it is kept in the system keychain.".to_string(),
        credentials::Kept::File(path) => {
            format!(
                "qloudpulse-collect: no system keychain here, so it is kept in {}, which only you can read.",
                path.display()
            )
        }
    });
    Ok(signed_in.token)
}

/// Removes the token kept for this endpoint's qloudpulse. Whether there was one.
pub fn sign_out(endpoint: &str) -> Result<bool> {
    credentials::system().remove(&login::origin(endpoint)?)
}

/// The token for an upload: the CLI's own lookup (`QLOUDPULSE_TOKEN`, the kept token, a sign-in at a terminal).
pub fn token(endpoint: &str) -> Result<String> {
    let origin = login::origin(endpoint)?;
    token::resolve(
        std::env::var(TOKEN_ENV).ok(),
        endpoint,
        || credentials::system().load(&origin, credentials::now()).map(|s| s.token),
        &mut token::Tty,
        |terminal| sign_in(endpoint, terminal),
    )
}

pub fn request(target: &QloudpulseTarget, token: &str, name: &str, bytes: &[u8]) -> Result<HttpRequest> {
    let endpoint = target.endpoint.trim_end_matches('/');
    send::check_endpoint(endpoint)?;
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
    let token = token(&target.endpoint)?;
    upload_with(transport, target, &token, name, bytes)
}

pub fn upload_with(
    transport: &dyn Transport,
    target: &QloudpulseTarget,
    token: &str,
    name: &str,
    bytes: &[u8],
) -> Result<String> {
    let response = transport.send(&request(target, token, name, bytes)?)?;
    let text: String = String::from_utf8_lossy(&response.body).chars().take(300).collect();
    let outcome: Result<String> = match response.status {
        200 | 201 => {
            let reference = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("reference").and_then(|r| r.as_str()).map(str::to_string))
                .unwrap_or_default();
            Ok(format!("{name} accepted, reference {reference}"))
        }
        409 => Ok(format!("{name} was already delivered")),
        401 | 403 => Err(anyhow::anyhow!(
            "qloudpulse refused the token ({}): {}",
            response.status,
            send::missing_token(&target.endpoint)
        )),
        status => Err(anyhow::anyhow!("qloudpulse answered {status}: {text}")),
    };
    outcome.context("uploading to qloudpulse")
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

    const TOKEN: &str = "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

    fn target() -> QloudpulseTarget {
        QloudpulseTarget {
            endpoint: "https://api.example.test".into(),
        }
    }

    #[test]
    fn posts_the_bundle_with_the_token_and_accepts_409() {
        let ok = Answer(201, r#"{"reference":"r1"}"#, Mutex::new(vec![]));
        let detail = upload_with(&ok, &target(), TOKEN, "b.tgz", b"bytes").unwrap();
        assert!(detail.contains("r1"));
        let sent = &ok.2.lock().unwrap()[0];
        assert_eq!(sent.url, "https://api.example.test/v1/collection-bundles");
        assert!(
            sent.headers
                .contains(&("authorization".into(), format!("Bearer {TOKEN}")))
        );
        assert!(upload_with(&Answer(409, "", Mutex::new(vec![])), &target(), TOKEN, "b.tgz", b"x").is_ok());
        assert!(
            upload_with(
                &Answer(422, "bad schema", Mutex::new(vec![])),
                &target(),
                TOKEN,
                "b.tgz",
                b"x"
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_plain_http_but_not_localhost() {
        let t = QloudpulseTarget {
            endpoint: "http://api.example.test".into(),
        };
        assert!(request(&t, "x", "b.tgz", b"").is_err());
        let t = QloudpulseTarget {
            endpoint: "http://127.0.0.1:8080".into(),
        };
        assert!(request(&t, "x", "b.tgz", b"").is_ok());
    }

    #[test]
    fn the_environment_token_wins() {
        // SAFETY: tests in this module do not read QLOUDPULSE_TOKEN concurrently.
        unsafe { std::env::set_var(TOKEN_ENV, TOKEN) };
        assert_eq!(token("https://api.example.test").unwrap(), TOKEN);
        unsafe { std::env::remove_var(TOKEN_ENV) };
    }
}

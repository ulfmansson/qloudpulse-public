//! Asks qloudpulse what to send for the types a preview holds (`POST /v1/estimates/catalogue`), and sends it the
//! request (`POST /v1/estimates`), on `api.qloudpulse.com`, the host for machines (qloudpulse's ADR 0127).

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::catalogue::Catalogue;
use crate::request::{FORMAT_VERSION, Request, Response};

/// Where the hosted service answers: the host for machines (qloudpulse's ADR 0127). Client 0.1.0 used
/// `https://app.qloudpulse.com/api/report/public/estimates`, which keeps answering.
pub const DEFAULT_ENDPOINT: &str = "https://api.qloudpulse.com/v1/estimates";

const USER_AGENT: &str = concat!("qloudpulse-cli/", env!("CARGO_PKG_VERSION"));

/// An endpoint the client will send a token to: https, or http to this machine for testing.
pub fn check_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.starts_with("https://") && endpoint.len() > "https://".len() {
        return Ok(());
    }
    for local in ["http://localhost", "http://127.0.0.1", "http://[::1]"] {
        if let Some(rest) = endpoint.strip_prefix(local)
            && (rest.is_empty() || rest.starts_with(':') || rest.starts_with('/'))
        {
            return Ok(());
        }
    }
    bail!("the endpoint must start with https:// (http:// only to localhost), so the token is never sent in the clear")
}

/// Where the catalogue of an endpoint is: the endpoint's address and `/catalogue`.
pub fn catalogue_url(endpoint: &str) -> String {
    format!("{}/catalogue", endpoint.trim_end_matches('/'))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// Asks the endpoint what to send for these resource types: it answers with the entries of those it prices. Only the
/// type tokens go, and no token, so a dry run needs none.
pub fn fetch_catalogue(endpoint: &str, types: &[String]) -> Result<Catalogue> {
    check_endpoint(endpoint)?;
    let url = catalogue_url(endpoint);
    let question = serde_json::json!({"formatVersion": FORMAT_VERSION, "types": types});
    let mut answer = agent()
        .post(&url)
        .send_json(&question)
        .with_context(|| format!("could not reach qloudpulse for its catalogue ({url})"))?;
    match answer.status().as_u16() {
        200 => {}
        400 => bail!("qloudpulse refused the catalogue question (400): update the client"),
        404 => bail!("no catalogue at {url} (404): this qloudpulse is older than the client, or the address is wrong"),
        s => bail!("qloudpulse answered {s} for its catalogue ({url})"),
    }
    let body = answer
        .body_mut()
        .read_to_string()
        .context("could not read qloudpulse's catalogue")?;
    Catalogue::parse(&body)
}

/// Sends the request and reads the answer.
pub fn send(endpoint: &str, token: &str, request: &Request) -> Result<Response> {
    check_endpoint(endpoint)?;
    let mut answer = agent()
        .post(endpoint)
        .header("Authorization", &format!("Bearer {token}"))
        .send_json(request)
        .context("could not reach qloudpulse")?;
    let status = answer.status().as_u16();
    if status != 200 {
        let body = answer.body_mut().read_to_string().unwrap_or_default();
        let detail: String = body.chars().take(2000).collect();
        match status {
            400 => bail!("qloudpulse refused the request (400): {detail}"),
            401 => bail!("qloudpulse refused the token (401): it is wrong, rotated or removed"),
            404 => bail!("no estimates endpoint at this address (404)"),
            413 => bail!("the change is too large to send in one request (413)"),
            429 => bail!("too many requests from this token; try again in a minute (429)"),
            s => bail!("qloudpulse answered {s}: {detail}"),
        }
    }
    let response: Response = answer
        .body_mut()
        .read_json()
        .context("qloudpulse's answer is not an estimate")?;
    if response.format_version != FORMAT_VERSION {
        bail!(
            "qloudpulse answered in format {}, and this client reads {FORMAT_VERSION}; update the client",
            response.format_version
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_or_local_http_only() {
        assert!(check_endpoint(DEFAULT_ENDPOINT).is_ok());
        assert!(check_endpoint("http://localhost:8080/api/report/public/estimates").is_ok());
        assert!(check_endpoint("http://127.0.0.1/x").is_ok());
        assert!(check_endpoint("http://[::1]:9000").is_ok());
        assert!(check_endpoint("http://app.qloudpulse.com/api").is_err());
        assert!(check_endpoint("http://localhost.evil.example/").is_err());
        assert!(check_endpoint("https://").is_err());
        assert!(check_endpoint("ftp://x").is_err());
    }

    #[test]
    fn the_catalogue_is_next_to_the_endpoint() {
        assert_eq!(
            catalogue_url(DEFAULT_ENDPOINT),
            "https://api.qloudpulse.com/v1/estimates/catalogue"
        );
        assert_eq!(
            catalogue_url("http://localhost:8080/e/"),
            "http://localhost:8080/e/catalogue"
        );
    }
}

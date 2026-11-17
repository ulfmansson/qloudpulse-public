//! Sends a request to qloudpulse (`POST /api/report/public/estimates`).

use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::request::{FORMAT_VERSION, Request, Response};

/// Where the hosted service answers.
pub const DEFAULT_ENDPOINT: &str = "https://app.qloudpulse.com/api/report/public/estimates";

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

/// Sends the request and reads the answer.
pub fn send(endpoint: &str, token: &str, request: &Request) -> Result<Response> {
    check_endpoint(endpoint)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build()
        .into();
    let mut answer = agent
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
}

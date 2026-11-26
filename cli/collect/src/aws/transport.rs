//! Sending one HTTP request. A trait, so tests answer from fixtures without a network.

use anyhow::Result;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub trait Transport: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse>;
}

/// Why a request got no answer, as one fixed word: `dns`, `timeout`, `connect`, `tls` or `network`. The error's own
/// text stays out of it, so the word can leave in a manifest.
pub fn unanswered_kind(error: &anyhow::Error) -> &'static str {
    use std::io::ErrorKind as K;
    for cause in error.chain() {
        let Some(e) = cause.downcast_ref::<ureq::Error>() else {
            continue;
        };
        return match e {
            ureq::Error::HostNotFound => "dns",
            ureq::Error::Timeout(_) => "timeout",
            ureq::Error::ConnectionFailed => "connect",
            ureq::Error::Tls(_) | ureq::Error::TlsRequired => "tls",
            // The system resolver's failure arrives as a plain I/O error.
            ureq::Error::Io(io) if io.to_string().contains("lookup address") => "dns",
            ureq::Error::Io(io) => match io.kind() {
                K::TimedOut => "timeout",
                K::ConnectionRefused | K::ConnectionReset | K::ConnectionAborted | K::NotConnected => "connect",
                _ => "network",
            },
            _ => "network",
        };
    }
    "network"
}

/// The real one, over HTTPS.
pub struct UreqTransport {
    agent: ureq::Agent,
}

/// An answer larger than this is refused rather than held in memory.
const MAX_BODY: u64 = 256 * 1024 * 1024;

impl UreqTransport {
    pub fn new() -> UreqTransport {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .user_agent(concat!("qloudpulse-collect/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        UreqTransport { agent }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for UreqTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let mut builder = http::Request::builder()
            .method(request.method.as_str())
            .uri(&request.url);
        for (k, v) in &request.headers {
            builder = builder.header(k, v);
        }
        let mut response = self.agent.run(builder.body(request.body.clone())?)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = response.body_mut().with_config().limit(MAX_BODY).read_to_vec()?;
        Ok(HttpResponse { status, headers, body })
    }
}

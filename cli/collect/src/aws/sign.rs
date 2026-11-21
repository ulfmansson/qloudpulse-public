//! SigV4, with AWS's own signing crate.

use super::transport::HttpRequest;
use anyhow::{Context, Result};
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use sha2::{Digest, Sha256};
use std::time::SystemTime;

/// Adds the SigV4 headers to `request`. S3 also gets `x-amz-content-sha256`, which it requires.
pub fn sign_request(
    request: &mut HttpRequest,
    credentials: &Credentials,
    region: &str,
    signing_name: &str,
    time: SystemTime,
) -> Result<()> {
    let mut settings = SigningSettings::default();
    if signing_name == "s3" {
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.uri_path_normalization_mode = aws_sigv4::http_request::UriPathNormalizationMode::Disabled;
        settings.percent_encoding_mode = aws_sigv4::http_request::PercentEncodingMode::Single;
        request.headers.push((
            "x-amz-content-sha256".into(),
            hex::encode(Sha256::digest(&request.body)),
        ));
    }
    let identity = credentials.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(signing_name)
        .time(time)
        .settings(settings)
        .build()
        .context("signing parameters")?
        .into();
    let signable = SignableRequest::new(
        &request.method,
        &request.url,
        request.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        SignableBody::Bytes(&request.body),
    )
    .context("request to sign")?;
    let (instructions, _signature) = sign(signable, &params).context("signing")?.into_parts();
    let (headers, query) = instructions.into_parts();
    debug_assert!(query.is_empty(), "header signing adds no query parameters");
    for header in headers {
        request
            .headers
            .push((header.name().to_string(), header.value().to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_authorization_and_date() {
        let mut request = HttpRequest {
            method: "POST".into(),
            url: "https://ec2.eu-west-1.amazonaws.com/".into(),
            headers: vec![(
                "content-type".into(),
                "application/x-www-form-urlencoded; charset=utf-8".into(),
            )],
            body: b"Action=DescribeRegions&Version=2016-11-15".to_vec(),
        };
        let credentials = Credentials::new("AKIDEXAMPLE", "secret", Some("token".into()), None, "test");
        sign_request(&mut request, &credentials, "eu-west-1", "ec2", SystemTime::UNIX_EPOCH).unwrap();
        let names: Vec<&str> = request.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"authorization"), "{names:?}");
        assert!(names.contains(&"x-amz-date"), "{names:?}");
        assert!(names.contains(&"x-amz-security-token"), "{names:?}");
    }
}

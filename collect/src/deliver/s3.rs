//! `PutObject` into the customer's own bucket, with their own credentials. The only write the client makes to AWS,
//! kept apart from the read-only call plan; its IAM statement names that one bucket.

use crate::aws::protocol::encode_label;
use crate::aws::sign::sign_request;
use crate::aws::{HttpRequest, Transport};
use crate::config::S3Target;
use anyhow::{Result, bail};
use aws_credential_types::Credentials;
use std::time::SystemTime;

pub fn object_key(target: &S3Target, name: &str) -> String {
    format!("{}{name}", target.prefix)
}

pub fn request(target: &S3Target, name: &str, bytes: &[u8]) -> HttpRequest {
    let host = format!("{}.s3.{}.amazonaws.com", target.bucket, target.region);
    let key: Vec<String> = object_key(target, name).split('/').map(encode_label).collect();
    let mut headers = vec![
        ("host".to_string(), host.clone()),
        ("content-type".to_string(), "application/gzip".to_string()),
    ];
    if let Some(kms) = &target.kms_key_id {
        headers.push(("x-amz-server-side-encryption".into(), "aws:kms".into()));
        headers.push(("x-amz-server-side-encryption-aws-kms-key-id".into(), kms.clone()));
    }
    HttpRequest {
        method: "PUT".into(),
        url: format!("https://{host}/{}", key.join("/")),
        headers,
        body: bytes.to_vec(),
    }
}

pub fn put(
    transport: &dyn Transport,
    target: &S3Target,
    credentials: &Credentials,
    name: &str,
    bytes: &[u8],
) -> Result<String> {
    let mut req = request(target, name, bytes);
    sign_request(&mut req, credentials, &target.region, "s3", SystemTime::now())?;
    let response = transport.send(&req)?;
    if response.status != 200 {
        let detail: String = String::from_utf8_lossy(&response.body).chars().take(300).collect();
        bail!("S3 answered {}: {detail}", response.status);
    }
    Ok(format!("s3://{}/{}", target.bucket, object_key(target, name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn puts_under_the_prefix_with_kms() {
        let target = S3Target {
            bucket: "acme-archive".into(),
            prefix: "qloudpulse/".into(),
            region: "eu-west-1".into(),
            kms_key_id: Some("alias/k".into()),
        };
        let req = request(&target, "qloudpulse-main-20261121T0600Z.tgz", b"x");
        assert_eq!(
            req.url,
            "https://acme-archive.s3.eu-west-1.amazonaws.com/qloudpulse/qloudpulse-main-20261121T0600Z.tgz"
        );
        assert!(
            req.headers
                .contains(&("x-amz-server-side-encryption".into(), "aws:kms".into()))
        );
    }
}

//! Credentials from the standard AWS chain (environment, profiles, SSO, instance and container roles, web identity),
//! and roles assumed from them in other accounts. This is AWS's own `aws-config`; nothing here reaches qloudpulse.

use anyhow::{Context, Result};
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;

pub struct CredentialSource {
    runtime: tokio::runtime::Runtime,
    config: SdkConfig,
}

impl CredentialSource {
    /// `profile`: an AWS profile name, or the chain's default. `region`: where STS is called.
    pub fn new(profile: Option<&str>, region: &str) -> Result<CredentialSource> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("async runtime")?;
        let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region.to_string()));
        if let Some(profile) = profile {
            loader = loader.profile_name(profile);
        }
        let config = runtime.block_on(loader.load());
        Ok(CredentialSource { runtime, config })
    }

    /// The chain's own credentials.
    pub fn ambient(&self) -> Result<Credentials> {
        let provider = self
            .config
            .credentials_provider()
            .context("no AWS credentials found in the standard chain")?;
        self.runtime
            .block_on(provider.provide_credentials())
            .context("reading AWS credentials")
    }

    /// Credentials for `role_arn`, assumed with the chain's credentials. The session name shows in CloudTrail.
    pub fn assume(&self, role_arn: &str, external_id: Option<&str>) -> Result<Credentials> {
        let mut builder = aws_config::sts::AssumeRoleProvider::builder(role_arn)
            .session_name("qloudpulse-collect")
            .configure(&self.config);
        if let Some(id) = external_id {
            builder = builder.external_id(id);
        }
        let provider = self.runtime.block_on(builder.build());
        self.runtime
            .block_on(provider.provide_credentials())
            .with_context(|| format!("assuming {role_arn}"))
    }
}

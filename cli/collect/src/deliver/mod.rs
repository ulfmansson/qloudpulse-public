//! Delivering finished bundles: to the customer's own S3 bucket, to qloudpulse (feature `upload`), or to a command.
//! Raw snapshots are never delivered. Which bundle reached which target is kept in `<output_dir>/.delivered.json`,
//! so a failed delivery is retried at the next run, and retention never deletes a bundle a target still lacks.

#[cfg(feature = "upload")]
pub mod qloudpulse;
pub mod s3;

use crate::aws::Transport;
use crate::aws::credentials::CredentialSource;
use crate::config::Config;
use crate::log;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const STATE: &str = ".delivered.json";

/// The targets the config turns on, by name.
pub fn targets(config: &Config) -> Vec<&'static str> {
    let d = &config.deliver;
    let mut out = Vec::new();
    if d.s3.is_some() {
        out.push("s3");
    }
    if d.qloudpulse.is_some() {
        out.push("qloudpulse");
    }
    if d.command.is_some() {
        out.push("command");
    }
    out
}

type State = BTreeMap<String, BTreeSet<String>>;

fn load_state(output_dir: &Path) -> State {
    std::fs::read(output_dir.join(STATE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state(output_dir: &Path, state: &State) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;
    std::fs::write(output_dir.join(STATE), serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

pub struct Deliverer<'a> {
    pub config: &'a Config,
    pub transport: &'a dyn Transport,
    /// For S3; created on first use, since only S3 needs AWS credentials.
    pub credentials: Option<&'a CredentialSource>,
}

impl Deliverer<'_> {
    /// Sends `bundle` to each of `only` (or every configured target) that does not have it yet.
    pub fn deliver(&self, bundle: &Path, only: Option<&[String]>) -> Result<()> {
        let name = bundle
            .file_name()
            .context("a bundle path ends in a file name")?
            .to_string_lossy()
            .into_owned();
        if !name.ends_with(".tgz") {
            bail!("{} is not a bundle (.tgz)", bundle.display());
        }
        let mut state = load_state(&self.config.output_dir);
        let mut failed = Vec::new();
        for target in targets(self.config) {
            if only.is_some_and(|o| !o.iter().any(|t| t == target)) {
                continue;
            }
            if state.get(&name).is_some_and(|d| d.contains(target)) {
                continue;
            }
            match self.send(target, bundle, &name) {
                Ok(detail) => {
                    log::info(
                        "delivered",
                        &[("bundle", name.clone()), ("target", target.into()), ("detail", detail)],
                    );
                    state.entry(name.clone()).or_default().insert(target.to_string());
                    save_state(&self.config.output_dir, &state)?;
                }
                Err(e) => {
                    log::warn(
                        "delivery failed",
                        &[
                            ("bundle", name.clone()),
                            ("target", target.into()),
                            ("error", format!("{e:#}")),
                        ],
                    );
                    failed.push(target);
                }
            }
        }
        if !failed.is_empty() {
            bail!(
                "{name} was not delivered to {}; the next run retries",
                failed.join(", ")
            );
        }
        Ok(())
    }

    fn send(&self, target: &str, bundle: &Path, name: &str) -> Result<String> {
        // With [encrypt], what leaves is the JWE written next to the bundle, never the plaintext.
        let (bundle, name) = if self.config.encrypt.is_some() {
            let jwe = crate::encrypt::jwe_path(bundle);
            if !jwe.exists() {
                bail!(
                    "{} is not encrypted yet; pack it again with [encrypt] set",
                    bundle.display()
                );
            }
            (jwe, format!("{name}.jwe"))
        } else {
            (bundle.to_path_buf(), name.to_string())
        };
        let (bundle, name) = (bundle.as_path(), name.as_str());
        let bytes = std::fs::read(bundle).with_context(|| format!("reading {}", bundle.display()))?;
        match target {
            "s3" => {
                let s3 = self
                    .config
                    .deliver
                    .s3
                    .as_ref()
                    .expect("target listed only when configured");
                let credentials = self
                    .credentials
                    .context("S3 delivery needs AWS credentials")?
                    .ambient()?;
                s3::put(self.transport, s3, &credentials, name, &bytes)
            }
            "qloudpulse" => {
                #[cfg(feature = "upload")]
                {
                    let target = self
                        .config
                        .deliver
                        .qloudpulse
                        .as_ref()
                        .expect("target listed only when configured");
                    qloudpulse::upload(self.transport, target, name, &bytes)
                }
                #[cfg(not(feature = "upload"))]
                bail!("this build has no qloudpulse upload (built without the `upload` feature)")
            }
            "command" => {
                let command = self
                    .config
                    .deliver
                    .command
                    .as_ref()
                    .expect("target listed only when configured");
                run_command(&command.argv, bundle)
            }
            other => bail!("unknown delivery target {other}"),
        }
    }

    /// Bundles in `output_dir` some configured target still lacks, oldest first.
    pub fn pending(&self) -> Result<Vec<PathBuf>> {
        let wanted = targets(self.config);
        let state = load_state(&self.config.output_dir);
        Ok(bundles(&self.config.output_dir)?
            .into_iter()
            .filter(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                wanted.iter().any(|t| !state.get(&name).is_some_and(|d| d.contains(*t)))
            })
            .collect())
    }

    /// Keeps the newest `keep_bundles`; never deletes a bundle a target still lacks.
    pub fn prune(&self) -> Result<usize> {
        let pending: BTreeSet<PathBuf> = self.pending()?.into_iter().collect();
        let all = bundles(&self.config.output_dir)?;
        let excess = all.len().saturating_sub(self.config.run.keep_bundles);
        let mut state = load_state(&self.config.output_dir);
        let mut removed = 0;
        for old in all.iter().take(excess).filter(|p| !pending.contains(*p)) {
            std::fs::remove_file(old)?;
            let _ = std::fs::remove_file(crate::encrypt::jwe_path(old));
            if let Some(name) = old.file_name() {
                state.remove(&*name.to_string_lossy());
            }
            removed += 1;
        }
        save_state(&self.config.output_dir, &state)?;
        Ok(removed)
    }
}

/// `.tgz` files in `dir`, oldest first (names sort by window).
pub fn bundles(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "tgz"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.into()),
    };
    out.sort();
    Ok(out)
}

fn run_command(argv: &[String], bundle: &Path) -> Result<String> {
    let path = bundle.to_string_lossy();
    let args: Vec<String> = argv.iter().map(|a| a.replace("{bundle}", &path)).collect();
    let status = std::process::Command::new(&args[0])
        .args(&args[1..])
        .status()
        .with_context(|| format!("running {}", args[0]))?;
    if !status.success() {
        bail!("{} exited with {status}", args[0]);
    }
    Ok(format!("{} exited 0", args[0]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::{HttpRequest, HttpResponse};

    struct Never;
    impl Transport for Never {
        fn send(&self, _: &HttpRequest) -> Result<HttpResponse> {
            bail!("no network in this test")
        }
    }

    #[cfg(unix)]
    #[test]
    fn command_delivery_is_remembered_and_retention_keeps_undelivered() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        for h in ["T0100Z", "T0200Z", "T0300Z"] {
            std::fs::write(out.join(format!("qloudpulse-main-20261121{h}.tgz")), b"x").unwrap();
        }
        let text = format!(
            "source_id = \"main\"\noutput_dir = \"{}\"\n[run]\nkeep_bundles = 1\n[deliver.command]\nargv = [\"true\", \"{{bundle}}\"]\n",
            out.display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        let deliverer = Deliverer {
            config: &config,
            transport: &Never,
            credentials: None,
        };
        assert_eq!(deliverer.pending().unwrap().len(), 3);
        deliverer
            .deliver(&out.join("qloudpulse-main-20261121T0100Z.tgz"), None)
            .unwrap();
        assert_eq!(deliverer.pending().unwrap().len(), 2);
        assert_eq!(deliverer.prune().unwrap(), 1, "only the delivered old bundle goes");
        assert_eq!(bundles(&out).unwrap().len(), 2);
    }
}

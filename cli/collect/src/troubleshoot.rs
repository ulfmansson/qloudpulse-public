//! `troubleshoot`: checks, one by one, what a run needs, and says for each that fails what to do about it.
//!
//! It reads the config and the key, asks AWS who the credentials are and which regions are on, looks every region's
//! endpoint up, and reads what the newest bundle says stopped. Nothing is collected and nothing is written. It
//! contacts qloudpulse only when asked to (`--qloudpulse`) or when `[deliver.qloudpulse]` is configured, and then
//! sends an empty request with the token: no bundle, no data.

use crate::aws::caller::{Account, AwsError, Unanswered};
use crate::aws::credentials::CredentialSource;
use crate::aws::{Caller, Transport};
use crate::callplan::CallPlan;
use crate::collect;
use crate::config::Config;
use crate::pseudonym::Pseudonymizer;
use aws_credential_types::Credentials;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ok,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub state: State,
    pub detail: String,
    /// What to do about it, when it is not ok.
    pub fix: Option<String>,
}

impl Check {
    fn new(name: &'static str, state: State, detail: impl Into<String>) -> Check {
        Check {
            name,
            state,
            detail: detail.into(),
            fix: None,
        }
    }

    fn fix(mut self, fix: impl Into<String>) -> Check {
        self.fix = Some(fix.into());
        self
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.state == State::Fail)
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            let state = match c.state {
                State::Ok => "ok",
                State::Warn => "warn",
                State::Fail => "FAIL",
                State::Skipped => "skip",
            };
            out.push_str(&format!("{state:<5} {:<12} {}\n", c.name, c.detail));
            if let Some(fix) = &c.fix {
                out.push_str(&format!("{:<18} what to do: {fix}\n", ""));
            }
        }
        let count = |s| self.checks.iter().filter(|c| c.state == s).count();
        out.push_str(&match (count(State::Fail), count(State::Warn)) {
            (0, 0) => "\nnothing wrong found\n".to_string(),
            (fails, warns) => format!("\n{fails} failed, {warns} to look at\n"),
        });
        out
    }
}

/// Looks a host name up; an error says why it did not resolve.
pub type Resolve<'a> = &'a dyn Fn(&str) -> std::io::Result<()>;

/// The system resolver, as the collector's requests use it.
pub fn system_resolve(host: &str) -> std::io::Result<()> {
    use std::net::ToSocketAddrs as _;
    (host, 443).to_socket_addrs().map(|_| ())
}

pub struct Options<'a> {
    pub config_path: &'a Path,
    /// Also check qloudpulse, at this endpoint when given.
    pub qloudpulse: bool,
    pub endpoint: Option<String>,
}

/// Every check, with this machine's real credentials and resolver.
pub fn run(options: &Options, transport: &dyn Transport) -> Report {
    let mut report = Report::default();
    let config = match Config::load(options.config_path) {
        Ok((config, _)) => {
            report.checks.push(Check::new(
                "config",
                State::Ok,
                format!("{}, source {}", options.config_path.display(), config.source_id),
            ));
            config
        }
        Err(e) => {
            report
                .checks
                .push(Check::new("config", State::Fail, format!("{e:#}")).fix(format!(
                    "`qloudpulse-collect init --config {}` writes a starter; an error naming a line is in that line.",
                    options.config_path.display()
                )));
            for name in ["key", "credentials", "network", "last bundle"] {
                report.checks.push(Check::new(name, State::Skipped, "needs the config"));
            }
            report.checks.extend(qloudpulse_checks(options, None, transport));
            return report;
        }
    };
    report.checks.push(key(&config));

    match CredentialSource::new(config.profile.as_deref(), &config.home_region) {
        Ok(source) => match source.ambient() {
            Ok(credentials) => {
                let mut caller = Caller::new(CallPlan::embedded(), transport, config.options());
                caller.endpoint = std::env::var("AWS_ENDPOINT_URL").ok().filter(|e| !e.trim().is_empty());
                // A look, not a run: do not wait a minute and a half on a network that is gone.
                caller.backoff = Duration::from_millis(100);
                let (mut checks, account) = identity(&caller, &credentials, &config);
                for a in &config.accounts {
                    checks.push(match source.assume(&a.role_arn, a.external_id.as_deref()) {
                        Ok(_) => Check::new("role", State::Ok, format!("{} can be assumed", a.role_arn)),
                        Err(e) => Check::new("role", State::Fail, format!("{}: {e:#}", a.id)).fix(format!(
                            "the role's trust policy must let these credentials assume {}{}.",
                            a.role_arn,
                            if a.external_id.is_some() {
                                ", with the external_id in the config"
                            } else {
                                ""
                            }
                        )),
                    });
                }
                report.checks.extend(checks);
                report.checks.push(match account {
                    Some(account) => network(&caller, &account, &config, &system_resolve),
                    None => Check::new("network", State::Skipped, "needs credentials AWS accepts"),
                });
            }
            Err(e) => {
                report.checks.push(no_credentials(&e, &config));
                report
                    .checks
                    .push(Check::new("network", State::Skipped, "needs credentials"));
            }
        },
        Err(e) => {
            report.checks.push(no_credentials(&e, &config));
            report
                .checks
                .push(Check::new("network", State::Skipped, "needs credentials"));
        }
    }
    report.checks.push(last_bundle(&config));
    report
        .checks
        .extend(qloudpulse_checks(options, Some(&config), transport));
    report
}

fn no_credentials(error: &anyhow::Error, config: &Config) -> Check {
    Check::new("credentials", State::Fail, format!("{error:#}")).fix(match &config.profile {
        Some(profile) => format!(
            "the config names the AWS profile {profile}; sign in to it (`aws sso login --profile {profile}`) or \
             correct `profile`."
        ),
        None => "set AWS_PROFILE, or `profile` in the config, to a profile that is signed in (`aws sso login`), or \
                 run where a role is attached."
            .to_string(),
    })
}

/// The pseudonym key: there, and long enough.
pub fn key(config: &Config) -> Check {
    match Pseudonymizer::load(&config.key_file, false) {
        Ok(p) => Check::new(
            "key",
            State::Ok,
            format!("{}, fingerprint {}", config.key_file.display(), p.fingerprint()),
        ),
        Err(e) => Check::new("key", State::Fail, format!("{e:#}")).fix(
            "`qloudpulse-collect init` creates it. If bundles were already delivered with another key, restore that \
             key instead: a new one changes every pseudonym.",
        ),
    }
}

/// Whose credentials these are, by asking AWS. The account they belong to when AWS answers.
pub fn identity(caller: &Caller, credentials: &Credentials, config: &Config) -> (Vec<Check>, Option<Account>) {
    match collect::caller_account(caller, credentials, &config.home_region) {
        Ok(id) => (
            vec![Check::new(
                "credentials",
                State::Ok,
                format!("AWS accepts them; account {id}"),
            )],
            Some(Account {
                id,
                credentials: credentials.clone(),
            }),
        ),
        Err(e) => (vec![aws_failure("credentials", &e, &config.home_region)], None),
    }
}

/// A failed AWS call as a check: what went wrong and what to do, by the kind of error.
fn aws_failure(name: &'static str, error: &anyhow::Error, region: &str) -> Check {
    let check = Check::new(name, State::Fail, format!("{error:#}"));
    if let Some(u) = error.chain().find_map(|c| c.downcast_ref::<Unanswered>()) {
        return check.fix(match u.kind {
            "dns" => format!(
                "{} does not resolve from here. Check the network, VPN and DNS of this machine; in a private \
                 network, that its resolver answers for amazonaws.com.",
                u.host
            ),
            "tls" => format!(
                "the connection to {} is not AWS's own: a proxy that opens TLS is in the way, or the clock is wrong.",
                u.host
            ),
            _ => format!(
                "{} is not reachable on port 443 from here. Check the network, VPN, proxy and firewall.",
                u.host
            ),
        });
    }
    match error.chain().find_map(|c| c.downcast_ref::<AwsError>()) {
        Some(aws) if aws.status == 403 || aws.code.contains("AccessDenied") || aws.code.contains("Unauthorized") => {
            let expired = aws.code.contains("Expired") || aws.code.contains("InvalidClientTokenId");
            check.fix(if expired {
                "the credentials are expired or not valid; sign in again (`aws sso login`).".to_string()
            } else if aws.code == "SignatureDoesNotMatch" {
                "the secret key is wrong, or this machine's clock is off by minutes.".to_string()
            } else {
                format!(
                    "the credentials may not do this in {region}. `qloudpulse-collect policy` prints the read-only \
                     policy the client needs."
                )
            })
        }
        _ => check,
    }
}

/// The regions a run would read, and whether each one's endpoint resolves.
pub fn network(caller: &Caller, account: &Account, config: &Config, resolve: Resolve) -> Check {
    let regions = match &config.regions {
        Some(regions) => regions.clone(),
        None => match collect::enabled_regions(caller, account, &config.home_region) {
            Ok(regions) => regions,
            Err(e) => return aws_failure("network", &e, &config.home_region),
        },
    };
    let lost: Vec<String> = regions
        .iter()
        .map(|r| format!("ec2.{r}.amazonaws.com"))
        .filter_map(|host| resolve(&host).err().map(|e| format!("{host} ({e})")))
        .collect();
    if lost.is_empty() {
        return Check::new(
            "network",
            State::Ok,
            format!("{} regions, every endpoint resolves", regions.len()),
        );
    }
    Check::new(
        "network",
        State::Fail,
        format!(
            "{} of {} regions do not resolve: {}",
            lost.len(),
            regions.len(),
            lost.join(", ")
        ),
    )
    .fix(
        "check this machine's network, VPN and DNS. A name that fails once and resolves the next time is a resolver \
         dropping lookups; `collect` tries each request for about a minute and a half before it gives a region up. \
         To leave a region out, list the ones to read in `regions`.",
    )
}

/// What the newest bundle says stopped early.
pub fn last_bundle(config: &Config) -> Check {
    let newest = match crate::deliver::bundles(&config.output_dir) {
        Ok(mut all) => all.pop(),
        Err(e) => return Check::new("last bundle", State::Warn, format!("{e:#}")),
    };
    let Some(path) = newest else {
        return Check::new(
            "last bundle",
            State::Skipped,
            format!("none in {} yet", config.output_dir.display()),
        );
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match crate::bundle::open(&path) {
        Err(e) => Check::new("last bundle", State::Fail, format!("{name}: {e:#}"))
            .fix("the file does not match its manifest; `pack` the snapshot again."),
        Ok(opened) if opened.manifest.failures.is_empty() => Check::new(
            "last bundle",
            State::Ok,
            format!("{name}: {} records, no collector stopped", opened.records.len()),
        ),
        Ok(opened) => Check::new(
            "last bundle",
            State::Warn,
            format!(
                "{name}: {} collector(s) stopped: {}",
                opened.manifest.failures.len(),
                opened.manifest.failures.join("; ")
            ),
        )
        .fix(
            "what they read is missing from that bundle. `aws_error 403` is a permission (`qloudpulse-collect \
             policy`); `dns`, `connect` and `timeout` are this machine's network. The full error is in that run's \
             log; `collect` again reads the hour again.",
        ),
    }
}

#[cfg(not(feature = "upload"))]
fn qloudpulse_checks(_: &Options, _: Option<&Config>, _: &dyn Transport) -> Vec<Check> {
    vec![Check::new(
        "qloudpulse",
        State::Skipped,
        "this build has no upload and cannot contact qloudpulse",
    )]
}

#[cfg(feature = "upload")]
fn qloudpulse_checks(options: &Options, config: Option<&Config>, transport: &dyn Transport) -> Vec<Check> {
    use crate::deliver::qloudpulse::TOKEN_ENV;
    use qloudpulse_stack::{credentials, login};

    let configured = config.and_then(|c| c.deliver.qloudpulse.as_ref());
    if !options.qloudpulse && configured.is_none() {
        return vec![Check::new(
            "qloudpulse",
            State::Skipped,
            "not contacted: no [deliver.qloudpulse] in the config. `troubleshoot --qloudpulse` checks the upload.",
        )];
    }
    let endpoint = options
        .endpoint
        .clone()
        .or(configured.map(|t| t.endpoint.clone()))
        .unwrap_or(crate::config::QLOUDPULSE_ENDPOINT.into());
    let origin = match login::origin(&endpoint) {
        Ok(origin) => origin,
        Err(e) => {
            return vec![
                Check::new("qloudpulse", State::Fail, format!("{endpoint}: {e:#}"))
                    .fix("the endpoint is https://, such as https://api.qloudpulse.com."),
            ];
        }
    };
    // As `upload` finds it, but never asking: a check does not start a sign-in.
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| credentials::system().load(&origin, credentials::now()).map(|s| s.token));
    vec![qloudpulse(transport, &endpoint, token.as_deref())]
}

/// Whether qloudpulse at `endpoint` knows the token and takes bundles, by sending it an empty request: a bundle of
/// no bytes, which it can only refuse. How it refuses is the answer.
#[cfg(feature = "upload")]
pub fn qloudpulse(transport: &dyn Transport, endpoint: &str, token: Option<&str>) -> Check {
    use crate::config::QloudpulseTarget;
    use crate::deliver::qloudpulse::request;
    use qloudpulse_stack::send;

    let Some(token) = token else {
        return Check::new("qloudpulse", State::Fail, format!("no token for {endpoint}"))
            .fix(send::missing_token(endpoint).replace("qloudpulse-stack login", "qloudpulse-collect login"));
    };
    let target = QloudpulseTarget {
        endpoint: endpoint.to_string(),
    };
    let response = match request(&target, token, "troubleshoot.tgz", b"").and_then(|r| transport.send(&r)) {
        Ok(response) => response,
        Err(e) => {
            return Check::new("qloudpulse", State::Fail, format!("no answer from {endpoint}: {e:#}"))
                .fix("check the endpoint's spelling, and that this machine's network, proxy and firewall let it out.");
        }
    };
    let body = String::from_utf8_lossy(&response.body);
    let problem = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default();
    let kind = problem.get("type").and_then(|t| t.as_str()).unwrap_or_default();
    let said = problem
        .get("title")
        .and_then(|t| t.as_str())
        .map(qloudpulse_stack::login::plain)
        .unwrap_or_default();
    let answered = format!("{endpoint} answered {} {said}", response.status);
    let answered = answered.trim_end();
    match response.status {
        401 | 403 => Check::new(
            "qloudpulse",
            State::Fail,
            format!("{answered}: it does not know the token"),
        )
        .fix(send::missing_token(endpoint).replace("qloudpulse-stack login", "qloudpulse-collect login")),
        404 if kind.ends_with("not-taking-bundles") => Check::new(
            "qloudpulse",
            State::Fail,
            format!("{answered}: it knows the token, and does not take offline bundles"),
        )
        .fix(
            "offline collection is not turned on at this qloudpulse, or not for your company. Ask qloudpulse to turn \
             it on; bundles already packed stay in the output directory and `deliver` sends them then. Another \
             qloudpulse is named with --endpoint or [deliver.qloudpulse] endpoint.",
        ),
        404 => Check::new(
            "qloudpulse",
            State::Fail,
            format!("{answered}: nothing takes bundles at this address"),
        )
        .fix("the endpoint is the API's own address, such as https://api.qloudpulse.com, with no path."),
        // It read the empty bundle and refused it, as it must: bundles are taken here.
        400 | 409 | 411 | 413 | 415 | 422 => Check::new(
            "qloudpulse",
            State::Ok,
            format!("{endpoint} knows the token and takes bundles"),
        ),
        500.. => Check::new("qloudpulse", State::Warn, format!("{answered}: it failed on its side"))
            .fix("try again in a while."),
        _ => Check::new(
            "qloudpulse",
            State::Warn,
            format!("{answered}: reachable, but not an answer this client knows"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::{HttpRequest, HttpResponse};
    use anyhow::Result;
    use std::sync::Mutex;

    /// Answers every request with one status and body, and keeps what it was sent.
    struct Answer(u16, &'static str, Mutex<Vec<HttpRequest>>);

    impl Answer {
        fn new(status: u16, body: &'static str) -> Answer {
            Answer(status, body, Mutex::new(vec![]))
        }
    }

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

    struct Gone;
    impl Transport for Gone {
        fn send(&self, _: &HttpRequest) -> Result<HttpResponse> {
            Err(ureq::Error::Io(std::io::Error::other("failed to lookup address information")).into())
        }
    }

    fn config(extra: &str) -> Config {
        toml::from_str(&format!("source_id = \"main\"\n{extra}")).unwrap()
    }

    fn credentials() -> Credentials {
        Credentials::new("AKID", "secret", None, None, "test")
    }

    fn caller(transport: &dyn Transport) -> Caller<'_> {
        let mut caller = Caller::new(CallPlan::embedded(), transport, vec![]);
        caller.backoff = Duration::ZERO;
        caller
    }

    #[test]
    fn says_whose_credentials_they_are() {
        let sts = Answer::new(
            200,
            "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>111122223333</Account>\
             </GetCallerIdentityResult></GetCallerIdentityResponse>",
        );
        let (checks, account) = identity(&caller(&sts), &credentials(), &config(""));
        assert_eq!(checks[0].state, State::Ok);
        assert!(checks[0].detail.contains("111122223333"));
        assert_eq!(account.unwrap().id, "111122223333");
    }

    #[test]
    fn a_name_that_does_not_resolve_is_a_network_fix_and_a_refusal_a_policy_fix() {
        let (checks, account) = identity(&caller(&Gone), &credentials(), &config(""));
        assert!(account.is_none());
        assert_eq!(checks[0].state, State::Fail);
        let fix = checks[0].fix.as_deref().unwrap();
        assert!(fix.contains("sts.eu-west-1.amazonaws.com does not resolve"), "{fix}");

        let denied = Answer::new(
            403,
            "<ErrorResponse><Error><Code>AccessDenied</Code></Error></ErrorResponse>",
        );
        let (checks, _) = identity(&caller(&denied), &credentials(), &config(""));
        assert!(checks[0].fix.as_deref().unwrap().contains("qloudpulse-collect policy"));

        let expired = Answer::new(
            403,
            "<ErrorResponse><Error><Code>ExpiredToken</Code></Error></ErrorResponse>",
        );
        let (checks, _) = identity(&caller(&expired), &credentials(), &config(""));
        assert!(checks[0].fix.as_deref().unwrap().contains("sign in again"));
    }

    #[test]
    fn names_the_regions_that_do_not_resolve() {
        let none = Answer::new(200, "<R/>");
        let account = Account {
            id: "111122223333".into(),
            credentials: credentials(),
        };
        let config = config("regions = [\"eu-west-1\", \"ap-southeast-1\"]");
        let all = network(&caller(&none), &account, &config, &|_| Ok(()));
        assert_eq!(all.state, State::Ok);
        assert!(all.detail.starts_with("2 regions"));

        let one = network(&caller(&none), &account, &config, &|host| {
            if host.contains("ap-southeast-1") {
                Err(std::io::Error::other("nodename nor servname provided"))
            } else {
                Ok(())
            }
        });
        assert_eq!(one.state, State::Fail);
        assert!(
            one.detail
                .starts_with("1 of 2 regions do not resolve: ec2.ap-southeast-1.amazonaws.com"),
            "{}",
            one.detail
        );
        assert!(
            none.2.lock().unwrap().is_empty(),
            "configured regions are not asked of AWS"
        );
    }

    #[test]
    fn a_missing_key_says_how_to_make_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config("");
        config.key_file = dir.path().join("absent.key");
        let check = key(&config);
        assert_eq!(check.state, State::Fail);
        assert!(check.fix.unwrap().contains("init"));
        Pseudonymizer::create_key(&config.key_file).unwrap();
        assert_eq!(key(&config).state, State::Ok);
    }

    #[test]
    fn no_bundle_yet_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config("");
        config.output_dir = dir.path().join("bundles");
        assert_eq!(last_bundle(&config).state, State::Skipped);
    }

    #[cfg(feature = "upload")]
    #[test]
    fn reads_qloudpulses_answer_to_an_empty_bundle() {
        const TOKEN: &str = "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
        const AT: &str = "https://api.example.test";
        let not_taking = Answer::new(
            404,
            r#"{"type":"https://qloudpulse.com/api-reference.html#problem-not-taking-bundles","title":"Not taking bundles","status":404}"#,
        );
        let check = qloudpulse(&not_taking, AT, Some(TOKEN));
        assert_eq!(check.state, State::Fail);
        assert!(
            check.detail.contains("does not take offline bundles"),
            "{}",
            check.detail
        );
        assert!(!check.detail.contains(TOKEN));
        let sent = not_taking.2.lock().unwrap();
        assert_eq!(sent[0].url, "https://api.example.test/v1/collection-bundles");
        assert!(sent[0].body.is_empty(), "no bundle and no data is sent");

        let refused = qloudpulse(&Answer::new(401, "{}"), AT, Some(TOKEN));
        assert!(refused.detail.contains("does not know the token"));
        assert!(refused.fix.unwrap().contains("qloudpulse-collect login"));

        assert_eq!(qloudpulse(&Answer::new(422, "{}"), AT, Some(TOKEN)).state, State::Ok);
        assert_eq!(qloudpulse(&Answer::new(503, ""), AT, Some(TOKEN)).state, State::Warn);
        assert_eq!(qloudpulse(&Gone, AT, Some(TOKEN)).state, State::Fail);

        let none = Answer::new(200, "");
        assert_eq!(qloudpulse(&none, AT, None).state, State::Fail);
        assert!(none.2.lock().unwrap().is_empty(), "without a token nothing is sent");
    }

    #[test]
    fn the_report_fails_when_a_check_does_and_says_what_to_do() {
        let report = Report {
            checks: vec![
                Check::new("config", State::Ok, "there"),
                Check::new("key", State::Fail, "absent").fix("make one"),
            ],
        };
        assert!(report.failed());
        let text = report.text();
        assert!(text.contains("ok    config       there"), "{text}");
        assert!(text.contains("FAIL  key          absent"));
        assert!(text.contains("what to do: make one"));
        assert!(text.ends_with("1 failed, 0 to look at\n"));
    }
}

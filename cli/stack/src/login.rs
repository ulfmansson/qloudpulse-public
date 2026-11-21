//! `qloudpulse-stack login`: signs the client in with the browser, as OAuth's device authorization grant (RFC 8628) does
//! (qloudpulse's ADR 0135). The client asks qloudpulse for a code, shows it, opens the browser where a member approves
//! it, and asks every few seconds until qloudpulse hands it a personal token. The client never listens on a port, so
//! it works over SSH, and the code can be approved on another device.
//!
//! The calls carry no token. On hosted qloudpulse they go to `app.qloudpulse.com`, as `api.qloudpulse.com` answers only
//! `/v1` (ADR 0127); on an install of its own, to the endpoint's own address.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::send::{check_endpoint, is_hosted};
use crate::token::{Terminal, looks_like_token};

/// Where hosted qloudpulse starts a login and shows its page.
pub const HOSTED: &str = "https://app.qloudpulse.com";
/// The path that starts a login.
pub const START: &str = "/api/report/public/cli-login";
/// The path the client asks until the login is decided.
pub const TOKEN: &str = "/api/report/public/cli-login/token";
/// The page where a member approves a login.
pub const PAGE: &str = "/ui/?cli-login";
/// Where a member removes their tokens.
pub const TOKENS_PAGE: &str = "/ui/?profile#my-tokens";

/// The address of the qloudpulse behind an endpoint, which starts its logins and keys the token kept for it:
/// `https://app.qloudpulse.com` for hosted qloudpulse, else the endpoint's scheme and host, such as
/// `https://qloudpulse.example.com`.
pub fn origin(endpoint: &str) -> Result<String> {
    check_endpoint(endpoint)?;
    if is_hosted(endpoint) {
        return Ok(HOSTED.to_string());
    }
    let (scheme, rest) = endpoint.split_once("://").context("the endpoint has no scheme")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if authority.is_empty() {
        bail!("the endpoint has no host");
    }
    Ok(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

/// Where a member removes the tokens of the qloudpulse at this address.
pub fn tokens_page(origin: &str) -> String {
    format!("{origin}{TOKENS_PAGE}")
}

/// What the client says it is, for the approval page and the token's label: its version and system, never the
/// computer's name or the user's, such as `qloudpulse 0.4.0 on macOS arm64`.
pub fn client() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    };
    format!("qloudpulse-stack {} on {os} {arch}", env!("CARGO_PKG_VERSION"))
}

/// One answer: its status and its body.
pub struct Answer {
    pub status: u16,
    pub body: String,
}

/// The HTTP the login speaks: a JSON body posted, the answer read whatever its status. An `Err` is no answer at all.
pub trait Http {
    fn post(&mut self, url: &str, body: &Value) -> Result<Answer>;
}

/// HTTP over the network.
pub struct Network;

impl Http for Network {
    fn post(&mut self, url: &str, body: &Value) -> Result<Answer> {
        let mut answer = crate::send::agent()
            .post(url)
            .send_json(body)
            .with_context(|| format!("could not reach qloudpulse ({url})"))?;
        let status = answer.status().as_u16();
        let body = answer.body_mut().read_to_string().unwrap_or_default();
        Ok(Answer { status, body })
    }
}

/// The passing of time while the login waits.
pub trait Clock {
    /// Seconds since some fixed moment.
    fn now(&self) -> u64;
    fn sleep(&mut self, seconds: u64);
}

/// The machine's clock.
pub struct SystemClock(std::time::Instant);

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock(std::time::Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        self.0.elapsed().as_secs()
    }
    fn sleep(&mut self, seconds: u64) {
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }
}

/// What opens a page in the browser. False when it could not.
pub trait Browser {
    fn open(&mut self, url: &str) -> bool;
}

/// The system's own way to open a page: `open` on macOS, Explorer on Windows, `xdg-open` on Linux with a display.
pub struct SystemBrowser;

impl Browser for SystemBrowser {
    fn open(&mut self, url: &str) -> bool {
        use std::process::{Command, Stdio};
        let mut command = if cfg!(target_os = "macos") {
            Command::new("open")
        } else if cfg!(windows) {
            // Explorer passes the address to the default browser as it is: `cmd /c start` would split it at its `&`.
            Command::new("explorer.exe")
        } else {
            if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
                return false;
            }
            Command::new("xdg-open")
        };
        let status = command
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // Explorer's exit code says nothing about whether the page opened.
        status.is_ok_and(|s| cfg!(windows) || s.success())
    }
}

/// qloudpulse's answer to a login's start.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Started {
    device_code: String,
    user_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
}

/// A problem (RFC 9457), with RFC 8628's `error` while a login waits.
#[derive(Debug, Default, Deserialize)]
struct Problem {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

/// The token a login is answered with.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedIn {
    pub token: String,
    pub company: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Text from qloudpulse for the terminal: one line, no control characters, and not too long.
pub fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(300)
        .collect::<String>()
        .trim()
        .to_string()
}

fn detail(body: &str) -> String {
    let problem: Problem = serde_json::from_str(body).unwrap_or_default();
    problem.detail.map_or_else(|| plain(body), |d| plain(&d))
}

/// A user code the page can be opened with: letters, digits and dashes, as qloudpulse makes them (`WDJB-MJHT`).
fn is_user_code(code: &str) -> bool {
    (4..=20).contains(&code.len()) && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The page to approve the login on: qloudpulse's, if it is an address the client would open, else the endpoint's
/// own `/ui/?cli-login`.
fn page(origin: &str, started: &Started) -> (String, String) {
    let safe = |url: &Option<String>| {
        url.clone()
            .filter(|u| check_endpoint(u).is_ok() && !u.chars().any(char::is_control))
    };
    let bare = safe(&started.verification_uri).unwrap_or_else(|| format!("{origin}{PAGE}"));
    let complete = safe(&started.verification_uri_complete)
        .unwrap_or_else(|| format!("{origin}{PAGE}&code={}", started.user_code));
    (bare, complete)
}

/// Signs in with the browser at the qloudpulse at `origin` (see [`origin`]): says the code on the terminal, opens the
/// browser, and waits for the member to approve, until the code expires. The device code and the token are never
/// said.
pub fn sign_in(
    origin: &str,
    http: &mut impl Http,
    clock: &mut impl Clock,
    browser: &mut impl Browser,
    terminal: &mut impl Terminal,
) -> Result<SignedIn> {
    let start_url = format!("{origin}{START}");
    let answer = http.post(&start_url, &json!({"client": client()}))?;
    match answer.status {
        200 => {}
        404 => bail!(
            "this qloudpulse has no sign-in with the browser yet (404 at {start_url}). Set QLOUDPULSE_TOKEN to a token \
             from Your tokens ({}) instead",
            tokens_page(origin)
        ),
        429 | 503 => bail!(
            "qloudpulse cannot start a login now ({}): {}",
            answer.status,
            detail(&answer.body)
        ),
        s => bail!("qloudpulse answered {s} to the login: {}", detail(&answer.body)),
    }
    let started: Started =
        serde_json::from_str(&answer.body).context("qloudpulse's answer to the login is not one the client reads")?;
    if !started.device_code.starts_with("qpdc_") || !is_user_code(&started.user_code) {
        bail!("qloudpulse's answer to the login is not one the client reads");
    }
    let expires_in = started.expires_in.clamp(1, 3600);
    let mut interval = started.interval.clamp(1, 60);
    let (bare, complete) = page(origin, &started);

    terminal.say(&format!("qloudpulse: your one-time code is {}", started.user_code));
    terminal.say(&format!(
        "qloudpulse: open {complete} and approve it if the page shows the same code"
    ));
    terminal.say(&format!(
        "qloudpulse: (on another device, open {bare} and type the code)"
    ));
    if browser.open(&complete) {
        terminal.say("qloudpulse: the page is opening in your browser.");
    } else {
        terminal
            .say("qloudpulse: could not open a browser here: open the address yourself, on this or another device.");
    }
    let minutes = expires_in.div_ceil(60);
    terminal.say(&format!(
        "qloudpulse: waiting for you to approve it (the code lasts {minutes} minute{})...",
        if minutes == 1 { "" } else { "s" }
    ));

    let token_url = format!("{origin}{TOKEN}");
    let deadline = clock.now() + expires_in;
    let mut said_unreachable = false;
    loop {
        clock.sleep(interval);
        if clock.now() >= deadline {
            bail!("the code ran out before it was approved: run `qloudpulse-stack login` again");
        }
        let answer = match http.post(&token_url, &json!({"deviceCode": started.device_code})) {
            Ok(answer) => answer,
            Err(e) => {
                if !said_unreachable {
                    terminal.say(&format!("qloudpulse: {e:#}; still trying."));
                    said_unreachable = true;
                }
                continue;
            }
        };
        match answer.status {
            200 => {
                let signed_in: SignedIn = serde_json::from_str(&answer.body)
                    .context("qloudpulse's token answer is not one the client reads")?;
                if !looks_like_token(&signed_in.token) {
                    bail!("qloudpulse answered with what is not a qloudpulse token");
                }
                return Ok(signed_in);
            }
            400 => {
                let problem: Problem = serde_json::from_str(&answer.body).unwrap_or_default();
                let detail = problem.detail.as_deref().map(plain).unwrap_or_default();
                match problem.error.as_deref() {
                    Some("authorization_pending") => {}
                    Some("slow_down") => interval = (interval + 5).min(60),
                    Some("expired_token") => {
                        bail!("the login expired or was already used: run `qloudpulse-stack login` again")
                    }
                    Some("access_denied") => bail!("the login was not approved: {detail}"),
                    _ => bail!("qloudpulse refused the login (400): {detail}"),
                }
            }
            // Busy for a moment: keep asking until the code runs out.
            429 | 502 | 503 | 504 => {
                if !said_unreachable {
                    terminal.say(&format!(
                        "qloudpulse: qloudpulse is busy ({}); still trying.",
                        answer.status
                    ));
                    said_unreachable = true;
                }
            }
            s => bail!(
                "qloudpulse answered {s} while the login waited: {}",
                detail(&answer.body)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;

    use super::*;

    const TOKEN: &str = "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
    const DEVICE: &str = "qpdc_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

    /// qloudpulse's side of the login: scripted answers, keeping each request it was sent.
    #[derive(Default)]
    struct Server {
        answers: VecDeque<Result<(u16, String)>>,
        requests: Vec<(String, Value)>,
    }

    impl Server {
        fn new(answers: Vec<(u16, Value)>) -> Server {
            Server {
                answers: answers.into_iter().map(|(s, b)| Ok((s, b.to_string()))).collect(),
                requests: Vec::new(),
            }
        }
    }

    impl Http for Server {
        fn post(&mut self, url: &str, body: &Value) -> Result<Answer> {
            self.requests.push((url.to_string(), body.clone()));
            let (status, body) = self.answers.pop_front().expect("asked more than the script answers")?;
            Ok(Answer { status, body })
        }
    }

    /// Time that passes only when slept, keeping each sleep.
    #[derive(Default)]
    struct Time {
        now: u64,
        slept: Vec<u64>,
    }

    impl Clock for Time {
        fn now(&self) -> u64 {
            self.now
        }
        fn sleep(&mut self, seconds: u64) {
            self.now += seconds;
            self.slept.push(seconds);
        }
    }

    struct Opens(bool, Vec<String>);

    impl Browser for Opens {
        fn open(&mut self, url: &str) -> bool {
            self.1.push(url.to_string());
            self.0
        }
    }

    #[derive(Default)]
    struct Said(Vec<String>);

    impl Terminal for Said {
        fn interactive(&self) -> bool {
            true
        }
        fn ask(&mut self, _: &str) -> io::Result<String> {
            panic!("the login asks nothing")
        }
        fn confirm(&mut self, _: &str) -> io::Result<String> {
            panic!("the login asks nothing")
        }
        fn say(&mut self, line: &str) {
            self.0.push(line.to_string());
        }
    }

    fn started(page: bool) -> Value {
        json!({
            "deviceCode": DEVICE,
            "userCode": "WDJB-MJHT",
            "verificationUri": if page { json!("https://app.qloudpulse.com/ui/?cli-login") } else { Value::Null },
            "verificationUriComplete":
                if page { json!("https://app.qloudpulse.com/ui/?cli-login&code=WDJB-MJHT") } else { Value::Null },
            "expiresIn": 600,
            "interval": 5
        })
    }

    fn pending(error: &str, detail: &str) -> (u16, Value) {
        (
            400,
            json!({"title": "Not signed in yet", "status": 400, "error": error, "detail": detail}),
        )
    }

    fn made() -> (u16, Value) {
        (
            200,
            json!({"token": TOKEN, "company": "acme", "name": "me-3f9a1c07", "expiresAt": "2027-02-17T08:01:00Z"}),
        )
    }

    struct Run {
        result: Result<SignedIn>,
        server: Server,
        time: Time,
        opened: Vec<String>,
        said: Vec<String>,
    }

    fn attempt(origin: &str, answers: Vec<(u16, Value)>, opens: bool) -> Run {
        attempt_with(origin, Server::new(answers), opens)
    }

    fn attempt_with(origin: &str, mut server: Server, opens: bool) -> Run {
        let mut time = Time::default();
        let mut browser = Opens(opens, Vec::new());
        let mut said = Said::default();
        let result = sign_in(origin, &mut server, &mut time, &mut browser, &mut said);
        for line in &said.0 {
            assert!(!line.contains(DEVICE) && !line.contains(TOKEN), "said a secret: {line}");
        }
        Run {
            result,
            server,
            time,
            opened: browser.1,
            said: said.0,
        }
    }

    #[test]
    fn hosted_qloudpulse_signs_in_on_app_and_another_on_its_own_address() {
        assert_eq!(origin("https://api.qloudpulse.com/v1/estimates").unwrap(), HOSTED);
        assert_eq!(
            origin("https://app.qloudpulse.com/api/report/public/estimates").unwrap(),
            HOSTED
        );
        assert_eq!(
            origin("https://QLOUDPULSE.example.com:8443/v1/estimates?x=1").unwrap(),
            "https://qloudpulse.example.com:8443"
        );
        assert_eq!(
            origin("http://localhost:8080/v1/estimates").unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            origin("https://me@qloudpulse.example.com/v1").unwrap(),
            "https://qloudpulse.example.com"
        );
        assert!(
            origin("http://qloudpulse.example.com/v1").is_err(),
            "never in the clear"
        );
    }

    #[test]
    fn the_client_names_its_version_and_system_only() {
        let client = client();
        assert!(
            client.starts_with(&format!("qloudpulse-stack {} on ", env!("CARGO_PKG_VERSION"))),
            "{client}"
        );
        assert!(client.len() <= 100 && !client.contains('\n'));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(client.ends_with(" on macOS arm64"), "{client}");
    }

    #[test]
    fn a_login_says_the_code_opens_the_browser_and_waits_for_approval() {
        let run = attempt(
            HOSTED,
            vec![
                (200, started(true)),
                pending("authorization_pending", "Not approved yet."),
                made(),
            ],
            true,
        );
        let signed_in = run.result.unwrap();
        assert_eq!(signed_in.token, TOKEN);
        assert_eq!(signed_in.company, "acme");
        assert_eq!(signed_in.expires_at.as_deref(), Some("2027-02-17T08:01:00Z"));

        let urls: Vec<&str> = run.server.requests.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://app.qloudpulse.com/api/report/public/cli-login",
                "https://app.qloudpulse.com/api/report/public/cli-login/token",
                "https://app.qloudpulse.com/api/report/public/cli-login/token",
            ]
        );
        assert_eq!(run.server.requests[0].1, json!({"client": client()}));
        assert_eq!(run.server.requests[1].1, json!({"deviceCode": DEVICE}));
        assert_eq!(run.opened, ["https://app.qloudpulse.com/ui/?cli-login&code=WDJB-MJHT"]);
        assert_eq!(run.time.slept, [5, 5], "every interval");
        assert!(run.said[0].ends_with("WDJB-MJHT"), "{:?}", run.said);
        assert!(
            run.said.iter().any(|l| l.contains("opening in your browser")),
            "{:?}",
            run.said
        );
        assert!(run.said.iter().any(|l| l.contains("10 minutes")), "{:?}", run.said);
    }

    #[test]
    fn without_qloudpulse_s_page_address_it_is_the_endpoint_s() {
        let run = attempt(
            "https://qloudpulse.example.com",
            vec![(200, started(false)), made()],
            false,
        );
        run.result.unwrap();
        assert_eq!(
            run.server.requests[0].0,
            "https://qloudpulse.example.com/api/report/public/cli-login"
        );
        assert_eq!(
            run.opened,
            ["https://qloudpulse.example.com/ui/?cli-login&code=WDJB-MJHT"]
        );
        assert!(
            run.said
                .iter()
                .any(|l| l.contains("https://qloudpulse.example.com/ui/?cli-login&code=WDJB-MJHT")),
            "{:?}",
            run.said
        );
        assert!(
            run.said.iter().any(|l| l.contains("open the address yourself")),
            "{:?}",
            run.said
        );
    }

    #[test]
    fn a_page_address_the_client_would_not_open_is_replaced_by_its_own() {
        let mut odd = started(true);
        odd["verificationUriComplete"] = json!("file:///etc/passwd");
        let run = attempt(HOSTED, vec![(200, odd), made()], true);
        run.result.unwrap();
        assert_eq!(run.opened, ["https://app.qloudpulse.com/ui/?cli-login&code=WDJB-MJHT"]);
    }

    #[test]
    fn slow_down_adds_five_seconds() {
        let run = attempt(
            HOSTED,
            vec![
                (200, started(true)),
                pending("slow_down", "Asked too soon."),
                pending("authorization_pending", ""),
                made(),
            ],
            true,
        );
        run.result.unwrap();
        assert_eq!(run.time.slept, [5, 10, 10]);
    }

    #[test]
    fn refused_or_expired_stops_with_why() {
        let run = attempt(
            HOSTED,
            vec![
                (200, started(true)),
                pending("access_denied", "You have 10 tokens already."),
            ],
            true,
        );
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "the login was not approved: You have 10 tokens already."
        );

        let run = attempt(
            HOSTED,
            vec![(200, started(true)), pending("expired_token", "No such login.")],
            true,
        );
        assert!(
            run.result
                .unwrap_err()
                .to_string()
                .contains("run `qloudpulse-stack login` again")
        );
    }

    #[test]
    fn the_code_runs_out_on_the_client_s_clock_too() {
        let mut answers = vec![(200, started(true))];
        answers.extend((0..119).map(|_| pending("authorization_pending", "")));
        let run = attempt(HOSTED, answers, true);
        let err = run.result.unwrap_err().to_string();
        assert!(err.contains("ran out"), "{err}");
        assert_eq!(run.time.now, 600);
        assert_eq!(
            run.server.requests.len(),
            120,
            "the start and 119 questions, none at 600 s"
        );
    }

    #[test]
    fn a_moment_without_qloudpulse_is_waited_out() {
        let mut server = Server::new(vec![(200, started(true))]);
        server
            .answers
            .push_back(Err(anyhow::anyhow!("could not reach qloudpulse")));
        server.answers.push_back(Ok((503, String::new())));
        server.answers.push_back(Ok((200, made().1.to_string())));
        let run = attempt_with(HOSTED, server, true);
        run.result.unwrap();
        assert_eq!(
            run.said.iter().filter(|l| l.contains("still trying")).count(),
            1,
            "said once: {:?}",
            run.said
        );
    }

    #[test]
    fn a_qloudpulse_without_browser_sign_in_says_to_use_a_token() {
        let run = attempt(HOSTED, vec![(404, json!({"status": 404}))], true);
        let err = run.result.unwrap_err().to_string();
        assert!(err.contains("404") && err.contains("QLOUDPULSE_TOKEN"), "{err}");
        assert!(
            err.contains("https://app.qloudpulse.com/ui/?profile#my-tokens"),
            "{err}"
        );
        assert!(run.opened.is_empty());
    }

    #[test]
    fn too_many_logins_says_qloudpulse_s_detail() {
        for status in [429, 503] {
            let run = attempt(
                HOSTED,
                vec![(
                    status,
                    json!({"title": "Too many logins", "detail": "Try again in a minute.\u{1b}[2J"}),
                )],
                true,
            );
            let err = run.result.unwrap_err().to_string();
            assert!(
                err.ends_with("Try again in a minute. [2J"),
                "control characters go: {err}"
            );
        }
    }

    #[test]
    fn an_answer_that_is_not_a_login_or_a_token_is_refused() {
        let mut bad = started(true);
        bad["deviceCode"] = json!("abc");
        assert!(attempt(HOSTED, vec![(200, bad)], true).result.is_err());

        let mut bad = started(true);
        bad["userCode"] = json!("WDJB&x=1");
        assert!(attempt(HOSTED, vec![(200, bad)], true).result.is_err());

        let not_a_token = attempt(
            HOSTED,
            vec![
                (200, started(true)),
                (200, json!({"token": "hunter2", "company": "acme"})),
            ],
            true,
        );
        assert!(
            not_a_token
                .result
                .unwrap_err()
                .to_string()
                .contains("not a qloudpulse token")
        );
    }
}

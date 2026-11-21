//! The token a run sends: from `QLOUDPULSE_TOKEN`; else the one `qloudpulse-stack login` kept on this machine (see
//! [`crate::credentials`]); else, when someone is at a terminal, they are offered to sign in with the browser, which
//! keeps the token it gets, or asked to paste a token, with the input hidden, for this run only. stdin is usually the
//! preview, so every answer is read from the terminal itself (`/dev/tty`, or the console on Windows), never from
//! stdin. A pasted token is not written anywhere, and the environment is left as it was.

use std::io::{self, IsTerminal as _, Write as _};

use anyhow::{Context as _, Result, bail};

use crate::send::{TOKEN_PAGE, is_hosted, missing_token};

/// How many answers a run takes before it stops.
pub const TRIES: usize = 3;

/// Where the question is asked and answered.
pub trait Terminal {
    /// Whether someone can answer: stderr is a terminal. Whether the terminal itself opens is known on [`ask`].
    ///
    /// [`ask`]: Terminal::ask
    fn interactive(&self) -> bool;
    /// Writes the prompt to stderr and reads one line from the terminal, without echo.
    fn ask(&mut self, prompt: &str) -> io::Result<String>;
    /// Writes the prompt to stderr and reads one line from the terminal, with echo: for a yes or no.
    fn confirm(&mut self, prompt: &str) -> io::Result<String>;
    /// Writes a line to stderr.
    fn say(&mut self, line: &str);
}

/// The terminal the client runs at.
pub struct Tty;

impl Terminal for Tty {
    fn interactive(&self) -> bool {
        io::stderr().is_terminal()
    }

    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        let mut err = io::stderr();
        err.write_all(prompt.as_bytes())?;
        err.flush()?;
        rpassword::read_password()
    }

    fn confirm(&mut self, prompt: &str) -> io::Result<String> {
        use std::io::BufRead as _;
        // The terminal itself, as rpassword reads it: stdin is the preview.
        #[cfg(windows)]
        let tty = std::fs::OpenOptions::new().read(true).write(true).open("CONIN$")?;
        #[cfg(not(windows))]
        let tty = std::fs::File::open("/dev/tty")?;
        let mut err = io::stderr();
        err.write_all(prompt.as_bytes())?;
        err.flush()?;
        let mut line = String::new();
        if io::BufReader::new(tty).read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the terminal closed"));
        }
        Ok(line)
    }

    fn say(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// Whether a value has the shape of a qloudpulse token: `qpev_<company>.<id>.<secret>`, as qloudpulse makes them
/// (`EventToken`): a company name of up to 63 letters, digits, `_` and `-`, 8 hex digits, and 43 of base64url.
pub fn looks_like_token(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("qpev_") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    let url_safe = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let [company, id, secret] = parts.as_slice() else {
        return false;
    };
    (1..=63).contains(&company.len())
        && company.starts_with(|c: char| c.is_ascii_alphanumeric())
        && company.chars().all(url_safe)
        && id.len() == 8
        && id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        && secret.len() == 43
        && secret.chars().all(url_safe)
}

/// What the terminal is asked, with where a token is made: linked for hosted qloudpulse, as the error is.
pub fn prompt(endpoint: &str) -> String {
    if is_hosted(endpoint) {
        format!("qloudpulse token (qpev_..., make one at {TOKEN_PAGE}): ")
    } else {
        "qloudpulse token (qpev_..., make one under Your tokens, under your name in your qloudpulse): ".to_string()
    }
}

/// What a run at a terminal without a token is offered first.
pub const OFFER: &str = "Sign in with your browser? [Y/n] ";

/// The token for this run: `QLOUDPULSE_TOKEN`'s value, trimmed, when it is set and not blank; else the token `stored`
/// finds, kept by `qloudpulse-stack login`; else, at a terminal, [`OFFER`]: yes is `sign_in`, no is the answer to
/// [`prompt`], in up to [`TRIES`] tries; else the error that says where a token is made.
pub fn resolve<T: Terminal>(
    env: Option<String>,
    endpoint: &str,
    stored: impl FnOnce() -> Option<String>,
    terminal: &mut T,
    sign_in: impl FnOnce(&mut T) -> Result<String>,
) -> Result<String> {
    if let Some(token) = env.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Ok(token);
    }
    if let Some(token) = stored() {
        return Ok(token);
    }
    if !terminal.interactive() {
        bail!("{}", missing_token(endpoint));
    }
    terminal.say("qloudpulse: QLOUDPULSE_TOKEN is unset, and this machine is not signed in to qloudpulse.");
    let mut attempt = 0;
    loop {
        attempt += 1;
        // A terminal that does not open is no terminal: the same error as in CI.
        let Ok(answer) = terminal.confirm(OFFER) else {
            bail!("{}", missing_token(endpoint));
        };
        let answer = answer.trim();
        match answer.to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return sign_in(terminal).context("could not sign in with the browser"),
            "n" | "no" => break,
            // Someone used to the paste prompt pastes a token here: it is used as it would be there.
            _ if looks_like_token(answer) => {
                terminal.say(FOR_THIS_RUN);
                return Ok(answer.to_string());
            }
            _ if attempt < TRIES => terminal.say("qloudpulse: answer y or n."),
            _ => bail!("{}", missing_token(endpoint)),
        }
    }
    let question = prompt(endpoint);
    for attempt in 1..=TRIES {
        let Ok(answer) = terminal.ask(&question) else {
            bail!("{}", missing_token(endpoint));
        };
        let answer = answer.trim();
        if looks_like_token(answer) {
            terminal.say(FOR_THIS_RUN);
            return Ok(answer.to_string());
        }
        let left = TRIES - attempt;
        if left > 0 {
            let what = if answer.is_empty() {
                "nothing was typed"
            } else {
                "that is not a qloudpulse token (qpev_<company>.<id>.<secret>)"
            };
            terminal.say(&format!(
                "qloudpulse: {what}; {left} {} left.",
                if left == 1 { "try" } else { "tries" }
            ));
        }
    }
    bail!("no qloudpulse token after {TRIES} tries: {}", missing_token(endpoint))
}

/// What is said once a pasted token is taken.
const FOR_THIS_RUN: &str = "qloudpulse: the token is used for this run only and saved nowhere. Next time, \
                            `qloudpulse-stack login` keeps one on this machine, or `export QLOUDPULSE_TOKEN=...` first, or \
                            keep it in the CI secret.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::DEFAULT_ENDPOINT;

    const TOKEN: &str = "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
    const OTHER: &str = "qpev_acme.0a1b2c3d.ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq";

    /// A terminal with scripted answers, keeping what it was asked and told.
    struct Script {
        interactive: bool,
        /// Answers to the hidden question, the paste prompt.
        answers: Vec<io::Result<String>>,
        /// Answers to the echoed question, the offer to sign in.
        confirms: Vec<io::Result<String>>,
        asked: Vec<String>,
        confirmed: Vec<String>,
        said: Vec<String>,
    }

    impl Script {
        fn new(interactive: bool, answers: Vec<io::Result<String>>) -> Script {
            Script::offered(interactive, vec![ok("n")], answers)
        }

        fn offered(interactive: bool, confirms: Vec<io::Result<String>>, answers: Vec<io::Result<String>>) -> Script {
            Script {
                interactive,
                answers,
                confirms,
                asked: Vec::new(),
                confirmed: Vec::new(),
                said: Vec::new(),
            }
        }
    }

    impl Terminal for Script {
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn ask(&mut self, prompt: &str) -> io::Result<String> {
            self.asked.push(prompt.to_string());
            if self.answers.is_empty() {
                panic!("asked more than the script answers");
            }
            self.answers.remove(0)
        }
        fn confirm(&mut self, prompt: &str) -> io::Result<String> {
            self.confirmed.push(prompt.to_string());
            if self.confirms.is_empty() {
                panic!("offered more than the script answers");
            }
            self.confirms.remove(0)
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
    }

    fn ok(s: &str) -> io::Result<String> {
        Ok(s.to_string())
    }

    fn no_sign_in(_: &mut Script) -> Result<String> {
        panic!("signed in when it should not")
    }

    /// `resolve` with nothing kept on the machine and no sign-in.
    fn resolve_here(env: Option<String>, endpoint: &str, t: &mut Script) -> Result<String> {
        resolve(env, endpoint, || None, t, no_sign_in)
    }

    #[test]
    fn the_shape_of_a_token() {
        assert_eq!(TOKEN.len(), "qpev_acme.0a1b2c3d.".len() + 43);
        assert!(looks_like_token(TOKEN));
        assert!(looks_like_token(&TOKEN.replace("acme", "acme-eu_2")));
        for not in [
            "",
            "qpev_",
            "acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ",
            "qpev_.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ",
            "qpev_-acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ",
            "qpev_acme.0A1B2C3D.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ",
            "qpev_acme.0a1b2c3.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ",
            "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP",
            "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP=",
            "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ.x",
            "qpev_acme 0a1b2c3d abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ",
        ] {
            assert!(!looks_like_token(not), "{not:?}");
        }
    }

    #[test]
    fn the_environment_wins_and_nothing_is_asked_or_looked_up() {
        let mut t = Script::new(true, vec![]);
        let token = resolve(
            Some(format!("  {TOKEN}\n")),
            DEFAULT_ENDPOINT,
            || panic!("the keychain is not read when QLOUDPULSE_TOKEN is set"),
            &mut t,
            no_sign_in,
        )
        .unwrap();
        assert_eq!(token, TOKEN);
        assert!(t.asked.is_empty() && t.confirmed.is_empty() && t.said.is_empty());
    }

    #[test]
    fn a_kept_token_comes_next_and_nothing_is_asked() {
        for interactive in [true, false] {
            let mut t = Script::new(interactive, vec![]);
            let token = resolve(None, DEFAULT_ENDPOINT, || Some(OTHER.to_string()), &mut t, no_sign_in).unwrap();
            assert_eq!(token, OTHER);
            assert!(t.asked.is_empty() && t.confirmed.is_empty() && t.said.is_empty());
        }
    }

    #[test]
    fn without_a_terminal_the_error_is_today_s_and_names_login() {
        for env in [None, Some("   ".to_string())] {
            let mut t = Script::new(false, vec![]);
            let err = resolve_here(env, DEFAULT_ENDPOINT, &mut t).unwrap_err().to_string();
            assert_eq!(err, missing_token(DEFAULT_ENDPOINT));
            assert!(err.contains("qloudpulse-stack login"), "{err}");
            assert!(t.asked.is_empty() && t.confirmed.is_empty());
        }
    }

    #[test]
    fn a_terminal_that_does_not_open_gives_the_same_error() {
        let mut t = Script::offered(true, vec![Err(io::Error::other("no /dev/tty"))], vec![]);
        let err = resolve_here(None, DEFAULT_ENDPOINT, &mut t).unwrap_err().to_string();
        assert_eq!(err, missing_token(DEFAULT_ENDPOINT));
        assert_eq!(t.confirmed.len(), 1);

        let mut t = Script::new(true, vec![Err(io::Error::other("no /dev/tty"))]);
        let err = resolve_here(None, DEFAULT_ENDPOINT, &mut t).unwrap_err().to_string();
        assert_eq!(err, missing_token(DEFAULT_ENDPOINT));
        assert_eq!(t.asked.len(), 1);
    }

    #[test]
    fn at_a_terminal_yes_signs_in_with_the_browser() {
        for yes in ["", "\n", "y", "Y", "yes", " YES "] {
            let mut t = Script::offered(true, vec![ok(yes)], vec![]);
            let token = resolve(
                None,
                DEFAULT_ENDPOINT,
                || None,
                &mut t,
                |t: &mut Script| {
                    t.say("signed in");
                    Ok(OTHER.to_string())
                },
            )
            .unwrap();
            assert_eq!(token, OTHER, "{yes:?}");
            assert_eq!(t.confirmed, ["Sign in with your browser? [Y/n] "]);
            assert!(t.asked.is_empty(), "no paste prompt after yes");
            assert_eq!(t.said.last().map(String::as_str), Some("signed in"));
        }
    }

    #[test]
    fn a_sign_in_that_fails_says_so() {
        let mut t = Script::offered(true, vec![ok("y")], vec![]);
        let err = resolve(
            None,
            DEFAULT_ENDPOINT,
            || None,
            &mut t,
            |_: &mut Script| anyhow::bail!("the login was not approved: refused"),
        )
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "could not sign in with the browser: the login was not approved: refused"
        );
    }

    #[test]
    fn no_asks_for_a_token_to_paste() {
        let mut t = Script::offered(true, vec![ok("maybe"), ok("n")], vec![ok(TOKEN)]);
        assert_eq!(resolve_here(None, DEFAULT_ENDPOINT, &mut t).unwrap(), TOKEN);
        assert_eq!(t.confirmed.len(), 2);
        assert!(
            t.said.contains(&"qloudpulse: answer y or n.".to_string()),
            "{:?}",
            t.said
        );
        assert_eq!(t.asked.len(), 1);

        let mut t = Script::offered(true, vec![ok("a"), ok("b"), ok("c")], vec![]);
        let err = resolve_here(None, DEFAULT_ENDPOINT, &mut t).unwrap_err().to_string();
        assert_eq!(err, missing_token(DEFAULT_ENDPOINT), "three answers that are neither");
    }

    #[test]
    fn a_token_pasted_at_the_offer_is_taken_for_this_run() {
        let mut t = Script::offered(true, vec![ok(&format!(" {TOKEN} "))], vec![]);
        assert_eq!(resolve_here(None, DEFAULT_ENDPOINT, &mut t).unwrap(), TOKEN);
        assert!(t.asked.is_empty());
        assert!(t.said.last().unwrap().contains("this run only"));
    }

    #[test]
    fn pasted_it_says_the_token_is_for_this_run_only() {
        let mut t = Script::new(true, vec![ok(&format!(" {TOKEN} "))]);
        assert_eq!(resolve_here(None, DEFAULT_ENDPOINT, &mut t).unwrap(), TOKEN);
        assert_eq!(
            t.asked,
            ["qloudpulse token (qpev_..., make one at https://app.qloudpulse.com/ui/?profile#my-tokens): "]
        );
        let last = t.said.last().unwrap();
        assert!(
            last.contains("this run only")
                && last.contains("export QLOUDPULSE_TOKEN")
                && last.contains("qloudpulse-stack login"),
            "{:?}",
            t.said
        );
        assert!(!t.said.iter().any(|l| l.contains(TOKEN)), "the token is never echoed");
    }

    #[test]
    fn another_endpoint_is_asked_without_a_guessed_address() {
        let mut t = Script::new(true, vec![ok(TOKEN)]);
        resolve_here(None, "https://qloudpulse.example.com/v1/estimates", &mut t).unwrap();
        assert_eq!(
            t.asked,
            ["qloudpulse token (qpev_..., make one under Your tokens, under your name in your qloudpulse): "]
        );
    }

    #[test]
    fn three_tries_then_the_error() {
        let mut t = Script::new(true, vec![ok("hunter2"), ok(""), ok(TOKEN)]);
        assert_eq!(resolve_here(None, DEFAULT_ENDPOINT, &mut t).unwrap(), TOKEN);
        assert_eq!(
            t.said[1..3],
            [
                "qloudpulse: that is not a qloudpulse token (qpev_<company>.<id>.<secret>); 2 tries left.",
                "qloudpulse: nothing was typed; 1 try left.",
            ]
        );
        assert!(
            !t.said.iter().any(|l| l.contains("hunter2")),
            "a wrong answer is never echoed"
        );

        let mut t = Script::new(true, vec![ok("a"), ok("b"), ok("c")]);
        let err = resolve_here(None, DEFAULT_ENDPOINT, &mut t).unwrap_err().to_string();
        assert_eq!(t.asked.len(), TRIES);
        assert_eq!(
            err,
            format!("no qloudpulse token after 3 tries: {}", missing_token(DEFAULT_ENDPOINT))
        );
        assert_eq!(t.said.len(), 3, "the opening line and no 'tries left' after the last");
    }
}

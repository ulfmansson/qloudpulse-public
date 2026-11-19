//! The token a run sends: from `QLOUDPULSE_TOKEN`, or, when that is unset and someone is at a terminal, asked for
//! there with the input hidden. stdin is usually the preview, so the answer is read from the terminal itself
//! (`/dev/tty`, or the console on Windows), never from stdin. The answer is used for this run only: it is not written
//! anywhere, and the environment is left as it was.

use std::io::{self, IsTerminal as _, Write as _};

use anyhow::{Result, bail};

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

/// The token for this run: `QLOUDPULSE_TOKEN`'s value, trimmed, when it is set and not blank; else, at a terminal,
/// the answer to [`prompt`], in up to [`TRIES`] tries; else the error that says where a token is made.
pub fn resolve(env: Option<String>, endpoint: &str, terminal: &mut impl Terminal) -> Result<String> {
    if let Some(token) = env.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Ok(token);
    }
    if !terminal.interactive() {
        bail!("{}", missing_token(endpoint));
    }
    let question = prompt(endpoint);
    for attempt in 1..=TRIES {
        // A terminal that does not open is no terminal: the same error as in CI.
        let Ok(answer) = terminal.ask(&question) else {
            bail!("{}", missing_token(endpoint));
        };
        let answer = answer.trim();
        if looks_like_token(answer) {
            terminal.say(
                "qloudpulse: the token is used for this run only and saved nowhere. Next time, `export \
                 QLOUDPULSE_TOKEN=...` first, or keep it in the CI secret.",
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::DEFAULT_ENDPOINT;

    const TOKEN: &str = "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

    /// A terminal with scripted answers, keeping what it was asked and told.
    struct Script {
        interactive: bool,
        answers: Vec<io::Result<String>>,
        asked: Vec<String>,
        said: Vec<String>,
    }

    impl Script {
        fn new(interactive: bool, answers: Vec<io::Result<String>>) -> Script {
            Script {
                interactive,
                answers,
                asked: Vec::new(),
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
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
    }

    fn ok(s: &str) -> io::Result<String> {
        Ok(s.to_string())
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
    fn the_environment_wins_and_nothing_is_asked() {
        let mut t = Script::new(true, vec![]);
        let token = resolve(Some(format!("  {TOKEN}\n")), DEFAULT_ENDPOINT, &mut t).unwrap();
        assert_eq!(token, TOKEN);
        assert!(t.asked.is_empty() && t.said.is_empty());
    }

    #[test]
    fn without_a_terminal_the_error_is_today_s() {
        for env in [None, Some("   ".to_string())] {
            let mut t = Script::new(false, vec![]);
            let err = resolve(env, DEFAULT_ENDPOINT, &mut t).unwrap_err().to_string();
            assert_eq!(err, missing_token(DEFAULT_ENDPOINT));
            assert!(t.asked.is_empty());
        }
    }

    #[test]
    fn a_terminal_that_does_not_open_gives_the_same_error() {
        let mut t = Script::new(true, vec![Err(io::Error::other("no /dev/tty"))]);
        let err = resolve(None, DEFAULT_ENDPOINT, &mut t).unwrap_err().to_string();
        assert_eq!(err, missing_token(DEFAULT_ENDPOINT));
        assert_eq!(t.asked.len(), 1);
    }

    #[test]
    fn at_a_terminal_it_asks_and_says_the_token_is_for_this_run_only() {
        let mut t = Script::new(true, vec![ok(&format!(" {TOKEN} "))]);
        assert_eq!(resolve(None, DEFAULT_ENDPOINT, &mut t).unwrap(), TOKEN);
        assert_eq!(
            t.asked,
            ["qloudpulse token (qpev_..., make one at https://app.qloudpulse.com/ui/?profile#my-tokens): "]
        );
        assert_eq!(t.said.len(), 1);
        assert!(
            t.said[0].contains("this run only") && t.said[0].contains("export QLOUDPULSE_TOKEN"),
            "{:?}",
            t.said
        );
        assert!(!t.said[0].contains(TOKEN), "the token is never echoed");
    }

    #[test]
    fn another_endpoint_is_asked_without_a_guessed_address() {
        let mut t = Script::new(true, vec![ok(TOKEN)]);
        resolve(None, "https://qloudpulse.example.com/v1/estimates", &mut t).unwrap();
        assert_eq!(
            t.asked,
            ["qloudpulse token (qpev_..., make one under Your tokens, under your name in your qloudpulse): "]
        );
    }

    #[test]
    fn three_tries_then_the_error() {
        let mut t = Script::new(true, vec![ok("hunter2"), ok(""), ok(TOKEN)]);
        assert_eq!(resolve(None, DEFAULT_ENDPOINT, &mut t).unwrap(), TOKEN);
        assert_eq!(
            t.said[..2],
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
        let err = resolve(None, DEFAULT_ENDPOINT, &mut t).unwrap_err().to_string();
        assert_eq!(t.asked.len(), TRIES);
        assert_eq!(
            err,
            format!("no qloudpulse token after 3 tries: {}", missing_token(DEFAULT_ENDPOINT))
        );
        assert_eq!(t.said.len(), 2, "no 'tries left' after the last");
    }
}

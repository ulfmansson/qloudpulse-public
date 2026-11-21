# Changelog

What changed in `qloudpulse-collect`, newest first. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). A release's notes on GitHub are its section here.

A pull request that changes what the client does adds a line under Unreleased. A release moves those lines into its
version's section and dates it: `release.sh` refuses a version whose section has no date, or while Unreleased still
has lines.

## [Unreleased]

## [0.1.0] - 2026-11-21

The first version. It collects what qloudpulse's AWS collectors read, with your own credentials, into a bundle you
inspect, redact and deliver. qloudpulse never gets access to your accounts.

- **Collects** what `aws-cost-collect` reads every hour, and once a day what `aws-cur-collect`,
  `aws-infra-collect`, `aws-forecast-collect` (opt-in: AWS bills Cost Explorer per request) and
  `aws-carbon-collect` read. That covers the bill (qloudpulse's CUR 2.0 queries in your Athena, results only), the
  inventory, network, exposure and settings (Cloud Control included), the forecast, and carbon. Not yet: the
  `usageMetrics` reads and ECR scan findings.
- **Only reads.** Every AWS operation it may call is in the compiled-in call plan (`call-plan`), it refuses any
  other, and `policy` prints the read-only IAM policy for exactly those. The answers are recorded as AWS sent them,
  so qloudpulse replays them through its unchanged collectors.
- **Redacts** by `qloudpulse-collect.toml`:
  - tags: unlisted ones are denied; others are kept, dropped, replaced or pseudonymized under your own key;
  - any field, by its path;
  - account and resource IDs, pseudonymized with their format kept, the same everywhere they appear;
  - `qloudpulse:environment`, derived from your tags or names before anything is redacted.
- **Checks before anything leaves:** `pack` stops while the bundle holds keys, tokens, passwords or email addresses.
  `analyze` shows every tag and what each rule costs in qloudpulse, `verify` checks a bundle against its manifest,
  and the local UI (`ui`, `run --ui`) shows all of it in a browser on this machine.
- **Delivers** every hour (`run`) to your own S3 bucket, to qloudpulse, or to your own command. The upload signs in
  as `qloudpulse-stack` does (`QLOUDPULSE_TOKEN`, or the token `login` kept). `[encrypt]` sends a JWE to
  qloudpulse's key instead of the plaintext, and `rotate-key` replaces your pseudonym key with a file to relink
  history.
- **No qloudpulse code at all, if you prefer:** `--no-default-features` builds a client without the upload, and
  holds no qloudpulse host.
- **Released as `qloudpulse-stack` is:** static binaries for Linux, macOS and Windows, `SHA256SUMS`, signed build
  provenance, an SBOM, reproducible builds, and `install.sh` and `install.ps1`. `--version` names the commit. The
  install scripts say which version is installed (in the install directory, or on PATH when none is given) and which
  they would install, and ask before replacing it; `--yes` (`-Yes`, `QLOUDPULSE_ASSUME_YES=1`) skips the question.

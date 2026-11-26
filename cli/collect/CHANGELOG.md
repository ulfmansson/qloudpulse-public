# Changelog

What changed in `qloudpulse-collect`, newest first. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). A release's notes on GitHub are its section here.

A pull request that changes what the client does adds a line under Unreleased. A release moves those lines into its
version's section and dates it: `release.sh` refuses a version whose section has no date, or while Unreleased still
has lines.

## [Unreleased]

- `run` collects when it starts, then at each aligned time as before; it no longer waits up to an hour for the first
  cycle. Started before `[run] minute` past the hour, it waits for that minute. An hour whose bundle is already in
  `output_dir` is not collected again (a restart within the hour only delivers what is owed), and a cycle that runs
  past an aligned time is followed at once by the next instead of leaving that hour out. `run --once` is unchanged.
- The local UI has a status strip on every page: which account and region the client is reading and how many
  answers it has, then the bill, packing and delivering; how the last cycle went, with its bundle; and the next run
  in UTC and in the browser's time, with a countdown. **Collect sample now** shows the same progress, and the page
  no longer reloads every 3 seconds while it runs.
- The local UI's header links to qloudpulse and to its page of collectors. The links send no referrer. `app_url` in
  the config names another app, or none with `""`; a build without the upload shows none.

## [0.1.1] - 2026-11-23

- `run --every` takes `1h` as well as `1`, the form the docs and the UI show; before, `1h` was refused.
- The Runs page names the whole command to start the loop, `qloudpulse-collect run --every 1h --ui`, not only its
  flags.

## [0.1.0] - 2026-11-22

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
  and the local UI (`ui`, `run --ui`) shows all of it in a browser on this machine. In Kubernetes too:
  through `kubectl port-forward` on any local port, `--ui-allow-host` for an Ingress or a Service name, and a
  Download of the config where it is a read-only ConfigMap.
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
- **Keeps itself current:** `update` installs the newest release over the running one, with the release's own
  install script, checked against the release's `SHA256SUMS` first. `--check` only says whether one is out;
  `--version` installs a given release; `--attest` also checks its signed provenance. The commands a person runs at a
  terminal (`init`, `analyze`, `verify`, `policy`, `call-plan`, `rotate-key`, `login`, `logout`), and `ui` on every
  page, say when a newer release is out, asking GitHub at most once a day. `collect`, `pack`, `deliver`, `upload` and
  `run` never ask, nor does `--log-json`, and `QLOUDPULSE_NO_UPDATE_CHECK=1` turns it off. Neither is in the build
  without the upload.

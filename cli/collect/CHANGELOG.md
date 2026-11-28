# Changelog

What changed in `qloudpulse-collect`, newest first. The format is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). A release's notes on GitHub are its section here.

A pull request that changes what the client does adds a line under Unreleased. A release moves those lines into its
version's section and dates it: `release.sh` refuses a version whose section has no date, or while Unreleased still
has lines.

## [Unreleased]

- **The config lives in `~/.config/qloudpulse` by default** (`$XDG_CONFIG_HOME/qloudpulse` if set,
  `%APPDATA%\qloudpulse` on Windows), beside the qloudpulse CLI's sign-in: with no `--config` and no
  `QLOUDPULSE_COLLECT_CONFIG`, the client reads `qloudpulse-collect.toml` there, and `init` writes it there with the
  key and the view token, creating the directory (0700). A `qloudpulse-collect.toml` in the current directory is still
  used first, so a setup made before keeps its key and its pseudonyms.
- **Raw snapshots and bundles go to the state directory**, not beside the config:
  `~/.local/state/qloudpulse/collect/<source_id>/work` and `.../bundles` (`$XDG_STATE_HOME` if set,
  `%LOCALAPPDATA%\qloudpulse` on Windows), one directory per `source_id`. Only when the config leaves `work_dir` and
  `output_dir` out: a config that names them, as `init` wrote them before, keeps its files where they are. The starter
  config leaves them out.
- **A delivered bundle is deleted**: once every `[deliver]` target has it, or once `upload` sent a bundle of
  `output_dir`, the bundle and its `.jwe` go. `[run] delete_delivered = false` keeps them until `keep_bundles` pushes
  them out, as before. A bundle outside `output_dir` is never deleted.
- `--help` ends with the files on this machine, worked out from the config the run would use: config, pseudonym key,
  view token, raw snapshots, bundles, sign-in token and update check.
- `cookie` 0.18.3, for RUSTSEC-2026-0594 (private cookie nonces could repeat in a process that forks). It comes in
  through `ureq`'s cookie store.
- **Encrypted values and the view token.** A new action, `encrypt`, for `[tags]` (`encrypt = ["Owner"]`) and
  `[[fields]]` (`action = "encrypt"`): the value leaves as `qpe1_<key id>_…`, AES-256-GCM under a key derived from
  the view token, `qp_…`, which `init` (or `view-token --create`) creates and only you hold. The same value always
  gives the same output, so qloudpulse still groups costs by it and the replay still matches. Enter the token in
  qloudpulse and your browser decrypts the values there; the token and the real values never reach qloudpulse.
  `view-token` prints it. The manifest names the token by its key id (`view_key_id`), never the token itself.
- The browser code that decrypts, `web/qloudpulse-values.js` (WebCrypto only, no network), is published here with
  test vectors the client's tests keep current.
- `pack` refuses a bundle that holds a view token (`qloudpulse-view-token`).
- Bundle `schema_version` 1.1.0: the manifest adds `view_key_id` and `rules.tags_encrypted`, both optional.
- After an upload, the client says where to follow the bundle on qloudpulse: `upload`, `deliver` and `run` print the
  report site's **Bundles arrived** page with the reference, and in the local UI a delivered `qloudpulse ✓` (Bundles,
  a bundle's page, Run history) links to it, as does the Bundles page's targets panel.

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

# Changelog

What changed in the qloudpulse CLI, newest first. The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A release's notes on GitHub are its
section here.

A pull request that changes what the client does adds a line under Unreleased. A release moves those lines into its
version's section and dates it: `release.sh` refuses a version whose section has no date, or while Unreleased still
has lines.

## [Unreleased]

### Added

- `--help` ends with the files on this machine: where `login` keeps the token (the system keychain, else
  `~/.config/qloudpulse/credentials`, or `QLOUDPULSE_CREDENTIALS_FILE`) and the update check's cache.

## [0.6.0] - 2026-11-22

### Added

- `qloudpulse-stack update` installs the newest release over the running one, with the release's own install script,
  checked against the release's `SHA256SUMS` first. `--check` only says whether one is out; `--version` installs a
  given release; `--attest` also checks its signed provenance.

### Changed

- The line saying a newer release is out, and the error below the oldest qloudpulse takes, name
  `qloudpulse-stack update`.

### Fixed

- A resource the change leaves the same sends its references, as a changed one does: an unchanged Auto Scaling group
  names its launch template, an ECS service its task definition, an Aurora instance its cluster. qloudpulse prices
  them through those, so before this every unchanged one came back not priced and the stack's cost left out its
  biggest resources. What such a resource refers to goes in the request too, while it has room.

## [0.5.0] - 2026-11-21

The first release since 0.3.1, and the first as `qloudpulse-stack`. 0.4.0 was tagged on 2026-11-21 but its release
run failed before it published anything, so its changes are in this list too.

### Added

- `qloudpulse-stack --version` names the commit a release was built from and that commit's date, as rustc does:
  `qloudpulse-stack <version> (1ab39062b 2026-11-21)`.
- Each binary carries the list of crates it was built from, written in by cargo auditable, so `cargo audit bin`,
  Trivy or Grype can check an installed client for crates with known vulnerabilities.
- `SHA256SUMS` and the signed build provenance cover the binary inside each archive as well as the archive, so
  `gh attestation verify "$(command -v qloudpulse-stack)"` checks an installed client.
- Releases are reproducible: the release workflow builds every target twice, on two runners, and publishes nothing
  unless the binaries are the same, and `cli/stack/release.sh --reproduce` at a release's tag builds it again and compares
  with the release's `SHA256SUMS`.
- `--no-store` (also `QLOUDPULSE_NO_STORE`): qloudpulse prices the change and keeps nothing, and the client sends no
  names or URNs. The comment has no link and says nothing was kept, as it also does when a company's admins chose to
  keep nothing. An answer that shows the estimate was kept anyway, as from an endpoint older than ADR 0140, stops the
  run with an error.

### Changed

- **Renamed: the client is now `qloudpulse-stack`**, to make room for other qloudpulse command-line tools. The binary
  is `qloudpulse-stack` (`qloudpulse-stack.exe` on Windows), releases are tagged `qloudpulse-stack-<version>` and
  their archives are `qloudpulse-stack-<version>-<target>.tar.gz` (or `.zip`), and the source moved to `cli/stack/`.
  Replace `qloudpulse` with `qloudpulse-stack` in pipelines and scripts. `QLOUDPULSE_TOKEN` and the other
  `QLOUDPULSE_*` variables, and the token `login` kept, are unchanged and shared by every qloudpulse CLI. A token
  `login` makes from now on is named `qloudpulse-stack <version> on <os> <arch>`. Releases up to 0.3.1 keep their
  names: tag `qloudpulse-cli-<version>`, binary `qloudpulse`.
- `--region` now wins over the stack's `aws:region`: it is the region of every resource for which neither the preview
  nor its provider names one. Before, the stack's `aws:region` came first.
- Each resource is sent with its name and its Pulumi URN, and qloudpulse keeps them with the estimate, so its page and
  the stack's cost history name every resource (qloudpulse's ADR 0140). From such a qloudpulse the link is the short
  `…/ui/?estimate=est_…`, without the names after `#`; to an older endpoint's link the client still adds them.
- At a terminal, "See it in qloudpulse" is the last line, after the table and notes. The pull request comment keeps it
  under the headline.
- The install scripts ask before replacing an installed client: they say which version is installed (as
  `qloudpulse-stack`, or `qloudpulse` up to 0.3.1, in the install directory, or on PATH when none is given) and which
  they would install, and leave one already at that version alone. `--yes` (`-Yes`, `QLOUDPULSE_ASSUME_YES=1`) skips
  the question; `--version`, and a pipeline with no terminal to ask on, go ahead without asking.
- The copies of `install.sh` and `install.ps1` in the repository find the newest release under either name,
  `qloudpulse-stack-<version>` or `qloudpulse-cli-<version>`, and install 0.3.1 and older by their old names. A lookup
  that fails, offline or past GitHub's rate limit, says so.

### Fixed

- A resource's region is found the way the AWS provider finds it, so a stack whose region comes from somewhere other
  than its provider's `region` or the stack's `aws:region` is no longer sent with `"region": null` and left
  unpriced. First found: the resource's own `region` (AWS provider v7), from its inputs or outputs; its provider's
  `region`, from the new state, else the old one, so a provider whose region is known only after apply keeps the one
  it had; `--region`; the stack's `aws:region`; `AWS_REGION`; `AWS_DEFAULT_REGION`; and the region of the AWS profile
  in use (the provider's `profile`, else the stack's `aws:profile`, else `AWS_PROFILE`, else `default`) in
  `AWS_CONFIG_FILE` or `~/.aws/config`. Only the region is read from that file, never credentials, and every value
  must still be a region name.
- A resource that still has no region is said on stderr and in the comment, with how to give it one
  (`pulumi config set aws:region <region>` or `--region`), instead of leaving the totals empty without a word.
- A priced attribute the inputs leave out, such as a default the provider filled in (an EBS volume's `type`, an
  instance's root volume type), is sent from the outputs: for a resource left the same, and for the side before an
  update, a replacement or a delete. On the side after an update or a replacement it is taken from the old outputs
  only when neither side's inputs set it, so the two sides do not differ by a default only one of them names. A
  value hidden in the inputs is never filled, and an unknown or secret output is named under `unknown`, not sent.

## [0.3.1] - 2026-11-21

The first release since 0.1.0. 0.2.0 was merged on 2026-11-18 but never released, and 0.3.0 was tagged on 2026-11-21
but its release run failed before it published anything, so their changes are in this list too.

### Added

- `qloudpulse pulumi preview` and `qloudpulse pulumi up` run Pulumi with your own arguments and print the estimate of
  the change: after the diff for `preview`, before the update and its prompt for `up`.
- The pull request comment gives the whole stack's monthly cost beside what the change adds: "about $A a month now,
  about $B after this change". The client sends the resources the change leaves the same, of priced types, as
  `"change": "same"`, from `pulumi preview --json --show-sames`; without `--show-sames` the comment says to add it.
- The catalogue of priced types and their attributes comes from qloudpulse (`POST .../estimates/catalogue`, open,
  no token): the client sends the preview's type tokens and gets back what to send, so qloudpulse prices a new type
  without a new client. `--catalogue <file>` reads a saved answer instead.
- References between resources, such as an ECS service's `taskDefinition`, so qloudpulse prices the two together.
- The comment links to the estimate's page in qloudpulse ("See it in qloudpulse") when the answer carries one. The
  resources' names go after `#` in the link, so they reach only the browser of whoever opens it, never a server. The
  request names the stack, `"stack": "project/stack"`, from the preview's URNs.
- Types AWS charges nothing for, such as IAM roles, a VPC's parts, ECS clusters and S3 bucket settings, are listed as
  "no charge of their own" instead of "not priced", and are neither sent nor counted as not priced. A change of only
  such types says it adds no monthly cost.
- `qloudpulse login` signs in with your browser: it shows a one-time code, opens qloudpulse's page, and once you
  approve there it keeps a token of your own in the macOS Keychain, the Windows Credential Manager or Linux's Secret
  Service. Without a keychain it keeps it in a file only you can read, `~/.config/qloudpulse/credentials`, or the file
  `QLOUDPULSE_CREDENTIALS_FILE` names. Runs use the kept token when `QLOUDPULSE_TOKEN` is unset; `QLOUDPULSE_TOKEN`
  still wins, so CI is unchanged. It needs qloudpulse's report service with ADR 0135 (report migration V33).
- `qloudpulse logout` removes the kept token from the machine; it still works until you remove it under Your tokens.
- A run at a terminal with no token asks "Sign in with your browser? [Y/n]": yes signs in and keeps the token, no
  asks for a token to paste, without echo, and uses it for that run only. In CI, with no terminal, it stops with the
  error as before.
- `--format text`: plain text with the table in columns, the default when stdout is a terminal. Piped or redirected,
  the output is still Markdown, so `> comment.md` is unchanged. `--format markdown` asks for it by name.
- The client says on stderr and under the comment when a newer client is out, and stops before sending anything when
  it is older than the oldest qloudpulse takes.
- Release builds for Windows: x86_64, `qloudpulse-<version>-x86_64-pc-windows-gnu.zip`, and Windows on ARM,
  `qloudpulse-<version>-aarch64-pc-windows-gnullvm.zip`. The `.exe` files are run on Windows x64 and Windows on ARM
  before a release.
- `install.sh` and `install.ps1` in every release: `curl -fsSL <release>/install.sh | sh` on Linux and macOS,
  `irm <release>/install.ps1 | iex` on Windows. Each picks the build for the machine (Windows on ARM included), checks
  it against `SHA256SUMS`, installs the release it came with, and on a GitHub Actions runner puts it on the job's
  PATH. Both are listed in `SHA256SUMS` and attested with the archives.
- Releases are built on GitHub's runners by `.github/workflows/cli-release.yml`, never on a laptop, and every archive
  has signed SLSA build provenance: `gh attestation verify <archive> --repo ulfmansson/qloudpulse-public` checks it
  was built from this repository's tag. The signed bundle is in the release too, for `cosign` offline.
- A CycloneDX SBOM, `qloudpulse-<version>.cdx.json`, in every release, signed for the same archives.
- Every pull request runs `cargo test`, `cargo clippy`, `cargo deny` (advisories, licences, sources) and
  `cargo audit`, and they run weekly on `main`.
- Every help text says how to give the token. The README says how to verify what you run, build it from a tag, and
  run it in CI without cloud credentials.

### Changed

- The default address is `https://api.qloudpulse.com/v1/estimates`, qloudpulse's host for machines. 0.1.0's
  `https://app.qloudpulse.com/api/report/public/estimates` keeps answering.
- `estimate` needs a token on every run, also one whose preview holds nothing to price, and stops with an error
  before it reads the preview. `--dry-run` still needs none.
- The missing-token error links your own token page on hosted qloudpulse,
  [app.qloudpulse.com/ui/?profile#my-tokens](https://app.qloudpulse.com/ui/?profile#my-tokens): anyone in the
  company, a viewer too, makes a token there. An admin's event-source token still works. It, and the error for a
  refused token (401), name `qloudpulse login`.
- `release.sh --publish` no longer builds or uploads anything: it checks the commit may be released and pushes the
  tag, which starts the release workflow. The build images are pinned by digest, and the workflow's actions by
  commit.

### Fixed

- A preview without `--show-sames` no longer says "none of its resources could be priced" for the stack: Pulumi lists
  the stack and its providers as unchanged even then, and only an unchanged cloud resource now counts.
- Names and notes in the comment lose control characters, so nothing from the preview or the answer can move a
  terminal's cursor.
- `release.sh` packs its tarballs with GNU tar as well as bsdtar, and its archives carry no macOS extended
  attributes.
- `release.sh`'s version and path checks no longer read a match as none (`grep -q` under `pipefail`).

### Security

- `time` 0.3.45 is updated to 0.3.55 in `Cargo.lock` (RUSTSEC-2026-0009). It comes only with ureq's `cookies`
  feature, which the client does not enable, so no released binary held it.

## [0.1.0] - 2026-11-17

### Added

- `qloudpulse estimate` reads `pulumi preview --json` and prints a pull request comment with what the change will
  cost a month, priced at the company's own rates. It sends only the attributes that set each resource's price, a
  hashed key per resource, and the region: no names, tags, config, outputs or secrets.
- `--dry-run` prints the request and sends nothing; `--region`, `--with-resource-ids`, `--format json` and
  `--endpoint`.
- Static binaries for Linux x86_64 and arm64, and binaries for macOS on both architectures, with `SHA256SUMS`.

[Unreleased]: https://github.com/ulfmansson/qloudpulse-public/compare/qloudpulse-stack-0.6.0...main
[0.6.0]: https://github.com/ulfmansson/qloudpulse-public/compare/qloudpulse-stack-0.5.0...qloudpulse-stack-0.6.0
[0.5.0]: https://github.com/ulfmansson/qloudpulse-public/compare/qloudpulse-cli-0.3.1...qloudpulse-stack-0.5.0
[0.3.1]: https://github.com/ulfmansson/qloudpulse-public/compare/qloudpulse-cli-0.1.0...qloudpulse-cli-0.3.1
[0.1.0]: https://github.com/ulfmansson/qloudpulse-public/releases/tag/qloudpulse-cli-0.1.0

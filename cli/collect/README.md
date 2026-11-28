# qloudpulse-collect

`qloudpulse-collect` reads your AWS accounts with your own credentials, records what qloudpulse's collectors would
read, applies your redaction rules, and writes a `.tgz` bundle. You can open the bundle and check it before
qloudpulse ingests it. Nothing is read from qloudpulse, and you decide whether and how bundles reach it.

The design and its reasons are in [`docs/offline-collection.md`](../../docs/offline-collection.md). This first version
covers qloudpulse's hourly AWS cost collector.

## Install

Each release has a static binary for Linux (x86_64 and arm64), binaries for macOS, and binaries for Windows (x86_64
and arm64), with their SHA-256 sums, and an install script that picks the one for your machine:

```bash
curl -fsSL https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-collect-0.1.1/install.sh | sh
```

```powershell
irm https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-collect-0.1.1/install.ps1 | iex
```

Each script installs the release it came from: the version is in the URL, so a machine gets the same client every
run until you change it. The copies in this repository, `cli/collect/install.sh` and `cli/collect/install.ps1`,
install the newest release instead, or the one `--version` names. When `qloudpulse-collect` is already installed, in
that directory (or, without `--dir`, anywhere on PATH), it says which version that is and which it would install, and asks before replacing it;
one already at that version is left alone. `--version`, `--yes`, or no terminal to ask on goes ahead without asking.
It downloads the archive and the release's
`SHA256SUMS`, stops unless the sum matches, and puts the binary in `~/.local/bin`, or on Windows in
`%LOCALAPPDATA%\Programs\qloudpulse`, which it adds to your PATH.

| `install.sh` | `install.ps1` (or its variable, as `iex` takes no options) | |
|---|---|---|
| `--version <version>` | `-Version`, `$env:QLOUDPULSE_COLLECT_VERSION` | the release to install |
| `--dir <dir>` | `-Dir`, `$env:QLOUDPULSE_INSTALL_DIR` | where the binary goes |
| `--yes` | `-Yes`, `$env:QLOUDPULSE_ASSUME_YES=1` | replace an installed version without asking |
| `--attest` | `-Attest` | also check its signed build provenance with `gh`, as below |
| `--base <url or dir>` | `-Base`, `$env:QLOUDPULSE_COLLECT_DOWNLOAD_BASE` | a mirror of the release's files |
| | `-NoModifyPath`, `$env:QLOUDPULSE_NO_MODIFY_PATH` | leave PATH alone |

Pass `install.sh` options after `sh -s --`, e.g. `curl -fsSL <url> | sh -s -- --dir /usr/local/bin --attest`. The
scripts are in `SHA256SUMS` too, and attested with the archives, so you can check one before running it.

Or build it from this directory with Rust 1.88 or later: `cargo build --release --locked`. The binary is
`target/release/qloudpulse-collect`. The checksum shows the download is whole; that it was built from this
repository's source, by its public workflow, is checked as in [Verify what you run](#verify-what-you-run).

### Updates

```bash
qloudpulse-collect update
```

`update` asks GitHub for the newest `qloudpulse-collect` release, downloads that release's `install.sh`
(`install.ps1` on Windows), checks the script against the release's `SHA256SUMS`, and runs it for the directory the
running binary is in. The script then checks the archive as it does on a first install. `--check` only says whether a
newer release is out, `--version <version>` installs that release instead, newer or older, and `--attest` also checks
the archive's signed provenance with `gh`.

When you run `init`, `analyze`, `verify`, `policy`, `call-plan`, `rotate-key`, `login` or `logout` at a terminal, the
client also asks GitHub for the newest release, at most once a day, and says on stderr when one is out. `ui` asks
too, and shows the line at the top of every page. Nothing else asks: `collect`, `pack`, `deliver`, `upload` and `run`,
which are what unattended jobs run, never contact GitHub, and neither does a run with `--log-json` or with
`QLOUDPULSE_NO_UPDATE_CHECK=1`. What the last check found is kept in `~/.cache/qloudpulse/qloudpulse-collect-update.json`
(`$XDG_CACHE_HOME`, or `%LOCALAPPDATA%\qloudpulse` on Windows). A machine that cannot reach GitHub waits at most three
seconds once a day, and nothing is said.

The [build without the upload](#a-build-that-cannot-contact-qloudpulse) has no `update` and never asks GitHub: the
release it would install has the upload in it. Build the new tag the same way instead.

## Get started

```bash
qloudpulse-collect init
```

`init` writes `qloudpulse-collect.toml`, a pseudonym key, `qloudpulse-collect.key`, and a view token,
`qloudpulse-collect.view-token` (see [Encrypted values](#encrypted-values-and-the-view-token)), all in
`~/.config/qloudpulse` (`$XDG_CONFIG_HOME/qloudpulse` if that is set, `%APPDATA%\qloudpulse` on Windows), the directory
the qloudpulse CLI keeps its sign-in in. `--config <file>` or `QLOUDPULSE_COLLECT_CONFIG` names another config file; a
`qloudpulse-collect.toml` in the current directory, as earlier releases wrote it, is used before the default. The key
and the view token sit beside the config file. What the client writes as it runs goes to the state directory, not the
config directory: raw snapshots to `~/.local/state/qloudpulse/collect/<source_id>/work`, bundles and their delivery
state to `.../bundles` (`$XDG_STATE_HOME` if set, `%LOCALAPPDATA%` on Windows), unless the config sets `work_dir` or
`output_dir`. A bundle is deleted once every `[deliver]` target has it, or once `upload` sent it (`[run]
delete_delivered = false` keeps it). `qloudpulse-collect --help` lists where each file is with the config it would
use. Back the key and the view token up: losing
the key changes every pseudonym, which breaks cost history at that point, and without the view token nobody can read
encrypted values again. Then set the role to read through:

```bash
qloudpulse-collect policy > policy.json
```

That command prints the read-only IAM policy the client needs, including the read actions Cloud Control uses on your
behalf for the settings of queues, topics, file systems, ECS, streams, keys, load balancers, NAT gateways, VPC
endpoints, transit gateways and hosted zones. Every action in it comes from the compiled-in call
plan (`qloudpulse-collect call-plan`, source [`call-plan.json`](call-plan.json)). The client refuses any operation
that is not in the call plan, before it builds the request.

## Look before anything leaves

```bash
qloudpulse-collect collect
```

```bash
qloudpulse-collect analyze
```

```bash
qloudpulse-collect analyze --draft-rules
```

- `collect` writes a **raw snapshot** to `work_dir`. It is local only and never delivered.
- `analyze` lists every tag key with counts, sample values, what your rules do to it, and what each rule costs in
  qloudpulse's reports.
- `--draft-rules` prints a `[tags]` section to paste into the config.

```bash
qloudpulse-collect pack
```

```bash
qloudpulse-collect analyze ~/.local/state/qloudpulse/collect/main/bundles/qloudpulse-main-20261121T0600Z.tgz
```

```bash
qloudpulse-collect verify ~/.local/state/qloudpulse/collect/main/bundles/qloudpulse-main-20261121T0600Z.tgz
```

- `pack` applies the rules and writes the bundle, and prints its path.
- `analyze <bundle>` shows exactly what leaves.
- `verify` checks every file in the bundle against its manifest.

A bundle is plain text, so `tar -xzf` and `jq` work on it:
- `manifest.json` holds the window, the accounts and regions read, a SHA-256 for every file, the hash of your
  config, a summary of your rules, what each rule changed, and your key's fingerprint.
- `calls/*.ndjson` holds one recorded AWS answer per line, after redaction.

## The bill

With `[cur]` set, the run at `daily_hour` also reads the bill: the same Athena SQL qloudpulse runs, over your CUR 2.0
table in the payer account, for the previous and the current month (`periods_back`). Only the query results are
recorded, never the CUR files: about one row per day, account, product, resource and usage type.

The role needs, besides what `policy` prints:
- Athena on the workgroup (`qloudpulse-cost` unless you name another),
- Glue `GetDatabase` and `GetTable` on the database and table (`policy` prints these),
- read on the CUR export bucket, and write on the workgroup's results location.

`scope_tag_keys` must be the scope tag keys you set in qloudpulse: they are columns in the query, and qloudpulse's
replay matches the query text. The bill's tag columns (`Name`, the EKS cluster tags and your scope keys) follow your
tag rules like tags anywhere else; a dropped value is left empty. Use `collect --daily` to read the bill outside the
daily hour.

## Forecast and carbon

- **Forecast** (`forecast = true` under `[cur]`, off by default): Cost Explorer in the payer account, this month's
  and last month's spend per linked account, then AWS's forecast to the end of the month for each. AWS bills $0.01
  per request, which is why it is opt-in. It asks for the 80% prediction interval, the one qloudpulse's collector uses.
- **Carbon** (`carbon = true`, the default): AWS's monthly carbon estimate of each account since 2022, by region and
  service, once a day. The role needs `sustainability:GetEstimatedCarbonEmissions`; an account without it is
  recorded as refused.

## Rules

Under `[tags]`, a tag no rule names is dropped (`default = "deny"`) or kept (`"allow"`). Each tag key can have one
action:
- `keep`
- `drop`
- `pseudonymize`: the same value always gives the same `p_…`, and only your key can reverse it
- `encrypt`: the same value always gives the same `qpe1_…`, which anyone with your view token can read in
  qloudpulse's web UI, in their own browser (see [Encrypted values](#encrypted-values-and-the-view-token))
- `replace`: a regex with `$1`, or a fixed value

`[identifiers]` pseudonymizes account IDs and resource IDs while keeping their format: an account ID becomes another
12-digit number, `i-0abc…` becomes `i-` and as many other hex digits. The same ID gets the same pseudonym everywhere,
inside ARNs and requests too, so the bill and the inventory still join. Both are kept by default.

`[[fields]]` rules apply the same actions to any field, matched by the end of its path. The client refuses rules
for fields qloudpulse prices from, such as instance type, size or engine. When a rule changes a value, for example a
table name, the change carries into the later requests that name it. Nothing leaks through a request, and
qloudpulse's replay still matches.

## Encrypted values and the view token

`pseudonymize` hides a value from qloudpulse and from everyone who reads your reports. `encrypt` hides it from
qloudpulse only: the people you give the **view token** still see the real value in qloudpulse's reports, decrypted
in their browser.

```toml
[tags]
encrypt = ["Owner", "Name"]

[[fields]]
path = "TableNames"
action = "encrypt"
```

```bash
qloudpulse-collect view-token
```

- **The token** is `qp_` and 43 characters, 32 random bytes that `init` writes to `view_token_file`
  (`qloudpulse-collect.view-token`, readable by its owner only). `view-token` prints it on stdout, for a password
  manager; `view-token --create` makes one for a config from before `encrypt`, and refuses to replace one.
  `view-token --key-id` prints only the eight hex digits that name it.
- **What leaves** is `qpe1_<key id>_<data>`: AES-256-GCM under a key derived from the token. The same value always
  gives the same output, across bundles, so costs still group by it and a table name carried into a later request
  still matches on replay. Like a pseudonym, that shows which resources share a value and nothing more; values are
  padded to 16 bytes, so short values do not give away their length. The manifest carries the key id,
  `view_key_id`, never the token.
- **Reading it**: enter the token where qloudpulse asks for one. The page derives the keys with WebCrypto and
  decrypts the values in your browser; the token and the real values are never sent to qloudpulse or stored by it.
  That page's code is [`web/qloudpulse-values.js`](web/qloudpulse-values.js), tested against values this client
  encrypted ([`web/test-vectors.json`](web/test-vectors.json), kept current by `cargo test`).
- **Anyone holding the token reads every value it encrypted.** Share it the way you share a password. Losing it
  makes those values unreadable for good: qloudpulse cannot recover them. `pack` refuses a bundle that holds a view
  token, so one pasted into a tag does not leave.
- qloudpulse's own features see only the encrypted value: rules in qloudpulse that match a tag's value, such as an
  environment rule on `Name`, cannot match it. Derive what they need first (`[environment]` below), or keep that tag.

## Privacy extras

- **Scan before packing.** What would leave is scanned, after your rules, for AWS keys, private keys, tokens,
  passwords and email addresses. `pack` stops while any is left, naming the kind, the count and a masked sample,
  until a rule redacts it, `[scan] allow` lets it through, or you pass `--allow-findings`. Public IP addresses are
  reported but never block. In `run`, a stopped pack keeps its raw snapshot so you can look.
- **Environment before redaction** (`[environment]`): adds the tag `qloudpulse:environment` from a tag you name, or
  from rules on the `Name` tag, before any rule runs. It is always kept, so `Name` can be pseudonymized and costs
  still group by environment.
- **`rotate-key`** replaces the pseudonym key. With `keep_pseudonym_map = true` it writes a file pairing each old
  pseudonym with its new one, without real values, which qloudpulse can use to relink history. Without the map
  it refuses unless `--force`.
- **Encryption** (`[encrypt] public_key_file`): each bundle is also written as `.tgz.jwe` (ECDH-ES+A256KW, A256GCM)
  to qloudpulse's public key for bundles, and that is what is delivered. The plaintext stays here for you to
  inspect; the JWE's header carries its SHA-256.

## Every hour

```bash
qloudpulse-collect run
```

`run` collects at 5 past each hour, packs, deletes the raw snapshot, delivers, and deletes each bundle once every
delivery target has it (`[run] delete_delivered = false` keeps the newest `keep_bundles` instead). It never deletes a
bundle a delivery target is still missing; a failed delivery is retried at the next run. Use `run --once` for cron, a systemd timer or a Kubernetes CronJob.

## The local UI

```bash
qloudpulse-collect ui
```

```bash
qloudpulse-collect run --ui
```

- `ui` serves a web page on `127.0.0.1:7781` and prints its address with a session token. Open that address; the
  token is new each time. `run --ui` does the same next to the hourly loop, and shows each cycle as it happens.
- **Analyze**: every tag key with its values and what your rules do with it, what the rules cost in qloudpulse, the
  scan before and after your rules, errors and calls by operation. **Collect sample now** reads your accounts into
  memory only; a raw snapshot that `collect` left on disk can be loaded instead.
- **Rules**: edit `[tags]` and `[[fields]]` with the raw and redacted answer side by side, run through the same
  redactor as `pack`. Saving goes through the same checks as the CLI, rewrites only what changed (your comments
  stay), and refuses if the file changed on disk meanwhile. Under `run`, a saved rule applies from the next cycle.
- **Bundles**: every bundle, which target has it, its manifest checked against its files, and every record as it
  left. **Run history**: each bundle's window, failures and deliveries. Once qloudpulse has a bundle, its
  `qloudpulse ✓` links to **Bundles arrived** on qloudpulse (`/account/aws#aws-offline`), which shows each upload by
  reference and what became of it. The link opens in a new tab; the UI itself still loads nothing from qloudpulse.
- It answers only to requests that name it by IP address, as `localhost`, or by a name you give with
  `--ui-allow-host` (other DNS names are refused, against DNS rebinding), and that carry the token; every form carries
  a second token. Every asset is in the binary and the page may load nothing from anywhere else.
  `--ui-bind 0.0.0.0:7781` lets other machines reach it; they still need the token.
- **Download** gives the whole config file with the rules on the page, without saving it. When the config is on a
  file system the UI cannot write, such as a Kubernetes ConfigMap, that replaces **Save**.

### In Kubernetes

Run it as a Deployment with `run --every 1h --ui` (a CronJob with `run --once` exits before you could look), and
reach it through the API server, which needs no Service and leaves the default `127.0.0.1` address as it is:

```bash
kubectl -n qloudpulse logs deploy/qloudpulse-collect | grep 'token='
```

```bash
kubectl -n qloudpulse port-forward deploy/qloudpulse-collect 8080:7781
```

Then open the printed address with your local port, `http://localhost:8080/?token=…`. The token is new each time the
pod starts.

- **A Service or an Ingress**: start with `--ui-bind 0.0.0.0:7781 --ui-allow-host collect.example.com` (one
  `--ui-allow-host` per name the browser will use, such as the Ingress host or `qloudpulse-collect.qloudpulse.svc`).
  Put TLS in front of it: the token and the session cookie travel in every request.
- **The rules editor on a ConfigMap**: the mount is read-only, so the editor offers the file to download or copy. Put
  it back into the ConfigMap; under `run`, the next cycle after Kubernetes refreshes the mount uses it (a `subPath`
  mount never refreshes: restart the pod).

## Delivery

Each delivery target is opt-in under `[deliver]`:

| Target | What it does |
|---|---|
| `[deliver.s3]` | `PutObject` into **your own** bucket, with optional SSE-KMS. This is the only write the client makes to AWS. `policy` adds a statement for that one bucket. |
| `[deliver.qloudpulse]`, or `qloudpulse-collect upload <bundle>` | HTTPS `POST` of the bundle, signed in exactly as the qloudpulse CLI is: `QLOUDPULSE_TOKEN` (a token from Your tokens, for unattended runs), else the token `qloudpulse login` or `qloudpulse-collect login` kept on this machine (the two share it), else, at a terminal, a sign-in with the browser. qloudpulse keeps only the newest bundle per source. |
| `[deliver.command]` | Runs your command with `{bundle}` replaced by the bundle's path. |

## A build that cannot contact qloudpulse

```bash
cargo build --release --no-default-features
```

This build leaves out the `upload` feature, so there is no qloudpulse code and no qloudpulse host in the binary.
CI checks that with `strings`.

## Verify what you run

The client reads your AWS accounts and holds the key behind your pseudonyms, so check what you run instead of
trusting it. Each step below
needs only public things: this repository, the release, and Sigstore's public log.

**1. The archive was built from this repository, by its release workflow, at the release's tag.** Every archive of a
release has signed [SLSA build provenance](https://slsa.dev/spec/v1.0/provenance), made by
[`.github/workflows/cli-collect-release.yml`](../../.github/workflows/cli-collect-release.yml) on GitHub's runners and recorded in
Sigstore's public transparency log. Nothing built on a laptop can carry it. With the [GitHub CLI](https://cli.github.com)
(signed in to any GitHub account):

```bash
VERSION=0.1.1
TARGET=x86_64-unknown-linux-musl
gh attestation verify "qloudpulse-collect-$VERSION-$TARGET.tar.gz" --repo ulfmansson/qloudpulse-public \
  --signer-workflow ulfmansson/qloudpulse-public/.github/workflows/cli-collect-release.yml \
  --source-ref "refs/tags/qloudpulse-collect-$VERSION"
```

`--repo` alone checks that the archive was attested by a workflow of this repository. `--signer-workflow` and
`--source-ref` also check that it was the release workflow, run on the release's tag and not on a branch. The commit
it was built from is in the signing certificate, which the workflow cannot write; add
`--format json --jq '.[].verificationResult.signature.certificate.sourceRepositoryDigest'` to print it, and compare it
with `git rev-parse "qloudpulse-collect-$VERSION^{commit}"` in a clone whose source you have read.

The binary inside each archive is attested too, so the same check works on a client
you have already installed, wherever it came from:

```bash
gh attestation verify "$(command -v qloudpulse-collect)" --repo ulfmansson/qloudpulse-public \
  --signer-workflow ulfmansson/qloudpulse-public/.github/workflows/cli-collect-release.yml
```

`qloudpulse-collect --version` names the commit a release was built from and that commit's date, as `rustc --version` does:
`qloudpulse-collect <version> (1ab39062b 2026-11-21)`. That line is a label, written in at build time; the provenance is what
proves it. A build of your own with plain `cargo build` prints the version alone, and `release.sh` marks one built
from a changed tree, as `(1ab39062b-modified 2026-11-21)`.

Without the GitHub CLI, or offline, use Sigstore's own [cosign](https://docs.sigstore.dev/cosign/) (3.0 or later)
with the bundle each release carries:

```bash
cosign verify-blob-attestation \
  --bundle "qloudpulse-collect-$VERSION.provenance.sigstore.json" \
  --type slsaprovenance1 \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity "https://github.com/ulfmansson/qloudpulse-public/.github/workflows/cli-collect-release.yml@refs/tags/qloudpulse-collect-$VERSION" \
  "qloudpulse-collect-$VERSION-$TARGET.tar.gz"
```

The release's SBOM, `qloudpulse-collect-$VERSION.cdx.json` (CycloneDX), lists every crate the client is built from, and it is
signed for the same archives: `gh attestation verify … --predicate-type https://cyclonedx.org/bom`. Each binary also
carries that list itself, written in by [cargo auditable](https://github.com/rust-secure-code/cargo-auditable), so an
installed client can be checked for crates with known vulnerabilities without the SBOM: `cargo audit bin
"$(command -v qloudpulse-collect)"`, or Trivy or Grype, which read the same list.

**2. The download is whole.** `SHA256SUMS` has the SHA-256 of every archive, as in [Install](#install), and of the
binary in each, named `<archive>/qloudpulse-collect` (or `qloudpulse-collect.exe`), so an installed binary can be checked by its sum
too: `shasum -a 256 "$(command -v qloudpulse-collect)"`.

```bash
sha256sum -c --ignore-missing SHA256SUMS   # on macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
```

The sums prove the file is the one uploaded, not who built it: whoever can upload an archive can upload its sum. The
provenance above proves who built it. In a pipeline, verify a version once when you adopt it, then pin its SHA-256
there, so every run checks the same bytes:

```bash
echo "<sha256 from SHA256SUMS>  qloudpulse-collect-$VERSION-$TARGET.tar.gz" | sha256sum -c -
```

**3. Or build it yourself from the tag, and get the same bytes.** The builds are
[reproducible](https://reproducible-builds.org): built again from the release's tag, with the images and the Rust that
`release.sh` pins, a binary comes out byte for byte the one the release holds. Nothing in it depends on when or where
it was built: no timestamps (the PE header has none, and archives carry the commit's time), and no paths of the
machine. The release workflow itself builds every target twice, on two runners, and signs and publishes nothing unless
the two agree. To check a release yourself, from a clone, with Docker for the Linux and Windows targets:

```bash
git switch --detach "qloudpulse-collect-$VERSION"
cli/collect/release.sh --reproduce --targets x86_64-unknown-linux-musl
```

It builds the target as the release workflow does and compares the binary with the release's `SHA256SUMS`. Two inputs are not pinned by digest. The Linux builds install Alpine's
`musl-dev` when they run, as Alpine keeps only the newest build of a package; the version a build used is printed
beside it. And a macOS binary is linked by the Mac's own Xcode tools, so it matches only on a Mac with the Xcode of
GitHub's `macos-15` image.

To build one for yourself instead, with Rust 1.88 or later (`--locked` builds exactly the crate versions, with their
checksums, in the tag's `Cargo.lock`; its `--version` names no commit):

```bash
cargo install --git https://github.com/ulfmansson/qloudpulse-public --tag "qloudpulse-collect-$VERSION" --locked qloudpulse-collect
```

Every pull request to this repository runs `cargo test`, `cargo clippy`, `cargo deny` (RustSec advisories, the
licences in [`deny.toml`](deny.toml), crates.io as the only source) and `cargo audit`, in
[`.github/workflows/cli-collect-checks.yml`](../../.github/workflows/cli-collect-checks.yml); the same checks run weekly on `main`.

**4. Run it where it can do no harm.** The client only reads: every AWS operation it may call is in the compiled-in
call plan (`qloudpulse-collect call-plan`), it refuses any other before building the request, and `qloudpulse-collect
policy` prints the read-only IAM policy for exactly those. Its only write is the optional `PutObject` to your own S3
bucket. Give the role nothing more, and limit the machine's egress to AWS's endpoints, plus `api.qloudpulse.com:443`
only if it uploads. The check for a newer release asks `api.github.com:443` (see [Updates](#updates)); blocking it
only means nothing is said.

To be sure it cannot reach qloudpulse at all, build it without the upload:

```bash
cargo install --git https://github.com/ulfmansson/qloudpulse-public --tag "qloudpulse-collect-$VERSION" --locked \
  --no-default-features qloudpulse-collect
```

That binary holds no qloudpulse code and no qloudpulse host; CI checks it on every pull request with `strings`.

**5. Read what leaves.** A bundle is plain text: `qloudpulse-collect verify <bundle>` checks every file against its
manifest, `qloudpulse-collect analyze <bundle>` shows every tag and what the rules did, and `tar -xzf` with `jq` shows
the rest. `pack` refuses to write a bundle that still holds keys, tokens, passwords or email addresses (see
[Privacy extras](#privacy-extras)).

## Releasing

Releases are built on GitHub's runners by [`.github/workflows/cli-collect-release.yml`](../../.github/workflows/cli-collect-release.yml),
never on a laptop, so that each archive's provenance names a public workflow and commit. To release, raise `version`
in `Cargo.toml`, and in [CHANGELOG.md](CHANGELOG.md) move the lines under Unreleased into that version's section and
date it (`## [<version>] - YYYY-MM-DD`). That section becomes the release's notes on GitHub. Merge, and from an
up-to-date `main`:

```bash
cli/collect/release.sh --publish
```

That builds nothing. It checks that `HEAD` is `origin/main`, that the changelog has the version's section, dated,
and nothing left under Unreleased, and that the version is not released yet. Then it
pushes the tag `qloudpulse-collect-<version>`. The tag starts the workflow, which checks the same again, runs
`cargo test --locked`, builds every target with `release.sh`, runs the two Windows `.exe` files on Windows x86_64 and
Windows on ARM, builds every target a second time on another runner and checks that the binaries are the same, writes
the SBOM and `SHA256SUMS`, signs provenance for every archive and binary, and publishes the release. Watch
it with `gh run list -R ulfmansson/qloudpulse-public --workflow cli-collect-release.yml`.

The workflow also builds every target, without signing or publishing, on each pull request that changes `cli/collect/` or `cli/stack/`.
Every action in it is pinned by commit, and every build image (in `release.sh`) by digest.

`release.sh` without `--publish` builds every target here and packs the same files, for trying a build; nothing leaves
your machine, and nothing it builds is released:

```bash
cli/collect/release.sh
```

It needs Docker (the Linux and Windows builds) and rustup (the macOS builds). The Windows `.exe` files are cross-built,
the x86_64 one with mingw-w64 and the arm64 one with [llvm-mingw](https://github.com/mstorsjo/llvm-mingw) (Rust's
`aarch64-pc-windows-gnullvm` target), and cannot run on the Mac that builds them: the script checks that each is a
Windows program for its architecture that needs only DLLs every Windows has, and that it names its version.

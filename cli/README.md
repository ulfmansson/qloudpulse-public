# qloudpulse CLI

`qloudpulse estimate` tells you what an infrastructure change will cost before you apply it, priced at your company's
own rates, and prints a comment for the pull request. This first version reads Pulumi previews. Terraform and OpenTofu
plans come next. `qloudpulse pulumi preview` and `qloudpulse pulumi up` run Pulumi for you and price the change on the
way, so every preview and update on your own machine shows its cost.

It sends the attributes that set each resource's price to qloudpulse (`POST https://api.qloudpulse.com/v1/estimates`). Names,
tags, config and secrets stay in your CI. [What it sends](#what-it-sends) lists exactly what goes.

Which resource types are priced, and which of their attributes are sent, is qloudpulse's **catalogue**. On every
run the client sends qloudpulse the resource types it found in the preview, type tokens only, to
`POST https://api.qloudpulse.com/v1/estimates/catalogue` (open, no token), and gets back what to send for those qloudpulse
prices. So when qloudpulse prices a new type, the client you already have sends it.

What changed in each version: [CHANGELOG.md](CHANGELOG.md).

## Install

Each release has a static binary for Linux (x86_64 and arm64), binaries for macOS, and binaries for Windows (x86_64
and arm64), with their SHA-256 sums:

```bash
VERSION=0.2.0
TARGET=x86_64-unknown-linux-musl   # aarch64-unknown-linux-musl, aarch64-apple-darwin, x86_64-apple-darwin
BASE=https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-cli-$VERSION
curl -fsSLO "$BASE/qloudpulse-$VERSION-$TARGET.tar.gz"
curl -fsSLO "$BASE/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS   # on macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
tar -xzf "qloudpulse-$VERSION-$TARGET.tar.gz"
./qloudpulse --version
```

On Windows, in PowerShell:

```powershell
$Version = "0.2.0"
$Base = "https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-cli-$Version"
$Zip = "qloudpulse-$Version-x86_64-pc-windows-gnu.zip"   # on Windows on ARM: aarch64-pc-windows-gnullvm
Invoke-WebRequest "$Base/$Zip" -OutFile $Zip
Invoke-WebRequest "$Base/SHA256SUMS" -OutFile SHA256SUMS
$Want = (Select-String -Path SHA256SUMS -Pattern ([regex]::Escape($Zip))).Line.Split(" ")[0]
if ((Get-FileHash $Zip -Algorithm SHA256).Hash -ne $Want.ToUpper()) { throw "checksum mismatch" }
Expand-Archive $Zip -DestinationPath .
.\qloudpulse.exe --version
```

The token goes in the same variable: `$env:QLOUDPULSE_TOKEN`, set from a secret in the pipeline. On a GitHub
`windows-latest` runner the bash steps below work as they are, in Git Bash.

Or build it from this directory with Rust 1.88 or later: `cargo build --release --locked`. The binary is
`target/release/qloudpulse`.

The checksum shows the download is whole. That it was built from this repository's source, by its public workflow,
is checked as in [Verify what you run](#verify-what-you-run).

### Updates

The catalogue also names the newest client and the oldest one qloudpulse still takes. When a newer one is out, the
client says so on stderr and under the pull request comment, and carries on. Below the oldest, it stops before sending
anything and says which version to download; qloudpulse refuses such a client's request too, by its `User-Agent`. With
`--catalogue`, a saved answer, the client goes by the versions in it; qloudpulse's refusal still holds.

## Use

```bash
pulumi preview --json --show-sames -s payments-prod | qloudpulse estimate > comment.md
```

Or let the client run Pulumi:

```bash
qloudpulse pulumi preview -s payments-prod
```

```bash
qloudpulse pulumi up -s payments-prod
```

| Command | |
|---|---|
| `qloudpulse estimate [FILE]` | Prices the JSON of `pulumi preview --json`, from `FILE` or stdin, and prints the comment |
| `qloudpulse pulumi preview [PULUMI_ARGS...]` | Runs `pulumi preview` with your arguments, its usual diff on your terminal, then `pulumi preview --json --show-sames` with the same arguments, and prints the estimate after the diff. Pulumi prints its diff or its JSON, never both, so the stack is previewed twice. A preview that fails ends with Pulumi's exit code; an estimate that fails exits 1, after the diff |
| `qloudpulse pulumi up [PULUMI_ARGS...]` | Runs `pulumi preview --json --show-sames` with your arguments, less those only `up` takes (`-y`/`--yes`, `-f`/`--skip-preview`, `--continue-on-error`, `--strict`, `--plan`, `--secrets-provider`, `--expect-no-changes`), prints the estimate, then runs `pulumi up` with your arguments as they are, on your terminal: its own preview, diff and "Do you want to perform this update?" as usual, so the stack is previewed twice. Ends with Pulumi's exit code. With `--yes` the estimate is printed and the update goes ahead without a pause; with `--skip-preview` nothing is priced. An estimate that fails (no token, the network, a refused version) is one line on stderr, and the update goes on |

| Option | |
|---|---|
| `FILE` | `estimate` only. The JSON of `pulumi preview --json`. Without it, or with `-`, stdin |
| `--dry-run` | Print the request that would be sent, and send nothing. The only run that needs no token |
| `--region <region>` | The region of resources whose provider and stack name none |
| `--with-resource-ids` | Send the cloud ID of each resource that exists before the change, so its usage comes from what it really used |
| `--format <markdown\|text\|json>` | The comment in Markdown, for a pull request, or as plain text, or qloudpulse's answer as JSON. Without it: text when stdout is a terminal, Markdown when it is piped or redirected, as in `> comment.md` |
| `--endpoint <url>` | Another address, such as your own qloudpulse install, also `QLOUDPULSE_ENDPOINT`. https only; http only to localhost. The types are asked about at this address and `/catalogue` |
| `--catalogue <file>` | Read the answer from a file instead of asking, also `QLOUDPULSE_CATALOGUE`, to pin what is sent. `qloudpulse estimate --dry-run` shows the types; `curl -fsS -H 'Content-Type: application/json' -d '{"formatVersion": 1, "types": ["aws:rds/instance:Instance"]}' https://api.qloudpulse.com/v1/estimates/catalogue > catalogue.json` saves an answer |
| `--require-estimate` | `pulumi up` only. Stop, and do not run `pulumi up`, when the change cannot be priced |

With `qloudpulse pulumi preview` and `qloudpulse pulumi up`, qloudpulse's options come first. From the first word it
does not know, everything goes to Pulumi as it is: `qloudpulse pulumi preview -s prod --format json` hands `--format`
to Pulumi. A `--` in front of Pulumi's arguments makes the split plain:
`qloudpulse pulumi up --require-estimate -- -s prod --yes`. Pulumi's flags are read as words of their own (`-y`, not
`-yf`). Pulumi's `--json` is refused, as the estimate is printed on the same stdout; for JSON, pipe
`pulumi preview --json --show-sames` into `qloudpulse estimate`. The priced preview has no terminal to prompt on, so a
stack with a passphrase needs `PULUMI_CONFIG_PASSPHRASE` or `PULUMI_CONFIG_PASSPHRASE_FILE`. `pulumi` is looked up on
`PATH`.

Pulumi's `--event-log` cannot spare the second preview: it is there only with `PULUMI_DEBUG_COMMANDS`, and its
resource states (`apitype.StepEventStateMetadata`) carry no `propertyDependencies` and no stack config, which the
estimate needs for references and regions.

The token is read from `QLOUDPULSE_TOKEN`, never from an option: a token of your own (`qpev_…`), which anyone in
the company, a viewer too, makes under Your tokens, under their name in qloudpulse (on hosted qloudpulse,
[app.qloudpulse.com/ui/?profile#my-tokens](https://app.qloudpulse.com/ui/?profile#my-tokens)). It lasts 30, 90 or 365
days, as you choose. An admin's event-source token from Settings, Events works too, and that source can send the
change event after `pulumi up`. Every run but
`--dry-run` needs it, even one whose preview holds nothing to price. When it is unset and you run the client at a
terminal, it asks for the token there, with the input hidden, and reads the answer from the terminal itself
(`/dev/tty`, or the console on Windows), never from stdin, so `pulumi preview --json | qloudpulse estimate` still
works. It takes three tries at a value shaped like a token, uses it for that run only, writes it nowhere, and suggests
`export QLOUDPULSE_TOKEN` for next time. `qloudpulse pulumi preview` asks after Pulumi's diff, and `qloudpulse pulumi
up` before its priced preview. In CI, or with no terminal, `estimate` stops with an error before it reads the preview
or asks for the catalogue, `pulumi preview` exits 1 after the diff without previewing again, and `pulumi up` says so
in a line on stderr and goes on with the update (with `--require-estimate`, stops).

In GitHub Actions:

```yaml
- name: Cost of this change
  env:
    QLOUDPULSE_TOKEN: ${{ secrets.QLOUDPULSE_TOKEN }}
    GH_TOKEN: ${{ github.token }}
  run: |
    pulumi preview --json --show-sames -s payments-prod > preview.json
    qloudpulse estimate preview.json > comment.md
    gh pr comment ${{ github.event.pull_request.number }} --body-file comment.md
```

## What it sends

Only what sets a price. For each resource the change creates, updates, replaces or deletes, of a type in the
catalogue:

- its type token and the change;
- its region, from its provider, else the stack's `aws:region`, else `--region`;
- the attributes in the catalogue for its type, before and after. A value Pulumi knows only after apply, or prints as
  `[secret]`, is left out and its attribute named under `unknown`;
- a key, `r` and 16 hex digits of the SHA-256 of its URN. The URN, with its resource name, stays in CI, and the
  comment's names come from the preview the client still holds;
- for a type whose catalogue entry has `references`, such as an ECS service's `taskDefinition`, the key of the resource
  each one names (from Pulumi's `propertyDependencies`), so qloudpulse can price the two together;
- with `--with-resource-ids`, its cloud ID.

Every resource the change leaves the same, of a type in the catalogue, is sent too, as `"change": "same"`: its type,
region, catalogue attributes and key, never its cloud ID, even with `--with-resource-ids`. qloudpulse answers each
with one figure for now and after, so the comment gives the whole stack's monthly cost beside what the change adds,
and prices a changed resource with one it refers to, such as an ECS service with its task definition. Pulumi lists
unchanged resources only with `pulumi preview --json --show-sames`; without it the comment says so, and a
reference's key is sent without the resource. When the unchanged resources would take a request past its 5,000, only
those a changed one refers to go, and the comment leaves the stack's cost out. An endpoint from before 2026-11-19
does not answer unchanged resources, and the comment says so instead of a stack line.

Once per request, the stack goes as `"stack": "project/stack"`, from the URNs when they all name the same one, so
qloudpulse's Pulumi stacks page can list the estimate under it. Types of the catalogue's providers (`aws`) that it
does not list are counted by type, except those the catalogue names under `free`, which AWS charges nothing for of
their own (IAM, a VPC and its parts, S3 bucket settings): those are neither sent nor counted, and the comment lists
them as no charge of their own. Nothing else from the preview is sent: no resource names, tags, config, outputs
or secrets.

The comment links to the estimate's page in qloudpulse ("See it in qloudpulse") when the answer gives its address.
The resource names travel only in the link's `#fragment`, `#names=` and base64url of a JSON object of key to name,
which a browser never sends to a server: they reach only the browser of whoever opens the link. `--dry-run` prints exactly what would go. The request and the answer are
[`estimate.schema.json`](estimate.schema.json).

## Verify what you run

The client holds a token that can price your stack, so check what you run instead of trusting it. Each step below
needs only public things: this repository, the release, and Sigstore's public log.

**1. The archive was built from this repository, by its release workflow, at the release's tag.** Every archive of a
release has signed [SLSA build provenance](https://slsa.dev/spec/v1.0/provenance), made by
[`.github/workflows/cli-release.yml`](../.github/workflows/cli-release.yml) on GitHub's runners and recorded in
Sigstore's public transparency log. Nothing built on a laptop can carry it. With the [GitHub CLI](https://cli.github.com)
(signed in to any GitHub account):

```bash
VERSION=0.3.0
TARGET=x86_64-unknown-linux-musl
gh attestation verify "qloudpulse-$VERSION-$TARGET.tar.gz" --repo ulfmansson/qloudpulse-public \
  --signer-workflow ulfmansson/qloudpulse-public/.github/workflows/cli-release.yml \
  --source-ref "refs/tags/qloudpulse-cli-$VERSION"
```

`--repo` alone checks that the archive was attested by a workflow of this repository. `--signer-workflow` and
`--source-ref` also check that it was the release workflow, run on the release's tag and not on a branch. The commit
it was built from is in the signing certificate, which the workflow cannot write; add
`--format json --jq '.[].verificationResult.signature.certificate.sourceRepositoryDigest'` to print it, and compare it
with `git rev-parse "qloudpulse-cli-$VERSION^{commit}"` in a clone whose source you have read.

Without the GitHub CLI, or offline, use Sigstore's own [cosign](https://docs.sigstore.dev/cosign/) (3.0 or later)
with the bundle each release carries:

```bash
cosign verify-blob-attestation \
  --bundle "qloudpulse-$VERSION.provenance.sigstore.json" \
  --type slsaprovenance1 \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity "https://github.com/ulfmansson/qloudpulse-public/.github/workflows/cli-release.yml@refs/tags/qloudpulse-cli-$VERSION" \
  "qloudpulse-$VERSION-$TARGET.tar.gz"
```

The release's SBOM, `qloudpulse-$VERSION.cdx.json` (CycloneDX), lists every crate the client is built from, and it is
signed for the same archives: `gh attestation verify … --predicate-type https://cyclonedx.org/bom`.

**2. The download is whole.** `SHA256SUMS` has the SHA-256 of every archive, as in [Install](#install):

```bash
sha256sum -c --ignore-missing SHA256SUMS   # on macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
```

The sums prove the file is the one uploaded, not who built it: whoever can upload an archive can upload its sum. The
provenance above proves who built it. In a pipeline, verify a version once when you adopt it, then pin its SHA-256
there, so every run checks the same bytes:

```bash
echo "<sha256 from SHA256SUMS>  qloudpulse-$VERSION-$TARGET.tar.gz" | sha256sum -c -
```

**3. Or build it yourself from the tag.** With Rust 1.88 or later; `--locked` builds exactly the crate versions, with
their checksums, in the tag's `Cargo.lock`:

```bash
cargo install --git https://github.com/ulfmansson/qloudpulse-public --tag "qloudpulse-cli-$VERSION" --locked qloudpulse-cli
```

Every pull request to this repository runs `cargo test`, `cargo clippy`, `cargo deny` (RustSec advisories, the
licences in [`deny.toml`](deny.toml), crates.io as the only source) and `cargo audit`, in
[`.github/workflows/cli-checks.yml`](../.github/workflows/cli-checks.yml); the same checks run weekly on `main`.

**4. Run it where it can do no harm.** The client reads two things: the preview's JSON (a file or stdin) and
`QLOUDPULSE_TOKEN`; and `QLOUDPULSE_ENDPOINT` and `QLOUDPULSE_CATALOGUE` if you set them. It needs no cloud
credentials, no Pulumi access token and no repository token, and it talks to one host, `api.qloudpulse.com` (the
estimates and their `/catalogue`), over HTTPS. So run `pulumi preview` in one job, with the cloud credentials, and the
client in another that has only the qloudpulse token:

```yaml
jobs:
  preview:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      id-token: write   # if your cloud credentials come from OIDC
    steps:
      # ... check out, set up Pulumi and the cloud credentials ...
      - run: pulumi preview --json --show-sames -s payments-prod > preview.json
      - uses: actions/upload-artifact@<sha> # pin by commit
        with:
          name: preview
          path: preview.json

  estimate:
    needs: preview
    runs-on: ubuntu-24.04
    permissions: {}   # no repository token, and no cloud credentials or Pulumi token in this job
    steps:
      - uses: actions/download-artifact@<sha>
        with:
          name: preview
      - env:
          VERSION: "0.3.0"
          TARGET: x86_64-unknown-linux-musl
          SHA256: "<sha256 from SHA256SUMS, checked once with gh attestation verify>"
          QLOUDPULSE_TOKEN: ${{ secrets.QLOUDPULSE_TOKEN }}
        run: |
          curl -fsSLO "https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-cli-$VERSION/qloudpulse-$VERSION-$TARGET.tar.gz"
          echo "$SHA256  qloudpulse-$VERSION-$TARGET.tar.gz" | sha256sum -c -
          tar -xzf "qloudpulse-$VERSION-$TARGET.tar.gz"
          ./qloudpulse estimate preview.json > comment.md
      - uses: actions/upload-artifact@<sha>
        with:
          name: comment
          path: comment.md

  comment:
    needs: estimate
    runs-on: ubuntu-24.04
    permissions:
      pull-requests: write   # this job posts the comment, and does not run the client
    steps:
      - uses: actions/download-artifact@<sha>
        with:
          name: comment
      - env:
          GH_TOKEN: ${{ github.token }}
          PR: ${{ github.event.pull_request.number }}
        run: gh pr comment "$PR" --repo "$GITHUB_REPOSITORY" --body-file comment.md
```

Where your runners let you, also limit the estimate job's egress to `api.qloudpulse.com:443` (and `github.com` and
its release download host, if it downloads the client there): a firewall or proxy allowlist on a self-hosted runner,
or a network policy on a Kubernetes one. GitHub-hosted runners have no egress filter of their own; tools such as
[StepSecurity Harden-Runner](https://docs.stepsecurity.io/harden-runner) add one. With `--catalogue <file>` the client
does not ask for the catalogue either, and sends only what that file lists.

**5. Read what it sends.** `--dry-run` prints the exact request the client would send, and sends nothing. It needs no
token; it still asks `api.qloudpulse.com/v1/estimates/catalogue` which types to send (open, no token), unless
`--catalogue` names a file:

```bash
qloudpulse estimate --dry-run preview.json | jq .
```

The request is what [What it sends](#what-it-sends) describes, in the format of
[`estimate.schema.json`](estimate.schema.json): no names, tags, config, outputs or secrets.

## Changing it

```bash
cargo test --locked
```

Before a commit, `cargo fmt` and `cargo clippy --all-targets -- -D warnings`. A change users would notice adds a line
under Unreleased in [CHANGELOG.md](CHANGELOG.md); a pull request that changes `src/` without one gets a warning.
Pull requests also run `cargo deny check
advisories licenses sources` and `cargo audit`; a new dependency under a licence [`deny.toml`](deny.toml) does not
allow fails until it is read and added there.

A new type is priced in qloudpulse, not here: its catalogue and its pricing change together, and qloudpulse keeps only
the catalogue's attributes of whatever a client sends. A client change is needed only for a new kind of input, such as
a Terraform plan.

`pulumi preview --json` writes Pulumi's `display.PreviewDigest`: `steps[]`, each with `op`, `urn` and the resource's
`oldState` and `newState` (`apitype.ResourceV3`). A replacement can come as three steps, `create-replacement`,
`replace` and `delete-replaced`, and the client counts them as one. The fixtures in `tests/fixtures/` follow that
format.

## Releasing

Releases are built on GitHub's runners by [`.github/workflows/cli-release.yml`](../.github/workflows/cli-release.yml),
never on a laptop, so that each archive's provenance names a public workflow and commit. To release, raise `version`
in `Cargo.toml`, and in [CHANGELOG.md](CHANGELOG.md) move the lines under Unreleased into that version's section and
date it (`## [<version>] - YYYY-MM-DD`). That section becomes the release's notes on GitHub. Merge, and from an
up-to-date `main`:

```bash
cli/release.sh --publish
```

That builds nothing. It checks that `HEAD` is `origin/main`, that the changelog has the version's section, dated,
and nothing left under Unreleased, that the version is not released yet, and that the
default address's catalogue answers 200 (`https://api.qloudpulse.com/v1/estimates`, qloudpulse's host for machines,
ADR 0127; client 0.1.0 used `https://app.qloudpulse.com/api/report/public/estimates`, which keeps answering). Then it
pushes the tag `qloudpulse-cli-<version>`. The tag starts the workflow, which checks the same again, runs
`cargo test --locked`, builds every target with `release.sh`, runs the two Windows `.exe` files on Windows x86_64 and
Windows on ARM, writes the SBOM and `SHA256SUMS`, signs provenance for every archive, and publishes the release. Watch
it with `gh run list -R ulfmansson/qloudpulse-public --workflow cli-release.yml`.

The workflow also builds every target, without signing or publishing, on each pull request that changes `cli/`.
Every action in it is pinned by commit, and every build image (in `release.sh`) by digest.

`release.sh` without `--publish` builds every target here and packs the same files, for trying a build; nothing leaves
your machine, and nothing it builds is released:

```bash
cli/release.sh
```

It needs Docker (the Linux and Windows builds) and rustup (the macOS builds). The Windows `.exe` files are cross-built,
the x86_64 one with mingw-w64 and the arm64 one with [llvm-mingw](https://github.com/mstorsjo/llvm-mingw) (Rust's
`aarch64-pc-windows-gnullvm` target), and cannot run on the Mac that builds them: the script checks that each is a
Windows program for its architecture that needs only DLLs every Windows has, and that it names its version.

Clients hear about a release once qloudpulse says so: raise `estimates.client.latest` in the report service's
`application.yml` to the new version (`ESTIMATES_CLIENT_LATEST` on an install), and `estimates.client.minimum` only
when older clients must stop.

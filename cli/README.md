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
| `--format json` | Print qloudpulse's answer as JSON instead of the comment |
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

The token is read from `QLOUDPULSE_TOKEN` only, never from an option: a token of your own (`qpev_…`), which anyone in
the company, a viewer too, makes under Your tokens, under their name in qloudpulse (on hosted qloudpulse,
[app.qloudpulse.com/ui/?profile#my-tokens](https://app.qloudpulse.com/ui/?profile#my-tokens)). It lasts 30, 90 or 365
days, as you choose. An admin's event-source token from Settings, Events works too, and that source can send the
change event after `pulumi up`. Every run but
`--dry-run` needs it, even one whose preview holds nothing to price: without it `estimate` stops with an error before
it reads the preview or asks for the catalogue, `pulumi preview` exits 1 after the diff without previewing again, and
`pulumi up` says so in a line on stderr and goes on with the update (with `--require-estimate`, stops).

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
- a key, `r` and 16 hex digits of the SHA-256 of its URN. The URN, with the stack, project and resource names, stays
  in CI, and the comment's names come from the preview the client still holds;
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

Types of the catalogue's providers (`aws`) that it does not list are counted by type. Nothing else from the preview is sent: no names, tags, config,
outputs or secrets. `--dry-run` prints exactly what would go. The request and the answer are
[`estimate.schema.json`](estimate.schema.json).

## Changing it

```bash
cargo test --locked
```

Before a commit, `cargo fmt` and `cargo clippy --all-targets -- -D warnings`.

A new type is priced in qloudpulse, not here: its catalogue and its pricing change together, and qloudpulse keeps only
the catalogue's attributes of whatever a client sends. A client change is needed only for a new kind of input, such as
a Terraform plan.

`pulumi preview --json` writes Pulumi's `display.PreviewDigest`: `steps[]`, each with `op`, `urn` and the resource's
`oldState` and `newState` (`apitype.ResourceV3`). A replacement can come as three steps, `create-replacement`,
`replace` and `delete-replaced`, and the client counts them as one. The fixtures in `tests/fixtures/` follow that
format.

## Releasing

The default address is `https://api.qloudpulse.com/v1/estimates`, qloudpulse's host for machines (ADR 0127). Client
0.1.0 used `https://app.qloudpulse.com/api/report/public/estimates`, which keeps answering. `release.sh --publish`
asks the default address's catalogue first and stops unless it answers 200.

`release.sh` builds every target, checks each binary, and packs the files above. Without `--publish` nothing leaves
your machine:

```bash
cli/release.sh
```

```bash
cli/release.sh --publish
```

It needs Docker (the Linux and Windows builds), rustup (the macOS builds) and `gh` signed in with write access to this
repository. The Windows `.exe` files are cross-built, the x86_64 one with mingw-w64 and the arm64 one with
[llvm-mingw](https://github.com/mstorsjo/llvm-mingw) (Rust's `aarch64-pc-windows-gnullvm` target), and cannot run on
the Mac that builds them: the script checks that each is a Windows program for its architecture that needs only DLLs
every Windows has, and that it names its version. The first run of each release on Windows is the real test of it.
To release a new version, raise `version` in `Cargo.toml` first. The release's tag is `qloudpulse-cli-<version>`.

Clients hear about a release once qloudpulse says so: raise `estimates.client.latest` in the report service's
`application.yml` to the new version (`ESTIMATES_CLIENT_LATEST` on an install), and `estimates.client.minimum` only
when older clients must stop.

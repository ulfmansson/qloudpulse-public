# qloudpulse CLI

`qloudpulse estimate` tells you what an infrastructure change will cost before you apply it, priced at your company's
own rates, and prints a comment for the pull request. This first version reads Pulumi previews. Terraform and OpenTofu
plans come next.

It sends the attributes that set each resource's price to qloudpulse (`POST https://api.qloudpulse.com/v1/estimates`). Names,
tags, config and secrets stay in your CI. [What it sends](#what-it-sends) lists exactly what goes.

Which resource types are priced, and which of their attributes are sent, is qloudpulse's **catalogue**. On every
run the client sends qloudpulse the resource types it found in the preview, type tokens only, to
`POST https://api.qloudpulse.com/v1/estimates/catalogue` (open, no token), and gets back what to send for those qloudpulse
prices. So when qloudpulse prices a new type, the client you already have sends it.

## Install

Each release has a static binary for Linux (x86_64 and arm64) and binaries for macOS, with their SHA-256 sums:

```bash
VERSION=0.1.0
TARGET=x86_64-unknown-linux-musl   # aarch64-unknown-linux-musl, aarch64-apple-darwin, x86_64-apple-darwin
BASE=https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-cli-$VERSION
curl -fsSLO "$BASE/qloudpulse-$VERSION-$TARGET.tar.gz"
curl -fsSLO "$BASE/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS   # on macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
tar -xzf "qloudpulse-$VERSION-$TARGET.tar.gz"
./qloudpulse --version
```

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

| Option | |
|---|---|
| `FILE` | The JSON of `pulumi preview --json`. Without it, or with `-`, stdin |
| `--dry-run` | Print the request that would be sent, and send nothing. Needs no token |
| `--region <region>` | The region of resources whose provider and stack name none |
| `--with-resource-ids` | Send the cloud ID of each resource that exists before the change, so its usage comes from what it really used |
| `--format json` | Print qloudpulse's answer as JSON instead of the comment |
| `--endpoint <url>` | Another address, such as your own qloudpulse install, also `QLOUDPULSE_ENDPOINT`. https only; http only to localhost. The types are asked about at this address and `/catalogue` |
| `--catalogue <file>` | Read the answer from a file instead of asking, also `QLOUDPULSE_CATALOGUE`, to pin what is sent. `qloudpulse estimate --dry-run` shows the types; `curl -fsS -H 'Content-Type: application/json' -d '{"formatVersion": 1, "types": ["aws:rds/instance:Instance"]}' https://api.qloudpulse.com/v1/estimates/catalogue > catalogue.json` saves an answer |

The token is read from `QLOUDPULSE_TOKEN` only, never from an option: a change-event source's token (`qpev_…`), from
Settings, Events in qloudpulse. The same source can send the change event after `pulumi up`.

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

A resource a changed one refers to and the change leaves the same is sent too, as `"change": "same"`, with its
catalogue attributes only. Pulumi lists unchanged resources only with `pulumi preview --json --show-sames`; without it
the reference's key is sent and qloudpulse says what it could not price.

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

**0.2.0 waits for `api.qloudpulse.com`** (qloudpulse's ADR 0127, step 1): it is this client's default address, and the
host is not live yet. Check before releasing:

```bash
curl -fsS -o /dev/null -w '%{http_code}\n' -H 'Content-Type: application/json' -d '{"formatVersion": 1, "types": []}' https://api.qloudpulse.com/v1/estimates/catalogue
```

It must print `200`. Client 0.1.0 used `https://app.qloudpulse.com/api/report/public/estimates`, which keeps answering;
`--endpoint` points this client there meanwhile.

`release.sh` builds every target, checks each binary, and packs the files above. Without `--publish` nothing leaves
your machine:

```bash
cli/release.sh
```

```bash
cli/release.sh --publish
```

It needs Docker (the Linux builds), rustup (the macOS builds) and `gh` signed in with write access to this repository.
To release a new version, raise `version` in `Cargo.toml` first. The release's tag is `qloudpulse-cli-<version>`.

Clients hear about a release once qloudpulse says so: raise `estimates.client.latest` in the report service's
`application.yml` to the new version (`ESTIMATES_CLIENT_LATEST` on an install), and `estimates.client.minimum` only
when older clients must stop.

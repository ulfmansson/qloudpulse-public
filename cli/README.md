# qloudpulse CLI

`qloudpulse estimate` tells you what an infrastructure change will cost before you apply it, priced at your company's
own rates, and prints a comment for the pull request. This first version reads Pulumi previews. Terraform and OpenTofu
plans come next.

It sends the attributes that set each resource's price to qloudpulse (`POST /api/report/public/estimates`). Names,
tags, config and secrets stay in your CI. [What it sends](#what-it-sends) lists exactly what goes.

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

## Use

```bash
pulumi preview --json -s payments-prod | qloudpulse estimate > comment.md
```

| Option | |
|---|---|
| `FILE` | The JSON of `pulumi preview --json`. Without it, or with `-`, stdin |
| `--dry-run` | Print the request that would be sent, and send nothing. Needs no token |
| `--region <region>` | The region of resources whose provider and stack name none |
| `--with-resource-ids` | Send the cloud ID of each resource that exists before the change, so its usage comes from what it really used |
| `--format json` | Print qloudpulse's answer as JSON instead of the comment |
| `--endpoint <url>` | Another address, such as your own qloudpulse install, also `QLOUDPULSE_ENDPOINT`. https only; http only to localhost |

The token is read from `QLOUDPULSE_TOKEN` only, never from an option: a change-event source's token (`qpev_…`), from
Settings, Events in qloudpulse. The same source can send the change event after `pulumi up`.

In GitHub Actions:

```yaml
- name: Cost of this change
  env:
    QLOUDPULSE_TOKEN: ${{ secrets.QLOUDPULSE_TOKEN }}
    GH_TOKEN: ${{ github.token }}
  run: |
    pulumi preview --json -s payments-prod > preview.json
    qloudpulse estimate preview.json > comment.md
    gh pr comment ${{ github.event.pull_request.number }} --body-file comment.md
```

## What it sends

Only what sets a price. For each resource the change creates, updates, replaces or deletes, of a type in
[`src/catalogue.rs`](src/catalogue.rs):

- its type token and the change;
- its region, from its provider, else the stack's `aws:region`, else `--region`;
- the attributes in the catalogue for its type, before and after. A value Pulumi knows only after apply, or prints as
  `[secret]`, is left out and its attribute named under `unknown`;
- a key, `r` and 16 hex digits of the SHA-256 of its URN. The URN, with the stack, project and resource names, stays
  in CI, and the comment's names come from the preview the client still holds;
- with `--with-resource-ids`, its cloud ID.

AWS types it does not price are counted by type. Nothing else from the preview is sent: no names, tags, config,
outputs or secrets. `--dry-run` prints exactly what would go. The request and the answer are
[`estimate.schema.json`](estimate.schema.json).

## Changing it

```bash
cargo test --locked
```

Before a commit, `cargo fmt` and `cargo clippy --all-targets -- -D warnings`.

To price a new type, add it to `PRICED` in `src/catalogue.rs` with the attributes that set its price. qloudpulse has
to price that type too, or it answers the resource as not priced.

`pulumi preview --json` writes Pulumi's `display.PreviewDigest`: `steps[]`, each with `op`, `urn` and the resource's
`oldState` and `newState` (`apitype.ResourceV3`). A replacement can come as three steps, `create-replacement`,
`replace` and `delete-replaced`, and the client counts them as one. The fixtures in `tests/fixtures/` follow that
format.

## Releasing

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

#!/usr/bin/env bash
# Builds qloudpulse-collect for every target, checks each binary, and packs one archive per target (a tarball, or a
# zip for Windows) with SHA256SUMS and release notes, for trying a build here. Nothing it builds leaves this machine.
#
# Releases are built by .github/workflows/cli-collect-release.yml on GitHub's runners, which runs this script per target and
# signs build provenance for every archive and binary, so anyone can check which public commit and workflow made a
# binary. The builds are reproducible: the same commit built again, here or on another runner, gives the same bytes,
# which --reproduce checks against a published release. With --publish this script builds nothing: it checks that the
# commit may be released and pushes the tag qloudpulse-collect-<version>, which starts that workflow.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: cli/collect/release.sh [--targets "<target> ..."] [--out <dir>] [--skip-tests] [--compare <SHA256SUMS> | --reproduce]
       cli/collect/release.sh --publish
       cli/collect/release.sh --finish <dir>
       cli/collect/release.sh --check-changelog

  --targets     build only these targets (default: all six below)
  --out         where the files go (default: cli/collect/target/dist/<version>)
  --skip-tests  build without running cargo test first (the release workflow tests in a job of its own)
  --compare     after building, compare each binary with its line in this SHA256SUMS, a file or an https URL: the
                same commit built again has to give the same bytes (the release workflow builds every target twice)
  --reproduce   --compare with the SHA256SUMS of the published release of this version; HEAD has to be its tag
                (git switch --detach qloudpulse-collect-<version>), and cli/collect/ unchanged
  --publish     build nothing; check origin/main may be released and push the tag qloudpulse-collect-<version>, which
                starts the release workflow on GitHub. The workflow builds, attests and publishes
  --finish      write SHA256SUMS and notes.md for the archives already in <dir> (the release workflow, once every
                target is built)
  --check-changelog
                check that CHANGELOG.md has a dated section for this version and nothing left under Unreleased,
                as a release needs; --publish and --finish check it too

Targets:
  x86_64-unknown-linux-musl  aarch64-unknown-linux-musl   static, built in Docker
  aarch64-apple-darwin       x86_64-apple-darwin          built here with rustup and Rust 1.88.0 (macOS only)
  x86_64-pc-windows-gnu                                   built in Docker with mingw-w64, CRT linked in
  aarch64-pc-windows-gnullvm                              Windows on ARM, built in Docker with llvm-mingw

Environment:
  RUST_IMAGE     the image of the Linux builds (default rust:1.88-alpine, the client's rust-version)
  WINDOWS_IMAGE  the image of the x86_64 Windows build (default rust:1.88-bookworm)
  LLVM_MINGW_IMAGE  the llvm-mingw toolchain of the arm64 Windows build (default mstorsjo/llvm-mingw:20261104)
  RUSTUP_TOOLCHAIN  the Rust of the macOS builds (default 1.88.0, the Rust of the images)
  REPO         the GitHub repository to release in (default ulfmansson/qloudpulse-public)

The images are pinned by digest as well as tag: a tag can be moved to other contents, a digest cannot. To move to a
new image, look up the digest of its multi-platform index (docker buildx imagetools inspect <image>) and change both.

Every binary names its commit in --version, as rustc does (build.rs), and carries the list of crates it was built
from, put there by cargo auditable, so cargo audit bin, Trivy or Grype can check an installed client.
EOF
}

ALL_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin x86_64-pc-windows-gnu aarch64-pc-windows-gnullvm"
TARGETS=$ALL_TARGETS
PUBLISH=0
FINISH=""
CHECK_CHANGELOG=0
SKIP_TESTS=0
OUT=""
COMPARE=""
REPRODUCE=0
RUST_IMAGE=${RUST_IMAGE:-rust:1.88-alpine@sha256:9dfaae478ecd298b6b5a039e1f2cc4fc040fc818a2de9aa78fa714dea036574d}
WINDOWS_IMAGE=${WINDOWS_IMAGE:-rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0}
LLVM_MINGW_IMAGE=${LLVM_MINGW_IMAGE:-mstorsjo/llvm-mingw:20261104@sha256:b2a697af8fc50c5cc56011143453d02168ccfcff635d12ca76a1c363f5f6b0c6}
# The macOS builds use the Rust of the images, so a release built again on a Mac gives the same bytes.
MACOS_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-1.88.0}
# Pinned by version, as the images are by digest: cargo auditable writes the dependency list into each binary, and
# rust-audit-info reads it back to check that it is there.
CARGO_AUDITABLE_VERSION=0.7.7
RUST_AUDIT_INFO_VERSION=0.5.4
REPO=${REPO:-ulfmansson/qloudpulse-public}
WORKFLOW=cli-collect-release.yml

while [ $# -gt 0 ]; do
  case "$1" in
    --publish) PUBLISH=1 ;;
    --finish) FINISH=${2:?--finish needs a directory}; shift ;;
    --skip-tests) SKIP_TESTS=1 ;;
    --check-changelog) CHECK_CHANGELOG=1 ;;
    --targets) TARGETS=${2:?--targets needs a list}; shift ;;
    --out) OUT=${2:?--out needs a directory}; shift ;;
    --compare) COMPARE=${2:?--compare needs a SHA256SUMS file or URL}; shift ;;
    --reproduce) REPRODUCE=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

say() { printf '\n== %s\n' "$*"; }
fail() { printf 'release.sh: %s\n' "$*" >&2; exit 1; }

CLI=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$CLI" rev-parse --show-toplevel)
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$CLI/Cargo.toml" | head -1)
[ -n "$VERSION" ] || fail "no version in $CLI/Cargo.toml"
TAG="qloudpulse-collect-$VERSION"
# Checked before OUT gets its default below, which would otherwise look like an --out the caller passed.
if [ $PUBLISH -eq 1 ]; then
  [ "$TARGETS" = "$ALL_TARGETS" ] && [ -z "$OUT" ] && [ $SKIP_TESTS -eq 0 ] && [ -z "$COMPARE" ] && [ $REPRODUCE -eq 0 ] \
    || fail "--publish builds nothing, so it takes no --targets, --out, --skip-tests, --compare or --reproduce"
fi
[ -z "$COMPARE" ] || [ $REPRODUCE -eq 0 ] || fail "--reproduce compares with the release; it takes no --compare"
OUT=${OUT:-$CLI/target/dist/$VERSION}
COMMIT=$(git -C "$ROOT" rev-parse HEAD)
# What a binary's --version names (build.rs): the commit, marked when cli/collect/ has changes, and the commit's date in UTC.
# Both come from git and not the clock, so the same commit gives the same binary.
COMMIT_DATE=$(TZ=UTC git -C "$ROOT" log -1 --format=%cd --date=format-local:%Y-%m-%d "$COMMIT")
MODIFIED=""
# cli/stack is built in too (its sign-in), so a change there marks the binary as well.
[ -z "$(git -C "$ROOT" status --porcelain -- "$CLI" "$CLI/../stack")" ] || MODIFIED="-modified"
LONG_VERSION="$VERSION (${COMMIT:0:9}$MODIFIED $COMMIT_DATE)"
# The time the archives give their file: the commit's, so an archive is as reproducible as the binary in it.
STAMP=$(TZ=UTC git -C "$ROOT" log -1 --format=%cd --date=format-local:%Y%m%d%H%M.%S "$COMMIT")
TOOLS="$CLI/target/tools"

for t in $TARGETS; do
  case " $ALL_TARGETS " in *" $t "*) ;; *) fail "unknown target $t" ;; esac
done

# Archives are owned by root:0, whichever tar is first on PATH: bsdtar (macOS's /usr/bin/tar) and GNU tar (a devbox
# or Linux shell) spell it differently. COPYFILE_DISABLE=1 keeps bsdtar from adding AppleDouble (._*) files and
# --no-xattrs keeps out macOS attributes such as com.apple.provenance, which GNU tar warns about on extract; GNU tar
# stores neither unless asked.
TAR_VERSION=$(tar --version 2>/dev/null || true)
case "$TAR_VERSION" in
  *"GNU tar"*) TAR_OWNER="--owner=root:0 --group=root:0" ;;
  *bsdtar*) TAR_OWNER="--uid 0 --gid 0 --uname root --gname root --no-xattrs --no-mac-metadata" ;;
  *) fail "tar is neither GNU tar nor bsdtar: $(echo "$TAR_VERSION" | head -1)" ;;
esac

write_installers() { # dir: install.sh and install.ps1 with this version written in, so each installs its release
  sed "s/^DEFAULT_VERSION=\"\"\$/DEFAULT_VERSION=\"$VERSION\"/" "$CLI/install.sh" >"$1/install.sh"
  sed "s/^\\\$QloudpulseDefaultVersion = ''\$/\\\$QloudpulseDefaultVersion = '$VERSION'/" "$CLI/install.ps1" >"$1/install.ps1"
  grep -qxF "DEFAULT_VERSION=\"$VERSION\"" "$1/install.sh" || fail "install.sh has no DEFAULT_VERSION=\"\" line to fill in"
  grep -qxF "\$QloudpulseDefaultVersion = '$VERSION'" "$1/install.ps1" \
    || fail "install.ps1 has no \$QloudpulseDefaultVersion = '' line to fill in"
  chmod 755 "$1/install.sh"
}

write_sums() { # dir: the archives, the binary in each and the install scripts, so the provenance attested for
  # SHA256SUMS covers all of them. A binary is named <archive>/<file>, as
  # qloudpulse-collect-<version>-x86_64-unknown-linux-musl/qloudpulse-collect: sha256sum -c --ignore-missing passes over those lines,
  # and an installed binary can be checked by its sum or with gh attestation verify. The binaries are read back out of the archives, so the sums are of what a
  # download holds.
  local dir=$1 archive base tmp f
  (cd "$dir" && shasum -a 256 qloudpulse-collect-*.tar.gz qloudpulse-collect-*.zip install.sh install.ps1 2>/dev/null >SHA256SUMS || true)
  grep -Eq '\.(tar\.gz|zip)$' "$dir/SHA256SUMS" || fail "nothing was packed"
  for archive in "$dir"/qloudpulse-collect-*.tar.gz "$dir"/qloudpulse-collect-*.zip; do
    [ -f "$archive" ] || continue
    base=$(basename "$archive")
    base=${base%.tar.gz}
    base=${base%.zip}
    tmp=$(mktemp -d "$dir/.unpack.XXXXXX")
    case "$archive" in
      *.zip) unzip -q "$archive" -d "$tmp" ;;
      *) tar -xzf "$archive" -C "$tmp" ;;
    esac
    for f in "$tmp"/*; do
      printf '%s  %s\n' "$(shasum -a 256 <"$f" | cut -d ' ' -f 1)" "$base/$(basename "$f")" >>"$dir/SHA256SUMS"
    done
    rm -rf "$tmp"
  done
}

compare_sums() { # theirs ours: each binary built here against its line in theirs
  local theirs=$1 ours=$2 t exe name want got differ=0
  for t in $TARGETS; do
    case "$t" in *-windows-*) exe=qloudpulse-collect.exe ;; *) exe=qloudpulse-collect ;; esac
    name="qloudpulse-collect-$VERSION-$t/$exe"
    want=$(awk -v f="$name" '$2 == f || $2 == "*" f { print $1 }' "$theirs")
    got=$(awk -v f="$name" '$2 == f { print $1 }' "$ours")
    [ -n "$want" ] || fail "$COMPARE names no $name (releases up to 0.3.1 list their archives only)"
    if [ "$got" = "$want" ]; then
      echo "  the same:  $name  $got"
    else
      echo "  DIFFERENT: $name  built here $got, $COMPARE has $want"
      differ=1
    fi
  done
  [ $differ -eq 0 ] || fail "the binaries built here are not the ones in $COMPARE"
}

CHANGELOG="$CLI/CHANGELOG.md"

changelog_section() { # version: the lines under its "## [<version>]" heading, up to the next "## "
  awk -v v="$1" '
    index($0, "## [" v "]") == 1 { on = 1; next }
    on && (/^## / || /^\[[^]]*\]: /) { exit }
    on { print }
  ' "$CHANGELOG" | sed -e '/./,$!d'
}

check_changelog() { # a release needs its version's section, dated, and nothing left under Unreleased
  [ -f "$CHANGELOG" ] || fail "no $CHANGELOG"
  grep -Eq "^## \[${VERSION//./\\.}\] - [0-9]{4}-[0-9]{2}-[0-9]{2}\$" "$CHANGELOG" \
    || fail "CHANGELOG.md has no dated section for $VERSION: its heading must be '## [$VERSION] - YYYY-MM-DD'"
  # Not a pipe into grep -q: see check_binary.
  local unreleased
  unreleased=$(changelog_section Unreleased)
  case "$unreleased" in
    *"- "*) fail "CHANGELOG.md still has lines under Unreleased; move them into $VERSION's section first" ;;
  esac
}

write_notes() { # dir built-by
  local changes
  changes=$(changelog_section "$VERSION")
  [ -n "$changes" ] || changes="(CHANGELOG.md has no section for $VERSION.)"
  cat >"$1/notes.md" <<EOF
qloudpulse-collect $VERSION: collects cloud cost data with your own credentials into a bundle you inspect, redact and
deliver. Nothing is read from qloudpulse.
Built from $COMMIT $2.

$changes

## Install

\`\`\`bash
VERSION=$VERSION
TARGET=x86_64-unknown-linux-musl   # aarch64-unknown-linux-musl, aarch64-apple-darwin, x86_64-apple-darwin
BASE=https://github.com/$REPO/releases/download/$TAG
curl -fsSLO "\$BASE/qloudpulse-collect-\$VERSION-\$TARGET.tar.gz"
curl -fsSLO "\$BASE/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify "qloudpulse-collect-\$VERSION-\$TARGET.tar.gz" --repo $REPO \\
  --signer-workflow $REPO/.github/workflows/$WORKFLOW --source-ref refs/tags/$TAG
tar -xzf "qloudpulse-collect-\$VERSION-\$TARGET.tar.gz"
\`\`\`

On Windows (PowerShell):

\`\`\`powershell
\$Version = "$VERSION"
\$Base = "https://github.com/$REPO/releases/download/$TAG"
\$Zip = "qloudpulse-collect-\$Version-x86_64-pc-windows-gnu.zip"   # on Windows on ARM: aarch64-pc-windows-gnullvm
Invoke-WebRequest "\$Base/\$Zip" -OutFile \$Zip
Invoke-WebRequest "\$Base/SHA256SUMS" -OutFile SHA256SUMS
\$Want = (Select-String -Path SHA256SUMS -Pattern ([regex]::Escape(\$Zip))).Line.Split(" ")[0]
if ((Get-FileHash \$Zip -Algorithm SHA256).Hash -ne \$Want.ToUpper()) { throw "checksum mismatch" }
gh attestation verify \$Zip --repo $REPO --signer-workflow $REPO/.github/workflows/$WORKFLOW --source-ref refs/tags/$TAG
Expand-Archive \$Zip -DestinationPath .
.\\qloudpulse-collect.exe --version
\`\`\`

\`qloudpulse-collect-$VERSION.cdx.json\` lists every crate the client is built from (CycloneDX). How to verify a release, build
it from source, and build it without the upload:
[cli/collect/README.md](https://github.com/$REPO/blob/$TAG/cli/collect/README.md#verify-what-you-run).
EOF
}

if [ $CHECK_CHANGELOG -eq 1 ]; then
  check_changelog
  echo "CHANGELOG.md has $VERSION's section, dated, and nothing under Unreleased."
  exit 0
fi

if [ -n "$FINISH" ]; then
  [ -d "$FINISH" ] || fail "$FINISH is not a directory"
  check_changelog
  # A release has every target or none.
  for t in $ALL_TARGETS; do
    case "$t" in *-windows-*) f="qloudpulse-collect-$VERSION-$t.zip" ;; *) f="qloudpulse-collect-$VERSION-$t.tar.gz" ;; esac
    [ -f "$FINISH/$f" ] || fail "$FINISH has no $f"
  done
  write_installers "$FINISH"
  write_sums "$FINISH"
  write_notes "$FINISH" "by [the release workflow](https://github.com/$REPO/blob/$TAG/.github/workflows/$WORKFLOW) on
GitHub's runners, with signed build provenance for every archive"
  cat "$FINISH/SHA256SUMS"
  exit 0
fi

if [ $PUBLISH -eq 1 ]; then
  # Nothing is built here: the tag starts the release workflow, which builds every target on GitHub's runners from
  # the tagged commit and attests what it built. A binary from this machine could not be told from one built anywhere
  # else, so none is uploaded from it.
  command -v gh >/dev/null || fail "--publish needs gh"
  check_changelog
  # Unlike qloudpulse-stack there is no endpoint to ask first: the client reads nothing from qloudpulse, and its
  # upload is the customer's choice.
  git -C "$ROOT" fetch -q origin main
  [ "$COMMIT" = "$(git -C "$ROOT" rev-parse origin/main)" ] \
    || fail "HEAD is not origin/main; release only what main has (git switch main && git pull)"
  if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    fail "$TAG is already released in $REPO; raise version in cli/collect/Cargo.toml first"
  fi
  if git -C "$ROOT" ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null; then
    fail "the tag $TAG is already on origin; its workflow run is the release (gh run list -R $REPO --workflow $WORKFLOW)"
  fi
  printf 'Tag %s as %s and push it, so the release workflow builds and publishes it? [y/N] ' "$COMMIT" "$TAG"
  read -r answer
  [ "$answer" = y ] || [ "$answer" = Y ] || fail "not tagged"
  git -C "$ROOT" tag -a "$TAG" -m "qloudpulse-collect $VERSION" "$COMMIT"
  git -C "$ROOT" push origin "refs/tags/$TAG"
  printf '\nPushed %s. The release workflow builds, attests and publishes it:\n' "$TAG"
  printf '  gh run list -R %s --workflow %s --branch %s\n' "$REPO" "$WORKFLOW" "$TAG"
  exit 0
fi

if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
  echo "Note: the checkout has changes; this build is not of $COMMIT alone."
fi

if [ $REPRODUCE -eq 1 ]; then
  # The release's own commit, unchanged: anything else would be a different build, whatever it gives.
  tagged=$(git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG^{commit}") \
    || fail "no tag $TAG here: git fetch origin tag $TAG, then git switch --detach $TAG"
  [ "$tagged" = "$COMMIT" ] || fail "HEAD is not $TAG: git switch --detach $TAG"
  [ -z "$MODIFIED" ] || fail "cli/collect/ has changes; --reproduce builds the tag as it is (git stash, or a clean clone)"
  COMPARE="https://github.com/$REPO/releases/download/$TAG/SHA256SUMS"
fi

if [ $SKIP_TESTS -eq 0 ]; then
  say "Tests"
  (cd "$CLI" && cargo test --locked --quiet)
fi

rm -rf "$OUT"
mkdir -p "$OUT"
BUILD="$OUT/.build"
mkdir -p "$BUILD"

# The builds read these from the environment; the Docker ones are passed them by name (docker run -e).
export QLOUDPULSE_COMMIT="$COMMIT$MODIFIED" QLOUDPULSE_COMMIT_DATE="$COMMIT_DATE" CARGO_AUDITABLE_VERSION

host_tool() { # crate version: a cargo tool installed once under cli/collect/target/tools, for this machine
  grep -F "\"$1 $2 " "$TOOLS/.crates.toml" >/dev/null 2>&1 \
    || cargo install --locked --quiet --root "$TOOLS" "$1@$2" \
    || fail "could not install $1 $2 (cargo install)"
}
host_tool rust-audit-info "$RUST_AUDIT_INFO_VERSION"

# Paths of this machine are written into a binary's panic messages; map them to neutral ones so a public binary
# carries no user name.
REMAP="--remap-path-prefix=$(cd "$CLI/.." && pwd)=/src --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$HOME=/home"
ME=$(id -un)
# The containers build as root; what they leave in the mounted /out is handed to the user running this, so the files
# can be checked, packed and removed on a Linux host too (Docker Desktop on a Mac maps the owner on its own).
OWNER="$(id -u):$(id -g)"

check_binary() { # target binary
  local t=$1 bin=$2 info
  info=$(file "$bin")
  case "$t" in
    x86_64-unknown-linux-musl) echo "$info" | grep -q 'ELF 64-bit.*x86-64' || fail "$t: not an x86-64 ELF: $info" ;;
    aarch64-unknown-linux-musl) echo "$info" | grep -q 'ELF 64-bit.*aarch64' || fail "$t: not an arm64 ELF: $info" ;;
    aarch64-apple-darwin) echo "$info" | grep -q 'Mach-O 64-bit.*arm64' || fail "$t: not an arm64 Mach-O: $info" ;;
    x86_64-apple-darwin) echo "$info" | grep -q 'Mach-O 64-bit.*x86_64' || fail "$t: not an x86_64 Mach-O: $info" ;;
    x86_64-pc-windows-gnu) echo "$info" | grep -q 'PE32+ executable.*x86-64' || fail "$t: not an x86-64 PE: $info" ;;
    aarch64-pc-windows-gnullvm) echo "$info" | grep -q 'PE32+ executable.*Aarch64' || fail "$t: not an arm64 PE: $info" ;;
  esac
  case "$t" in
    *-linux-musl) echo "$info" | grep -q 'statically linked\|static-pie linked' || fail "$t: not static: $info" ;;
    *-windows-*)
      # Only DLLs every Windows has: no libgcc, winpthread or libunwind to ship beside the .exe.
      local extra
      extra=$(grep -viE '^(kernel32|ntdll|advapi32|bcrypt|bcryptprimitives|ws2_32|userenv|msvcrt|ucrtbase|secur32|crypt32|user32|shell32|ole32|oleaut32|synchronization|api-ms-win-[a-z0-9-]+)\.dll$' \
        "$BUILD/$t/dlls" || true)
      [ -z "$extra" ] || fail "$t: needs DLLs a Windows machine does not have: $(echo $extra)"
      ;;
  esac
  # Not grep -q after strings: it stops at the first match, strings dies of SIGPIPE, and with pipefail the pipeline
  # fails, so a match would read as none. -a reads the whole file. The user name counts as a path component only:
  # on a CI runner it is a word such as "runner", which a binary can hold for other reasons. libunwind's assertions,
  # linked in from the pinned llvm-mingw image, name where its maintainer built it, on a GitHub runner too
  # (/home/runner/work/llvm-mingw/llvm-mingw/llvm-project/...): the same in every build from that image, and no
  # path of this machine, so they do not count.
  local leaked
  leaked=$(strings -a "$bin" | grep -F -e "$HOME" -e "/$ME/" -e "${CARGO_HOME:-$HOME/.cargo}" \
    | grep -v -F '/work/llvm-mingw/llvm-mingw/llvm-project/' | head -3 || true)
  [ -z "$leaked" ] || fail "$t: the binary holds a path of this machine ($HOME, /$ME/ or CARGO_HOME): $leaked"
  # The dependency list cargo auditable wrote in (every crate the binary was built from), as scanners read it.
  local deps
  deps=$("$TOOLS/bin/rust-audit-info" "$bin" 2>&1 || true)
  case "$deps" in
    *'"name":"qloudpulse-collect"'*) ;;
    *) fail "$t: the binary has no dependency list from cargo auditable: $(echo "$deps" | head -c 200)" ;;
  esac
}

build_linux() { # target
  local t=$1 platform
  case "$t" in
    x86_64-*) platform=linux/amd64 ;;
    aarch64-*) platform=linux/arm64 ;;
  esac
  command -v docker >/dev/null || fail "the Linux builds need Docker"
  mkdir -p "$BUILD/$t"
  # The source is mounted read-only; --locked keeps Cargo.lock as it is. The crates cache is a named volume.
  # cargo auditable is built once per target into a volume of its own. musl-dev is the one input not pinned by a
  # digest: Alpine keeps only the newest build of a package, so its version is written down beside the binary.
  docker run --rm --platform "$platform" \
    -v "$CLI/..:/src:ro" -v qloudpulse-collect-cargo:/usr/local/cargo/registry -v "qloudpulse-collect-tools-$t:/tools" \
    -v "$BUILD/$t:/out" -e OWNER="$OWNER" -e QLOUDPULSE_COMMIT -e QLOUDPULSE_COMMIT_DATE -e CARGO_AUDITABLE_VERSION \
    -w /src/collect -e CARGO_TARGET_DIR=/tmp/target "$RUST_IMAGE" \
    sh -euc 'apk add --no-cache musl-dev >/dev/null
             apk info -v 2>/dev/null | grep -E "^(musl|musl-dev|gcc|binutils)-[0-9]" >/out/packages || true
             cargo install --locked --quiet --root /tools "cargo-auditable@$CARGO_AUDITABLE_VERSION"
             PATH=/tools/bin:$PATH cargo auditable build --release --locked --quiet
             cp /tmp/target/release/qloudpulse-collect /out/qloudpulse-collect
             chown -R "$OWNER" /out
             /out/qloudpulse-collect --version' >"$BUILD/$t/version"
}

build_windows() { # target
  local t=$1
  command -v docker >/dev/null || fail "the Windows build needs Docker"
  mkdir -p "$BUILD/$t"
  # Cross-built with mingw-w64, the C runtime linked in (+crt-static), so the .exe runs on its own; the DLLs it
  # imports are listed for check_binary. The linker leaves the build time out of the PE header
  # (--no-insert-timestamp), and the mingw-w64 packages are pinned to Debian bookworm's builds, so the .exe is the same
  # whenever it is built.
  docker run --rm \
    -v "$CLI/..:/src:ro" -v qloudpulse-collect-cargo:/usr/local/cargo/registry -v "qloudpulse-collect-tools-$t:/tools" \
    -v "$BUILD/$t:/out" -e OWNER="$OWNER" -e QLOUDPULSE_COMMIT -e QLOUDPULSE_COMMIT_DATE -e CARGO_AUDITABLE_VERSION \
    -w /src/collect -e CARGO_TARGET_DIR=/tmp/target \
    -e RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-Wl,--no-insert-timestamp" \
    -e CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc "$WINDOWS_IMAGE" \
    sh -euc 'apt-get update -qq >/dev/null
             apt-get install -qq -y --no-install-recommends gcc-mingw-w64-x86-64=12.2.0-14+25.2 \
               binutils-mingw-w64-x86-64=2.40-2+10.4 mingw-w64-x86-64-dev=10.0.0-3 >/dev/null
             dpkg-query -W gcc-mingw-w64-x86-64-posix gcc-mingw-w64-x86-64-win32 binutils-mingw-w64-x86-64 \
               mingw-w64-x86-64-dev 2>/dev/null >/out/packages || true
             rustup target add x86_64-pc-windows-gnu >/dev/null 2>&1
             # The tool runs here, on Linux: RUSTFLAGS are for the Windows build, and +crt-static rules out proc macros.
             env -u RUSTFLAGS cargo install --locked --quiet --root /tools "cargo-auditable@$CARGO_AUDITABLE_VERSION"
             PATH=/tools/bin:$PATH cargo auditable build --release --locked --quiet --target x86_64-pc-windows-gnu
             cp /tmp/target/x86_64-pc-windows-gnu/release/qloudpulse-collect.exe /out/qloudpulse-collect.exe
             x86_64-w64-mingw32-objdump -p /out/qloudpulse-collect.exe | sed -n "s/^[[:space:]]*DLL Name: //p" | sort -u >/out/dlls
             chown -R "$OWNER" /out'
  windows_version "$t"
}

windows_version() { # target
  local t=$1
  # It cannot run here; its version is the one it would print, read from the file. (No grep -q: see check_binary.)
  if strings -a "$BUILD/$t/qloudpulse-collect.exe" | grep -F "$LONG_VERSION" >/dev/null; then
    echo "qloudpulse-collect $LONG_VERSION" >"$BUILD/$t/version"
    echo "  (not run here: a Windows binary; its version is read from the file)"
  else
    echo "unknown" >"$BUILD/$t/version"
  fi
}

build_windows_arm64() { # target
  local t=$1 image
  command -v docker >/dev/null || fail "the Windows build needs Docker"
  mkdir -p "$BUILD/$t"
  # Debian's mingw-w64 has no arm64 compiler, so this is Rust's aarch64-pc-windows-gnullvm target (tier 2 with host
  # tools) linked with llvm-mingw, the LLVM toolchain the Rust docs name for it. The other route,
  # aarch64-pc-windows-msvc with cargo-xwin, downloads Microsoft's CRT and SDK under their licence on every machine
  # that builds; llvm-mingw is open source and pinned here by digest. ring compiles its arm64 Windows assembly
  # (pregenerated *-win64.S) with clang, which llvm-mingw is. +crt-static links libunwind in; the C runtime is the
  # UCRT, which Windows 10 and later carry (api-ms-win-crt-*.dll), as every Windows on ARM does.
  # The image is llvm-mingw with the client's Rust copied in, built once and kept by Docker. Its tag is a hash of the
  # two images it is made of: their names with digests are longer than the 128 characters a tag may have.
  image="qloudpulse-collect-llvm-mingw:$(printf '%s\n%s\n%s\n' "$LLVM_MINGW_IMAGE" "$WINDOWS_IMAGE" "$CARGO_AUDITABLE_VERSION" \
    | shasum -a 256 | cut -c1-16)"
  docker build -q -t "$image" - >/dev/null <<EOF
FROM $WINDOWS_IMAGE AS rust
FROM $LLVM_MINGW_IMAGE
COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo /usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:\$PATH
RUN rustup target add $t && cargo install --locked --quiet cargo-auditable@$CARGO_AUDITABLE_VERSION
EOF
  docker run --rm \
    -v "$CLI/..:/src:ro" -v qloudpulse-collect-cargo:/usr/local/cargo/registry -v "$BUILD/$t:/out" -e OWNER="$OWNER" \
    -e QLOUDPULSE_COMMIT -e QLOUDPULSE_COMMIT_DATE -w /src/collect -e CARGO_TARGET_DIR=/tmp/target \
    -e RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-Wl,--no-insert-timestamp" \
    -e CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER=aarch64-w64-mingw32-clang \
    -e CC_aarch64_pc_windows_gnullvm=aarch64-w64-mingw32-clang -e AR_aarch64_pc_windows_gnullvm=llvm-ar \
    -e T="$t" "$image" \
    sh -euc 'cargo auditable build --release --locked --quiet --target "$T"
             cp "/tmp/target/$T/release/qloudpulse-collect.exe" /out/qloudpulse-collect.exe
             llvm-readobj --coff-imports /out/qloudpulse-collect.exe | sed -n "s/^[[:space:]]*Name: //p" | sort -u >/out/dlls
             chown -R "$OWNER" /out'
  windows_version "$t"
}

build_macos() { # target
  local t=$1
  [ "$(uname -s)" = Darwin ] || fail "$t is built on macOS only"
  rustup toolchain list | grep -q "^$MACOS_TOOLCHAIN-" || rustup toolchain install "$MACOS_TOOLCHAIN" --profile minimal
  rustup +"$MACOS_TOOLCHAIN" target list --installed | grep -qx "$t" || rustup +"$MACOS_TOOLCHAIN" target add "$t"
  host_tool cargo-auditable "$CARGO_AUDITABLE_VERSION"
  (cd "$CLI" && PATH="$TOOLS/bin:$PATH" RUSTUP_TOOLCHAIN="$MACOS_TOOLCHAIN" RUSTFLAGS="$REMAP" \
    CARGO_TARGET_DIR="$CLI/target/release-build" cargo auditable build --release --locked --quiet --target "$t")
  mkdir -p "$BUILD/$t"
  cp "$CLI/target/release-build/$t/release/qloudpulse-collect" "$BUILD/$t/qloudpulse-collect"
  case "$t" in
    "$(uname -m | sed 's/arm64/aarch64/')"-apple-darwin) "$BUILD/$t/qloudpulse-collect" --version >"$BUILD/$t/version" ;;
    *)
      # The other architecture runs only under Rosetta; without it the file check above is what is checked.
      if ! "$BUILD/$t/qloudpulse-collect" --version >"$BUILD/$t/version" 2>/dev/null; then
        echo "qloudpulse-collect $LONG_VERSION" >"$BUILD/$t/version"
        echo "  (not run here: $t needs Rosetta)"
      fi
      ;;
  esac
}

for t in $TARGETS; do
  say "Build $t"
  case "$t" in
    *-linux-musl) build_linux "$t" ;;
    *-apple-darwin) build_macos "$t" ;;
    x86_64-pc-windows-gnu) build_windows "$t" ;;
    aarch64-pc-windows-gnullvm) build_windows_arm64 "$t" ;;
  esac
  case "$t" in *-windows-*) exe=qloudpulse-collect.exe ;; *) exe=qloudpulse-collect ;; esac
  check_binary "$t" "$BUILD/$t/$exe"
  [ "$(cat "$BUILD/$t/version")" = "qloudpulse-collect $LONG_VERSION" ] \
    || fail "$t says $(cat "$BUILD/$t/version"), not qloudpulse-collect $LONG_VERSION"
  if [ -s "$BUILD/$t/packages" ]; then
    echo "  built with $(tr '\n' ' ' <"$BUILD/$t/packages")"
  fi
  chmod 755 "$BUILD/$t/$exe"
  TZ=UTC touch -t "$STAMP" "$BUILD/$t/$exe"
  # No user name, extended attributes or build time in the archive.
  case "$t" in
    *-windows-*)
      (cd "$BUILD/$t" && TZ=UTC zip -q -X "$OUT/qloudpulse-collect-$VERSION-$t.zip" "$exe")
      echo "  qloudpulse-collect-$VERSION-$t.zip"
      ;;
    *)
      # gzip -n: tar -z would write the time of packing into the gzip header.
      COPYFILE_DISABLE=1 tar -cf - $TAR_OWNER -C "$BUILD/$t" "$exe" | gzip -n >"$OUT/qloudpulse-collect-$VERSION-$t.tar.gz"
      echo "  qloudpulse-collect-$VERSION-$t.tar.gz"
      ;;
  esac
done

write_installers "$OUT"
write_sums "$OUT"
rm -rf "$BUILD"

if [ -n "$COMPARE" ]; then
  say "Compare with $COMPARE"
  case "$COMPARE" in
    https://*) curl -fsSL --proto '=https' -o "$OUT/.compare" "$COMPARE" || fail "could not download $COMPARE" ;;
    *) cp "$COMPARE" "$OUT/.compare" || fail "no $COMPARE" ;;
  esac
  compare_sums "$OUT/.compare" "$OUT/SHA256SUMS"
  rm -f "$OUT/.compare"
fi
write_notes "$OUT" "on a maintainer's machine, for trying only: no release is built here, and these files have no provenance"

say "Built $TAG in $OUT"
cat "$OUT/SHA256SUMS"
printf '\nNothing was published. Releases are built and attested by the release workflow: %s --publish\n' "$0"

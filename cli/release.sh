#!/usr/bin/env bash
# Builds the qloudpulse client for every target, checks each binary, and packs one tarball per target with
# SHA256SUMS and release notes. With --publish it creates the release qloudpulse-cli-<version> in this repository
# from the commit it built. Without --publish nothing leaves this machine.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: cli/release.sh [--publish] [--targets "<target> ..."] [--out <dir>]

  --publish   create the release qloudpulse-cli-<version> on GitHub with the files it built
  --targets   build only these targets (default: all four below)
  --out       where the files go (default: cli/target/dist/<version>)

Targets:
  x86_64-unknown-linux-musl  aarch64-unknown-linux-musl   static, built in Docker
  aarch64-apple-darwin       x86_64-apple-darwin          built here with rustup (macOS only)

Environment:
  RUST_IMAGE   the image of the Linux builds (default rust:1.88-alpine, the client's rust-version)
  REPO         the GitHub repository to release in (default ulfmansson/qloudpulse-public)
EOF
}

ALL_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin"
TARGETS=$ALL_TARGETS
PUBLISH=0
OUT=""
RUST_IMAGE=${RUST_IMAGE:-rust:1.88-alpine}
REPO=${REPO:-ulfmansson/qloudpulse-public}

while [ $# -gt 0 ]; do
  case "$1" in
    --publish) PUBLISH=1 ;;
    --targets) TARGETS=${2:?--targets needs a list}; shift ;;
    --out) OUT=${2:?--out needs a directory}; shift ;;
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
TAG="qloudpulse-cli-$VERSION"
OUT=${OUT:-$CLI/target/dist/$VERSION}
COMMIT=$(git -C "$ROOT" rev-parse HEAD)

for t in $TARGETS; do
  case " $ALL_TARGETS " in *" $t "*) ;; *) fail "unknown target $t" ;; esac
done

# A release is built from a commit everyone can see, so its source is the tag's.
if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
  [ $PUBLISH -eq 0 ] || fail "the checkout has changes; commit or remove them before --publish"
  echo "Note: the checkout has changes, so this build is for trying only."
fi
if [ $PUBLISH -eq 1 ]; then
  command -v gh >/dev/null || fail "--publish needs gh"
  # The client's default address has to answer before a release names it (qloudpulse's ADR 0127).
  DEFAULT=$(sed -n 's/^pub const DEFAULT_ENDPOINT: &str = "\(.*\)";$/\1/p' "$CLI/src/send.rs")
  [ -n "$DEFAULT" ] || fail "no DEFAULT_ENDPOINT in cli/src/send.rs"
  STATUS=$(curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' \
    -d '{"formatVersion": 1, "types": []}' "$DEFAULT/catalogue" || true)
  [ "$STATUS" = 200 ] || fail "$DEFAULT/catalogue answers $STATUS, not 200; release once it is live"
  git -C "$ROOT" fetch -q origin main
  [ "$COMMIT" = "$(git -C "$ROOT" rev-parse origin/main)" ] \
    || fail "HEAD is not origin/main; release only what main has (git switch main && git pull)"
  if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    fail "$TAG is already released in $REPO; raise version in cli/Cargo.toml first"
  fi
fi

say "Tests"
(cd "$CLI" && cargo test --locked --quiet)

rm -rf "$OUT"
mkdir -p "$OUT"
BUILD="$OUT/.build"
mkdir -p "$BUILD"

# Paths of this machine are written into a binary's panic messages; map them to neutral ones so a public binary
# carries no user name.
REMAP="--remap-path-prefix=$CLI=/src --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$HOME=/home"
ME=$(id -un)

check_binary() { # target binary
  local t=$1 bin=$2 info
  info=$(file "$bin")
  case "$t" in
    x86_64-unknown-linux-musl) echo "$info" | grep -q 'ELF 64-bit.*x86-64' || fail "$t: not an x86-64 ELF: $info" ;;
    aarch64-unknown-linux-musl) echo "$info" | grep -q 'ELF 64-bit.*aarch64' || fail "$t: not an arm64 ELF: $info" ;;
    aarch64-apple-darwin) echo "$info" | grep -q 'Mach-O 64-bit.*arm64' || fail "$t: not an arm64 Mach-O: $info" ;;
    x86_64-apple-darwin) echo "$info" | grep -q 'Mach-O 64-bit.*x86_64' || fail "$t: not an x86_64 Mach-O: $info" ;;
  esac
  case "$t" in
    *-linux-musl) echo "$info" | grep -q 'statically linked\|static-pie linked' || fail "$t: not static: $info" ;;
  esac
  if strings "$bin" | grep -qF -e "$HOME" -e "$ME" -e "${CARGO_HOME:-$HOME/.cargo}"; then
    fail "$t: the binary holds a path of this machine ($HOME, the user name or CARGO_HOME)"
  fi
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
  docker run --rm --platform "$platform" \
    -v "$CLI:/src:ro" -v qloudpulse-cli-cargo:/usr/local/cargo/registry -v "$BUILD/$t:/out" \
    -w /src -e CARGO_TARGET_DIR=/tmp/target "$RUST_IMAGE" \
    sh -euc 'apk add --no-cache musl-dev >/dev/null
             cargo build --release --locked --quiet
             cp /tmp/target/release/qloudpulse /out/qloudpulse
             /out/qloudpulse --version' >"$BUILD/$t/version"
}

build_macos() { # target
  local t=$1
  [ "$(uname -s)" = Darwin ] || fail "$t is built on macOS only"
  rustup target list --installed | grep -qx "$t" || rustup target add "$t"
  (cd "$CLI" && RUSTFLAGS="$REMAP" CARGO_TARGET_DIR="$CLI/target/release-build" \
    cargo build --release --locked --quiet --target "$t")
  mkdir -p "$BUILD/$t"
  cp "$CLI/target/release-build/$t/release/qloudpulse" "$BUILD/$t/qloudpulse"
  case "$t" in
    "$(uname -m | sed 's/arm64/aarch64/')"-apple-darwin) "$BUILD/$t/qloudpulse" --version >"$BUILD/$t/version" ;;
    *)
      # The other architecture runs only under Rosetta; without it the file check above is what is checked.
      if ! "$BUILD/$t/qloudpulse" --version >"$BUILD/$t/version" 2>/dev/null; then
        echo "qloudpulse $VERSION" >"$BUILD/$t/version"
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
  esac
  check_binary "$t" "$BUILD/$t/qloudpulse"
  [ "$(cat "$BUILD/$t/version")" = "qloudpulse $VERSION" ] \
    || fail "$t says $(cat "$BUILD/$t/version"), not qloudpulse $VERSION"
  chmod 755 "$BUILD/$t/qloudpulse"
  # No user name or extended attributes in the archive.
  COPYFILE_DISABLE=1 tar -czf "$OUT/qloudpulse-$VERSION-$t.tar.gz" \
    --uid 0 --gid 0 --uname root --gname root -C "$BUILD/$t" qloudpulse
  echo "  qloudpulse-$VERSION-$t.tar.gz"
done

(cd "$OUT" && shasum -a 256 qloudpulse-*.tar.gz >SHA256SUMS)
rm -rf "$BUILD"

cat >"$OUT/notes.md" <<EOF
The qloudpulse client $VERSION: \`qloudpulse estimate\` prices a Pulumi change before it is applied.
Built from $COMMIT.

\`\`\`bash
VERSION=$VERSION
TARGET=x86_64-unknown-linux-musl   # aarch64-unknown-linux-musl, aarch64-apple-darwin, x86_64-apple-darwin
BASE=https://github.com/$REPO/releases/download/$TAG
curl -fsSLO "\$BASE/qloudpulse-\$VERSION-\$TARGET.tar.gz"
curl -fsSLO "\$BASE/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf "qloudpulse-\$VERSION-\$TARGET.tar.gz"
\`\`\`

How to use it: [cli/README.md](https://github.com/$REPO/blob/$TAG/cli/README.md).
EOF

say "Built $TAG in $OUT"
cat "$OUT/SHA256SUMS"

if [ $PUBLISH -eq 0 ]; then
  printf '\nNothing was published. To release it: %s --publish\n' "$0"
  exit 0
fi

printf '\nPublish %s in %s, tagged on %s? [y/N] ' "$TAG" "$REPO" "$COMMIT"
read -r answer
[ "$answer" = y ] || [ "$answer" = Y ] || fail "not published"
# --latest=false: the repository's "latest" stays with the cluster agent, whose install links name their version.
gh release create "$TAG" -R "$REPO" --target "$COMMIT" --latest=false \
  --title "qloudpulse client $VERSION" --notes-file "$OUT/notes.md" \
  "$OUT"/qloudpulse-*.tar.gz "$OUT/SHA256SUMS"

printf '\nReleased. Clients are told once qloudpulse names it: set estimates.client.latest to %s in the report\n' "$VERSION"
printf "service's application.yml (ESTIMATES_CLIENT_LATEST on an install), and deploy.\n"

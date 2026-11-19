#!/bin/sh
# Installs the qloudpulse client on Linux or macOS: picks the archive for this machine, checks it against the release's
# SHA256SUMS, and puts the binary in ~/.local/bin (or --dir).
#
#   curl -fsSL https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-cli-<version>/install.sh | sh
#
# The copy attached to a release installs that release; this copy in the repository needs --version. On Windows, use
# install.ps1 from the same release. Plain POSIX sh: it runs under dash, busybox and macOS's sh.
set -eu

# A release's copy has its version here (cli/release.sh writes it); this one has none.
DEFAULT_VERSION=""
REPO=ulfmansson/qloudpulse-public

usage() {
  cat <<'EOF'
Usage: install.sh [--version <version>] [--dir <dir>] [--attest] [--base <url or dir>]

  --version  the client version to install (default: the release this script came with)
  --dir      where the binary goes (default: $QLOUDPULSE_INSTALL_DIR, or ~/.local/bin)
  --attest   also check the archive's signed build provenance with gh (gh attestation verify); needs gh signed in
  --base     where the archives and SHA256SUMS are, a URL or a directory (default: $QLOUDPULSE_DOWNLOAD_BASE, or
             the release on GitHub)

With curl ... | sh, pass options after "sh -s --", e.g. curl -fsSL <url> | sh -s -- --dir /usr/local/bin
EOF
}

say() { printf 'qloudpulse install: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }

VERSION=${QLOUDPULSE_VERSION:-$DEFAULT_VERSION}
DIR=${QLOUDPULSE_INSTALL_DIR:-}
BASE=${QLOUDPULSE_DOWNLOAD_BASE:-}
ATTEST=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || fail "--version needs a version"; VERSION=$2; shift ;;
    --dir) [ $# -ge 2 ] || fail "--dir needs a directory"; DIR=$2; shift ;;
    --base) [ $# -ge 2 ] || fail "--base needs a URL or directory"; BASE=$2; shift ;;
    --attest) ATTEST=1 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; fail "unknown argument: $1" ;;
  esac
  shift
done

[ -n "$VERSION" ] || fail "no version: pass --version <version> (this copy is not from a release)"
case "$VERSION" in
  v*|qloudpulse-cli-*) VERSION=${VERSION#v}; VERSION=${VERSION#qloudpulse-cli-} ;;
esac
case "$VERSION" in
  *[!0-9.]*|"") fail "not a version: $VERSION (for example 0.2.0)" ;;
esac
TAG="qloudpulse-cli-$VERSION"
BASE=${BASE:-https://github.com/$REPO/releases/download/$TAG}
if [ -z "$DIR" ]; then
  [ -n "${HOME:-}" ] || fail "no HOME; pass --dir"
  DIR="$HOME/.local/bin"
fi

# The machine's target, as the archives name it.
os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux) os=unknown-linux-musl ;;
  Darwin)
    os=apple-darwin
    # A shell running under Rosetta reports x86_64 on an Apple silicon Mac; the native build is the one to have.
    if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
      arch=arm64
    fi
    ;;
  MINGW*|MSYS*|CYGWIN*) fail "this is Windows: use install.ps1 from the same release (see cli/README.md)" ;;
  *) fail "no build for $os; build from source (cli/README.md)" ;;
esac
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) fail "no build for $arch on $os; build from source (cli/README.md)" ;;
esac
TARGET="$arch-$os"
ARCHIVE="qloudpulse-$VERSION-$TARGET.tar.gz"

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  fail "needs sha256sum or shasum to check the download"
fi

fetch() { # name destination
  case "$BASE" in
    http://*|https://*)
      if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$BASE/$1" || fail "could not download $BASE/$1"
      elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$BASE/$1" || fail "could not download $BASE/$1"
      else
        fail "needs curl or wget"
      fi
      ;;
    *) cp "$BASE/$1" "$2" 2>/dev/null || fail "no $1 in $BASE" ;;
  esac
}

TMP=$(mktemp -d "${TMPDIR:-/tmp}/qloudpulse.XXXXXX") || fail "cannot make a temporary directory"
trap 'rm -rf "$TMP"' EXIT INT TERM

say "qloudpulse $VERSION for $TARGET"
fetch "$ARCHIVE" "$TMP/$ARCHIVE"
fetch SHA256SUMS "$TMP/SHA256SUMS"

# The line for this archive, compared in full: not every sha256sum has --ignore-missing.
want=$(awk -v f="$ARCHIVE" '$2 == f || $2 == "*" f { print $1 }' "$TMP/SHA256SUMS")
[ -n "$want" ] || fail "SHA256SUMS names no $ARCHIVE"
got=$(sha256 "$TMP/$ARCHIVE")
[ "$got" = "$want" ] || fail "$ARCHIVE does not match SHA256SUMS (got $got, want $want); nothing was installed"

if [ $ATTEST -eq 1 ]; then
  command -v gh >/dev/null 2>&1 || fail "--attest needs gh (https://cli.github.com)"
  gh attestation verify "$TMP/$ARCHIVE" --repo "$REPO" \
    --signer-workflow "$REPO/.github/workflows/cli-release.yml" --source-ref "refs/tags/$TAG" >&2 \
    || fail "$ARCHIVE has no provenance from $REPO's release workflow at $TAG; nothing was installed"
fi

tar -xzf "$TMP/$ARCHIVE" -C "$TMP" qloudpulse
"$TMP/qloudpulse" --version >/dev/null 2>&1 \
  || fail "the downloaded qloudpulse does not run here (if ${TMPDIR:-/tmp} is mounted noexec, set TMPDIR to another directory)"

mkdir -p "$DIR" || fail "cannot create $DIR; pass --dir, or run with the rights to write there"
# Copied next to the old one and moved over it, so a running qloudpulse is never half written.
cp "$TMP/qloudpulse" "$DIR/.qloudpulse.new" || fail "cannot write to $DIR; pass --dir, or run with the rights to write there"
chmod 755 "$DIR/.qloudpulse.new"
mv -f "$DIR/.qloudpulse.new" "$DIR/qloudpulse"
say "installed $("$DIR/qloudpulse" --version) as $DIR/qloudpulse"

# On a GitHub Actions runner, the later steps of the job find it.
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$DIR" >>"$GITHUB_PATH"
fi
case ":${PATH:-}:" in
  *":$DIR:"*) ;;
  *) [ -n "${GITHUB_PATH:-}" ] || say "$DIR is not on PATH; add it, e.g. export PATH=\"$DIR:\$PATH\" in your shell's profile" ;;
esac

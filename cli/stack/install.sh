#!/bin/sh
# Installs the qloudpulse client on Linux or macOS: picks the archive for this machine, checks it against the release's
# SHA256SUMS, and puts the binary in ~/.local/bin (or --dir).
#
#   curl -fsSL https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-stack-<version>/install.sh | sh
#
# The copy attached to a release installs that release; this copy in the repository installs the newest release, or
# --version. When a client is already installed it says which version that is and asks before replacing it. On
# Windows, use install.ps1 from the same release. Plain POSIX sh: it runs under dash, busybox and macOS's sh.
set -eu

# A release's copy has its version here (cli/stack/release.sh writes it); this one has none.
DEFAULT_VERSION=""
REPO=ulfmansson/qloudpulse-public
NAME=qloudpulse-stack
# Up to 0.3.1 the client was called qloudpulse: those releases are tagged qloudpulse-cli-<version>, their archives
# are qloudpulse-<version>-<target>.tar.gz and the binary in them is qloudpulse.
LEGACY_NAME=qloudpulse
LEGACY_TAG=qloudpulse-cli
FIRST_VERSION=0.4.0

usage() {
  cat <<'EOF'
Usage: install.sh [--version <version>] [--dir <dir>] [--yes] [--attest] [--base <url or dir>]

  --version  the client version to install (default: the release this script came with; for the copy in the
             repository, the newest release)
  --dir      where the binary goes (default: $QLOUDPULSE_INSTALL_DIR, or ~/.local/bin)
  --yes      replace an installed client with another version without asking (also $QLOUDPULSE_ASSUME_YES=1)
  --attest   also check the archive's signed build provenance with gh (gh attestation verify); needs gh signed in
  --base     where the archives and SHA256SUMS are, a URL or a directory (default: $QLOUDPULSE_DOWNLOAD_BASE, or
             the release on GitHub)

When a client is already installed (qloudpulse-stack, or qloudpulse up to 0.3.1), it says which version that is and
which this would install, and asks before replacing it; one already at that version is left alone. --version, or no
terminal to ask on (a pipeline), goes ahead without asking.

With curl ... | sh, pass options after "sh -s --", e.g. curl -fsSL <url> | sh -s -- --dir /usr/local/bin
EOF
}

say() { printf 'qloudpulse install: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }

VERSION=${QLOUDPULSE_VERSION:-}
DIR=${QLOUDPULSE_INSTALL_DIR:-}
BASE=${QLOUDPULSE_DOWNLOAD_BASE:-}
ATTEST=0
case "${QLOUDPULSE_ASSUME_YES:-0}" in 0|"") YES=0 ;; *) YES=1 ;; esac
while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || fail "--version needs a version"; VERSION=$2; shift ;;
    --dir) [ $# -ge 2 ] || fail "--dir needs a directory"; DIR=$2; shift ;;
    --base) [ $# -ge 2 ] || fail "--base needs a URL or directory"; BASE=$2; shift ;;
    --yes|-y) YES=1 ;;
    --attest) ATTEST=1 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; fail "unknown argument: $1" ;;
  esac
  shift
done

# Where the version comes from: asked for (--version or $QLOUDPULSE_VERSION), this installer's release, or the newest.
if [ -n "$VERSION" ]; then
  FROM=asked
elif [ -n "$DEFAULT_VERSION" ]; then
  FROM=release VERSION=$DEFAULT_VERSION
else
  FROM=newest
fi

version_cmp() { # a b: prints -1, 0 or 1 as version a is older than, the same as or newer than b
  awk -v a="$1" -v b="$2" 'BEGIN {
    n = split(a, x, "."); m = split(b, y, "."); k = n > m ? n : m
    for (i = 1; i <= k; i++) { if (x[i] + 0 < y[i] + 0) { print -1; exit } if (x[i] + 0 > y[i] + 0) { print 1; exit } }
    print 0
  }'
}

TMP=$(mktemp -d "${TMPDIR:-/tmp}/$NAME.XXXXXX") || fail "cannot make a temporary directory"
trap 'rm -rf "$TMP"' EXIT INT TERM

releases() { # the repository's releases, as GitHub's API lists them
  url="https://api.github.com/repos/$REPO/releases?per_page=100"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 -H 'Accept: application/vnd.github+json' "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O - --header 'Accept: application/vnd.github+json' "$url"
  else
    say "needs curl or wget"
    return 1
  fi
}

if [ "$FROM" = newest ]; then
  [ -z "$BASE" ] || fail "no version: pass --version <version> with --base"
  releases >"$TMP/releases.json" || fail "could not list the releases on github.com/$REPO (offline, or past" \
    "GitHub's 60 requests an hour without a token?); pass --version <version>"
  # The newest client release, under either name. Not GitHub's "latest": that may be another tool's release, in the
  # same repository. The API's JSON has one field per line, so sed finds the tags without a JSON parser.
  VERSION=$(sed -n -e "s/^ *\"tag_name\": *\"$NAME-\([0-9][0-9.]*\)\",*\$/\1/p" \
    -e "s/^ *\"tag_name\": *\"$LEGACY_TAG-\([0-9][0-9.]*\)\",*\$/\1/p" "$TMP/releases.json" |
    sort -t . -k 1,1n -k 2,2n -k 3,3n | tail -n 1)
  [ -n "$VERSION" ] || fail "github.com/$REPO has no $NAME release yet; pass --version <version>"
fi
case "$VERSION" in
  v*|"$NAME"-*|"$LEGACY_TAG"-*) VERSION=${VERSION#v}; VERSION=${VERSION#"$NAME"-}; VERSION=${VERSION#"$LEGACY_TAG"-} ;;
esac
case "$VERSION" in
  *[!0-9.]*|"") fail "not a version: $VERSION (for example 0.4.0)" ;;
esac
if [ "$(version_cmp "$VERSION" "$FIRST_VERSION")" -lt 0 ]; then
  BIN=$LEGACY_NAME TAG="$LEGACY_TAG-$VERSION" WORKFLOW=cli-release.yml
else
  BIN=$NAME TAG="$NAME-$VERSION" WORKFLOW=cli-stack-release.yml
fi
BASE=${BASE:-https://github.com/$REPO/releases/download/$TAG}
DIR_GIVEN=0
[ -z "$DIR" ] || DIR_GIVEN=1
if [ -z "$DIR" ]; then
  [ -n "${HOME:-}" ] || fail "no HOME; pass --dir"
  DIR="$HOME/.local/bin"
fi

# A client already installed: in DIR first, then on PATH, under either name. OLD is its path, OLD_VERSION its version.
# On PATH too, unless --dir (or $QLOUDPULSE_INSTALL_DIR) names where it goes: one elsewhere is then another install.
on_path() { [ "$DIR_GIVEN" = 1 ] || command -v "$1" 2>/dev/null || true; }
OLD="" OLD_VERSION=""
for f in "$DIR/$NAME" "$DIR/$LEGACY_NAME" "$(on_path "$NAME")" "$(on_path "$LEGACY_NAME")"; do
  [ -n "$f" ] && [ -f "$f" ] && [ -x "$f" ] || continue
  # "qloudpulse-stack 0.4.0 (1ab39062b 2026-11-21)", or "qloudpulse 0.3.1" before the rename.
  v=$("$f" --version 2>/dev/null | sed -n '1s/^[^ ]* \([0-9][0-9.]*\).*/\1/p') || v=""
  [ -n "$v" ] || continue
  OLD=$f OLD_VERSION=$v
  break
done

confirm() { # question y|n (the answer to an empty line): asks on the terminal; with none to ask on, goes ahead
  if ! (: </dev/tty) 2>/dev/null; then
    say "no terminal to ask on; going ahead"
    return 0
  fi
  case "$2" in y) hint='[Y/n]' ;; *) hint='[y/N]' ;; esac
  printf 'qloudpulse install: %s %s ' "$1" "$hint" >&2
  answer=""
  read -r answer </dev/tty || answer=""
  case "${answer:-$2}" in
    [yY]|[yY][eE][sS]) return 0 ;;
    *) return 1 ;;
  esac
}

if [ -n "$OLD" ]; then
  case "$FROM" in
    newest) offer="the newest release is $VERSION" ;;
    release) offer="this installer is for $VERSION" ;;
    *) offer="you asked for $VERSION" ;;
  esac
  case "$(version_cmp "$OLD_VERSION" "$VERSION")" in
    0)
      say "$OLD is $OLD_VERSION and $offer: already up to date, nothing to do"
      exit 0
      ;;
    -1) question="Upgrade to $VERSION?" default=y ;;
    *) question="Replace it with the older $VERSION?" default=n ;;
  esac
  say "$OLD is $OLD_VERSION; $offer"
  if [ "$FROM" != asked ] && [ $YES -eq 0 ]; then
    confirm "$question" "$default" || { say "left $OLD at $OLD_VERSION"; exit 0; }
  fi
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
  MINGW*|MSYS*|CYGWIN*) fail "this is Windows: use install.ps1 from the same release (see cli/stack/README.md)" ;;
  *) fail "no build for $os; build from source (cli/stack/README.md)" ;;
esac
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) fail "no build for $arch on $os; build from source (cli/stack/README.md)" ;;
esac
TARGET="$arch-$os"
ARCHIVE="$BIN-$VERSION-$TARGET.tar.gz"

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

say "$BIN $VERSION for $TARGET"
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
    --signer-workflow "$REPO/.github/workflows/$WORKFLOW" --source-ref "refs/tags/$TAG" >&2 \
    || fail "$ARCHIVE has no provenance from $REPO's release workflow at $TAG; nothing was installed"
fi

tar -xzf "$TMP/$ARCHIVE" -C "$TMP" "$BIN" || fail "$ARCHIVE has no $BIN in it"
"$TMP/$BIN" --version >/dev/null 2>&1 \
  || fail "the downloaded $BIN does not run here (if ${TMPDIR:-/tmp} is mounted noexec, set TMPDIR to another directory)"

mkdir -p "$DIR" || fail "cannot create $DIR; pass --dir, or run with the rights to write there"
# Copied next to the old one and moved over it, so a running client is never half written.
cp "$TMP/$BIN" "$DIR/.$BIN.new" || fail "cannot write to $DIR; pass --dir, or run with the rights to write there"
chmod 755 "$DIR/.$BIN.new"
mv -f "$DIR/.$BIN.new" "$DIR/$BIN"
say "installed $("$DIR/$BIN" --version) as $DIR/$BIN"
# One that was somewhere else, or under the old name, is still there.
if [ -n "$OLD" ] && [ "$OLD" != "$DIR/$BIN" ]; then
  say "$OLD ($OLD_VERSION) is still there; remove it if you no longer need it: rm \"$OLD\""
fi

# On a GitHub Actions runner, the later steps of the job find it.
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$DIR" >>"$GITHUB_PATH"
fi
case ":${PATH:-}:" in
  *":$DIR:"*) ;;
  *) [ -n "${GITHUB_PATH:-}" ] || say "$DIR is not on PATH; add it, e.g. export PATH=\"$DIR:\$PATH\" in your shell's profile" ;;
esac

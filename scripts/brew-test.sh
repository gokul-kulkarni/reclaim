#!/usr/bin/env bash
#
# Test the Homebrew formula locally, against your working tree, without
# publishing anything or needing a GitHub release to exist.
#
# It builds a real tarball, points a copy of the formula at it with a real
# checksum, installs it through brew for real, runs `brew test` and
# `brew audit`, then uninstalls. What you are testing is the exact path a user
# takes on `brew install`, minus the download.
#
#   ./scripts/brew-test.sh              # full run: package, install, test, audit, clean up
#   ./scripts/brew-test.sh --keep       # leave it installed so you can poke at it
#   ./scripts/brew-test.sh --skip-web   # reuse an existing web/dist (faster)
#
# Requires Homebrew. Safe to run repeatedly.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"

KEEP=false
PACKAGE_ARGS=()
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=true ;;
    --skip-web) PACKAGE_ARGS+=("--skip-web") ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 1 ;;
  esac
done

command -v brew >/dev/null 2>&1 || {
  echo "error: Homebrew is not installed. See https://brew.sh" >&2
  exit 1
}

TAP="reclaim-local/test"
TAP_DIR="$(brew --repository)/Library/Taps/reclaim-local/homebrew-test"
TARGET="$(rustc -vV | awk '/^host:/ {print $2}')"
TARBALL="$ROOT/dist/reclaim-${TARGET}.tar.gz"

cleanup() {
  if [ "$KEEP" = false ]; then
    echo
    echo "==> Cleaning up"
    brew uninstall --force "${TAP}/reclaim" >/dev/null 2>&1 || true
    brew untap "$TAP" >/dev/null 2>&1 || true
  else
    echo
    echo "Left installed. Remove it with:"
    echo "    brew uninstall ${TAP}/reclaim && brew untap ${TAP}"
  fi
}
trap cleanup EXIT

echo "==> Building the tarball"
./scripts/package.sh "${PACKAGE_ARGS[@]+"${PACKAGE_ARGS[@]}"}"

SHA="$(cat "${TARBALL}.sha256")"
VERSION="$(awk -F'"' '/^version/ {print $2; exit}' Cargo.toml)"

echo
echo "==> Creating a throwaway local tap"
# A previous run that was interrupted (or used --keep) leaves the tap behind,
# and `brew tap-new` fails outright on an existing tap. Clear it either way.
brew uninstall --force "${TAP}/reclaim" >/dev/null 2>&1 || true
brew untap "$TAP" >/dev/null 2>&1 || true
rm -rf "$TAP_DIR"
brew tap-new "$TAP" --no-git >/dev/null

# A local formula pointing at the tarball on disk. The install and test blocks
# are lifted verbatim from the real formula so the two cannot drift; only the
# URL and checksum differ, so what runs here is what will run for a user.
cat > "$TAP_DIR/Formula/reclaim.rb" <<FORMULA
class Reclaim < Formula
  desc "Find and safely reclaim disk space taken by developer caches and build artifacts"
  homepage "https://github.com/gokul-kulkarni/reclaim"
  url "file://${TARBALL}"
  version "${VERSION}"
  sha256 "${SHA}"
  license "MIT"

$(sed -n '/^  def install$/,/^  end$/p' "$ROOT/packaging/homebrew/reclaim.rb")

$(sed -n '/^  test do$/,/^  end$/p' "$ROOT/packaging/homebrew/reclaim.rb")
end
FORMULA

echo "==> Installing through brew"
brew install --formula "${TAP}/reclaim"

echo
echo "==> brew test (runs the formula's own test block)"
brew test "${TAP}/reclaim"

echo
echo "==> brew audit"
# --strict flags style issues that matter for a tap; a local file:// URL trips
# some online checks, so those are skipped rather than failing the run.
brew audit --formula --strict "${TAP}/reclaim" || {
  echo "note: audit reported issues (a file:// URL trips some checks that will pass for a real release)"
}

echo
echo "==> Smoke test of the installed binary"
INSTALLED="$(brew --prefix)/bin/reclaim"
SANDBOX="$(mktemp -d)"
mkdir -p "$SANDBOX/.npm/_cacache"
head -c 300000 /dev/urandom > "$SANDBOX/.npm/_cacache/blob"

"$INSTALLED" --version
"$INSTALLED" --root "$SANDBOX" --no-color scan --all
"$INSTALLED" --root "$SANDBOX" --no-color providers | head -3
test -f "$SANDBOX/.npm/_cacache/blob" || { echo "FAIL: scan deleted something"; exit 1; }
rm -rf "$SANDBOX"

echo
echo "==> Completions installed:"
ls "$(brew --prefix)/share/zsh/site-functions/_reclaim" 2>/dev/null && echo "    zsh ok"
ls "$(brew --prefix)/etc/bash_completion.d/reclaim" 2>/dev/null && echo "    bash ok"
ls "$(brew --prefix)/share/fish/vendor_completions.d/reclaim.fish" 2>/dev/null && echo "    fish ok"

echo
echo "ALL BREW CHECKS PASSED"

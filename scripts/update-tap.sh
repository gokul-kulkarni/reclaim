#!/usr/bin/env bash
#
# Render packaging/homebrew/reclaim.rb with real checksums from a GitHub
# release and push it to the homebrew-tap repo.
#
# Run this last, after CI has built the Linux targets (automatic, on tag push)
# and ./scripts/release-macos.sh has attached the two macOS targets. This
# script itself doesn't care which of those two paths produced a given
# tarball — it only reads the four .sha256 files already sitting on the
# release, so it works regardless of build order.
#
#   ./scripts/update-tap.sh 0.1.0
#   ./scripts/update-tap.sh v0.1.0    # a leading 'v' is stripped either way

set -euo pipefail

cd "$(dirname "$0")/.."

if [ $# -ne 1 ]; then
  echo "usage: $0 <version>   e.g. $0 0.1.0" >&2
  exit 1
fi
VERSION="${1#v}"
TAG="v${VERSION}"
REPO="gokul-kulkarni/reclaim"
TAP_REPO="gokul-kulkarni/homebrew-tap"
FORMULA_SRC="packaging/homebrew/reclaim.rb"

command -v gh >/dev/null 2>&1 || { echo "error: gh CLI is required" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "error: gh is not authenticated (run: gh auth login)" >&2; exit 1; }

TARGETS=(aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-gnu)

echo "==> Checking release ${TAG} has all four platform tarballs"
ASSETS="$(gh release view "$TAG" --repo "$REPO" --json assets --jq '.assets[].name')"
MISSING=()
for target in "${TARGETS[@]}"; do
  echo "$ASSETS" | grep -qx "reclaim-${target}.tar.gz.sha256" || MISSING+=("$target")
done
if [ "${#MISSING[@]}" -gt 0 ]; then
  echo "error: release ${TAG} is missing checksums for: ${MISSING[*]}" >&2
  echo "       Linux targets come from CI automatically; macOS targets need" >&2
  echo "       ./scripts/release-macos.sh ${TAG} run first." >&2
  exit 1
fi

echo "==> Downloading checksums"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
gh release download "$TAG" --repo "$REPO" --pattern '*.sha256' --dir "$TMP" --clobber

echo "==> Rendering the formula"
RENDERED="$TMP/reclaim.rb"
cp "$FORMULA_SRC" "$RENDERED"
sed -i.bak "s/^  version \".*\"/  version \"${VERSION}\"/" "$RENDERED" && rm "${RENDERED}.bak"

for target in "${TARGETS[@]}"; do
  sha_file="$TMP/reclaim-${target}.tar.gz.sha256"
  sha="$(awk '{print $1}' "$sha_file")"
  placeholder="REPLACE_WITH_$(echo "$target" | tr 'a-z-' 'A-Z_')_SHA256"
  sed -i.bak "s/${placeholder}/${sha}/" "$RENDERED" && rm "${RENDERED}.bak"
done

if grep -q REPLACE_WITH "$RENDERED"; then
  echo "error: formula still has unsubstituted placeholders:" >&2
  grep REPLACE_WITH "$RENDERED" >&2
  exit 1
fi

echo "==> Cloning ${TAP_REPO}"
TAP_DIR="$TMP/tap"
gh repo clone "$TAP_REPO" "$TAP_DIR" -- --quiet

mkdir -p "$TAP_DIR/Formula"
cp "$RENDERED" "$TAP_DIR/Formula/reclaim.rb"

echo "==> Committing and pushing"
git -C "$TAP_DIR" add Formula/reclaim.rb
if git -C "$TAP_DIR" diff --staged --quiet; then
  echo "Formula is already up to date for ${VERSION}; nothing to push."
  exit 0
fi
git -C "$TAP_DIR" commit -m "reclaim ${VERSION}"
git -C "$TAP_DIR" push

echo
echo "Done. Verify with:"
echo "  brew update && brew upgrade reclaim   # or: brew install gokul-kulkarni/tap/reclaim"

#!/usr/bin/env bash
#
# Build both macOS targets locally and attach them to an existing GitHub
# release, in place of building them on GitHub's macOS runners.
#
# Why: GitHub's macOS runner capacity — especially for the Intel
# (x86_64-apple-darwin) image — is small and has queued indefinitely in
# practice. Both macOS targets build natively from a single Apple Silicon
# machine via Xcode's toolchain (no Docker, no second Mac, no queue), so CI
# only builds the two Linux targets and this script covers the other two.
#
#   ./scripts/release-macos.sh v0.1.0
#
# Requires: this repo checked out at the tagged commit, the gh CLI
# authenticated, and a GitHub release for that tag already existing (the
# release workflow's `publish` job creates it from the Linux builds).
#
# Run ./scripts/update-tap.sh <version> afterwards to render and push the
# Homebrew formula once all four platform tarballs are attached.

set -euo pipefail

cd "$(dirname "$0")/.."

if [ $# -ne 1 ]; then
  echo "usage: $0 <version-tag>   e.g. $0 v0.1.0" >&2
  exit 1
fi
TAG="$1"
REPO="gokul-kulkarni/reclaim"

command -v gh >/dev/null 2>&1 || { echo "error: gh CLI is required" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "error: gh is not authenticated (run: gh auth login)" >&2; exit 1; }

if [ "$(uname -s)" != "Darwin" ]; then
  echo "error: this script builds macOS binaries and must run on a Mac" >&2
  exit 1
fi

echo "==> Checking release ${TAG} exists"
gh release view "$TAG" --repo "$REPO" >/dev/null || {
  echo "error: no release ${TAG} on ${REPO} yet. The release workflow's" >&2
  echo "       'publish' job creates it from the Linux builds — wait for that" >&2
  echo "       to finish first." >&2
  exit 1
}

echo "==> Verifying the working tree matches ${TAG}"
TAG_SHA="$(git rev-parse "${TAG}^{commit}")"
HEAD_SHA="$(git rev-parse HEAD)"
if [ "$TAG_SHA" != "$HEAD_SHA" ]; then
  echo "error: HEAD ($HEAD_SHA) is not $TAG ($TAG_SHA)." >&2
  echo "       Check out the tag first: git checkout $TAG" >&2
  exit 1
fi

TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

echo "==> Building frontend once, shared by both targets"
npm --prefix web ci --silent
npm --prefix web run build --silent

for target in "${TARGETS[@]}"; do
  echo
  echo "==> Building ${target}"
  ./scripts/package.sh --target "$target" --skip-web
done

echo
echo "==> Uploading to release ${TAG}"
ASSETS=()
for target in "${TARGETS[@]}"; do
  ASSETS+=("dist/reclaim-${target}.tar.gz" "dist/reclaim-${target}.tar.gz.sha256")
done
gh release upload "$TAG" "${ASSETS[@]}" --repo "$REPO" --clobber

echo
echo "==> Uploaded:"
for target in "${TARGETS[@]}"; do
  echo "    reclaim-${target}.tar.gz  sha256: $(cat "dist/reclaim-${target}.tar.gz.sha256")"
done
echo
echo "Next: once all four platform tarballs are attached (check with"
echo "  gh release view ${TAG} --repo ${REPO}"
echo "), run:"
echo "  ./scripts/update-tap.sh ${TAG#v}"

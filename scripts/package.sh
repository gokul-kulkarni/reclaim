#!/usr/bin/env bash
#
# Build a release tarball for the host platform, in exactly the layout the
# release workflow produces and the Homebrew formula expects.
#
#   ./scripts/package.sh            -> dist/reclaim-<target>.tar.gz (+ .sha256)
#   ./scripts/package.sh --skip-web -> reuse an existing web/dist
#
# Run this before ./scripts/brew-test.sh.

set -euo pipefail

cd "$(dirname "$0")/.."

SKIP_WEB=false
for arg in "$@"; do
  case "$arg" in
    --skip-web) SKIP_WEB=true ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 1 ;;
  esac
done

TARGET="$(rustc -vV | awk '/^host:/ {print $2}')"
NAME="reclaim-${TARGET}"

# The frontend is embedded into the binary, so it has to exist first. Building
# the binary without it produces a working CLI but a web UI that only shows the
# "frontend not built" fallback page.
if [ "$SKIP_WEB" = false ]; then
  echo "==> Building frontend"
  npm --prefix web ci --silent
  npm --prefix web run build --silent
elif [ ! -f web/dist/index.html ]; then
  echo "error: --skip-web given but web/dist/index.html does not exist" >&2
  exit 1
fi

echo "==> Building release binary for ${TARGET}"
cargo build --release --target "$TARGET" -p reclaim-cli

echo "==> Packaging"
rm -rf "dist/${NAME}"
mkdir -p "dist/${NAME}"
cp "target/${TARGET}/release/reclaim" "dist/${NAME}/"
cp README.md LICENSE "dist/${NAME}/"

tar -C dist -czf "dist/${NAME}.tar.gz" "$NAME"
rm -rf "dist/${NAME}"

# shasum is present on macOS; sha256sum on most Linux images.
if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "dist/${NAME}.tar.gz" | awk '{print $1}' > "dist/${NAME}.tar.gz.sha256"
else
  sha256sum "dist/${NAME}.tar.gz" | awk '{print $1}' > "dist/${NAME}.tar.gz.sha256"
fi

echo
echo "    dist/${NAME}.tar.gz"
echo "    sha256: $(cat "dist/${NAME}.tar.gz.sha256")"
echo "    size:   $(du -h "dist/${NAME}.tar.gz" | cut -f1)"

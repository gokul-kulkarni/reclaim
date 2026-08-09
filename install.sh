#!/usr/bin/env sh
# Install reclaim.
#
#   curl -fsSL https://github.com/gokul-kulkarni/reclaim/releases/latest/download/install.sh | sh
#
# Installs to ~/.local/bin by default. Set RECLAIM_INSTALL_DIR to change it.

set -eu

REPO="gokul-kulkarni/reclaim"
INSTALL_DIR="${RECLAIM_INSTALL_DIR:-$HOME/.local/bin}"

fail() { printf 'error: %s\n' "$1" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux)  os="unknown-linux-gnu" ;;
  *) fail "unsupported OS: $(uname -s). reclaim supports macOS and Linux." ;;
esac

case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64)  arch="x86_64" ;;
  *) fail "unsupported architecture: $(uname -m)" ;;
esac

# The only aarch64 Linux build is musl-static.
if [ "$os" = "unknown-linux-gnu" ] && [ "$arch" = "aarch64" ]; then
  os="unknown-linux-musl"
fi

target="${arch}-${os}"
name="reclaim-${target}"
url="https://github.com/${REPO}/releases/latest/download/${name}.tar.gz"

printf 'Installing reclaim (%s) to %s\n' "$target" "$INSTALL_DIR"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$url" -o "$tmp/reclaim.tar.gz" || fail "download failed: $url"
tar -xzf "$tmp/reclaim.tar.gz" -C "$tmp"

mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/$name/reclaim" "$INSTALL_DIR/reclaim"

printf 'Installed %s/reclaim\n' "$INSTALL_DIR"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) printf '\nRun: reclaim\n' ;;
  *) printf '\n%s is not on your PATH. Add it:\n  export PATH="%s:$PATH"\n\nThen run: reclaim\n' "$INSTALL_DIR" "$INSTALL_DIR" ;;
esac

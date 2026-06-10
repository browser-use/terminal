#!/usr/bin/env bash
set -euo pipefail

RG_VERSION="${RG_VERSION:-15.1.0}"
DEST_DIR="${1:-}"
TARGET_TRIPLE="${2:-}"

if [[ -z "$DEST_DIR" ]]; then
  echo "usage: scripts/install-agent-ripgrep.sh DEST_DIR [TARGET_TRIPLE]" >&2
  exit 2
fi

if [[ -x "$DEST_DIR/rg" ]] && "$DEST_DIR/rg" --version 2>/dev/null | sed -n '1p' | grep -q '^ripgrep '; then
  exit 0
fi

if [[ -x "$DEST_DIR/rg.exe" ]] && "$DEST_DIR/rg.exe" --version 2>/dev/null | sed -n '1p' | grep -q '^ripgrep '; then
  exit 0
fi

if [[ -z "$TARGET_TRIPLE" ]]; then
  case "$(uname -s)" in
    Darwin)
      case "$(uname -m)" in
        arm64|aarch64) TARGET_TRIPLE="aarch64-apple-darwin" ;;
        x86_64|amd64) TARGET_TRIPLE="x86_64-apple-darwin" ;;
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
      esac
      ;;
    Linux)
      case "$(uname -m)" in
        arm64|aarch64) TARGET_TRIPLE="aarch64-unknown-linux-musl" ;;
        x86_64|amd64) TARGET_TRIPLE="x86_64-unknown-linux-musl" ;;
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
      esac
      ;;
    MINGW*|MSYS*|CYGWIN*)
      case "$(uname -m)" in
        x86_64|amd64) TARGET_TRIPLE="x86_64-pc-windows-msvc" ;;
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
      esac
      ;;
    *)
      echo "unsupported OS: $(uname -s)" >&2
      exit 1
      ;;
  esac
fi

download_file() {
  local url="$1"
  local output="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$output"
    return
  fi

  if command -v wget >/dev/null 2>&1; then
    wget -q -O "$output" "$url"
    return
  fi

  echo "curl or wget is required to install managed ripgrep." >&2
  exit 1
}

file_sha256() {
  local path="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
    return
  fi

  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
    return
  fi

  echo "sha256sum or shasum is required to verify ripgrep downloads." >&2
  exit 1
}

url=""
digest=""
member=""
archive_kind="tar"

case "$TARGET_TRIPLE" in
  aarch64-apple-darwin)
    url="https://github.com/BurntSushi/ripgrep/releases/download/$RG_VERSION/ripgrep-$RG_VERSION-aarch64-apple-darwin.tar.gz"
    digest="378e973289176ca0c6054054ee7f631a065874a352bf43f0fa60ef079b6ba715"
    member="ripgrep-$RG_VERSION-aarch64-apple-darwin/rg"
    ;;
  x86_64-apple-darwin)
    url="https://github.com/BurntSushi/ripgrep/releases/download/$RG_VERSION/ripgrep-$RG_VERSION-x86_64-apple-darwin.tar.gz"
    digest="64811cb24e77cac3057d6c40b63ac9becf9082eedd54ca411b475b755d334882"
    member="ripgrep-$RG_VERSION-x86_64-apple-darwin/rg"
    ;;
  aarch64-unknown-linux-musl)
    url="https://github.com/BurntSushi/ripgrep/releases/download/$RG_VERSION/ripgrep-$RG_VERSION-aarch64-unknown-linux-gnu.tar.gz"
    digest="2b661c6ef508e902f388e9098d9c4c5aca72c87b55922d94abdba830b4dc885e"
    member="ripgrep-$RG_VERSION-aarch64-unknown-linux-gnu/rg"
    ;;
  x86_64-unknown-linux-musl)
    url="https://github.com/BurntSushi/ripgrep/releases/download/$RG_VERSION/ripgrep-$RG_VERSION-x86_64-unknown-linux-musl.tar.gz"
    digest="1c9297be4a084eea7ecaedf93eb03d058d6faae29bbc57ecdaf5063921491599"
    member="ripgrep-$RG_VERSION-x86_64-unknown-linux-musl/rg"
    ;;
  x86_64-pc-windows-msvc)
    url="https://github.com/BurntSushi/ripgrep/releases/download/$RG_VERSION/ripgrep-$RG_VERSION-x86_64-pc-windows-msvc.zip"
    digest="124510b94b6baa3380d051fdf4650eaa80a302c876d611e9dba0b2e18d87493a"
    member="ripgrep-$RG_VERSION-x86_64-pc-windows-msvc/rg.exe"
    archive_kind="zip"
    ;;
  *)
    echo "unsupported ripgrep target: $TARGET_TRIPLE" >&2
    exit 1
    ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
archive="$tmp/${url##*/}"
download_file "$url" "$archive"

actual_digest="$(file_sha256 "$archive")"
if [[ "$actual_digest" != "$digest" ]]; then
  echo "Downloaded ripgrep archive checksum did not match release metadata." >&2
  echo "target:   $TARGET_TRIPLE" >&2
  echo "expected: $digest" >&2
  echo "actual:   $actual_digest" >&2
  exit 1
fi

if [[ "$archive_kind" == "zip" ]]; then
  if command -v unzip >/dev/null 2>&1; then
    unzip -q "$archive" "$member" -d "$tmp"
  else
    python - "$archive" "$member" "$tmp" <<'PY'
import sys
import zipfile

archive, member, dest = sys.argv[1:]
with zipfile.ZipFile(archive) as zf:
    zf.extract(member, dest)
PY
  fi
else
  tar -xzf "$archive" -C "$tmp" "$member"
fi
mkdir -p "$DEST_DIR"
case "$TARGET_TRIPLE" in
  *windows*) output="$DEST_DIR/rg.exe" ;;
  *) output="$DEST_DIR/rg" ;;
esac
cp "$tmp/$member" "$output"
chmod 0755 "$output"

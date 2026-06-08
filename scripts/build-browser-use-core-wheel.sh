#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_SRC="$ROOT/packages/browser-use-core"
TARGET_TRIPLE="${TARGET_TRIPLE:-}"
OUT_DIR="${OUT_DIR:-"$ROOT/dist"}"
BUILD_DIR="${BUILD_DIR:-}"
TARGET_TRIPLE_PROVIDED=1

cd "$ROOT"

if [[ -z "$TARGET_TRIPLE" ]]; then
  TARGET_TRIPLE_PROVIDED=0
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

case "$TARGET_TRIPLE" in
  aarch64-apple-darwin) PLAT_NAME="macosx_11_0_arm64" ;;
  x86_64-apple-darwin) PLAT_NAME="macosx_10_13_x86_64" ;;
  x86_64-unknown-linux-musl) PLAT_NAME="manylinux_2_17_x86_64" ;;
  aarch64-unknown-linux-musl) PLAT_NAME="manylinux_2_17_aarch64" ;;
  x86_64-pc-windows-msvc) PLAT_NAME="win_amd64" ;;
  *) echo "unsupported target triple for wheel packaging: $TARGET_TRIPLE" >&2; exit 1 ;;
esac

case "$TARGET_TRIPLE" in
  *windows*) EXE_SUFFIX=".exe" ;;
  *) EXE_SUFFIX="" ;;
esac

if [[ -z "$BUILD_DIR" ]]; then
  if [[ "$TARGET_TRIPLE_PROVIDED" -eq 1 || "$TARGET_TRIPLE" == *"-musl" ]]; then
    cargo build --release --target "$TARGET_TRIPLE" -p browser-use-tui -p browser-use-cli
    BUILD_DIR="$ROOT/target/$TARGET_TRIPLE/release"
  else
    cargo build --release -p browser-use-tui -p browser-use-cli
    BUILD_DIR="$ROOT/target/release"
  fi
fi

for binary in but browser-use-terminal; do
  binary="$binary$EXE_SUFFIX"
  if [[ ! -x "$BUILD_DIR/$binary" ]]; then
    echo "missing built binary: $BUILD_DIR/$binary" >&2
    exit 1
  fi
done

STAGE="$(mktemp -d)"
cleanup() {
  rm -rf "$STAGE"
}
trap cleanup EXIT

mkdir -p "$STAGE/src/browser_use_core/bin" "$STAGE/src/browser_use_core/python" "$OUT_DIR"
cp "$PACKAGE_SRC/pyproject.toml" "$STAGE/pyproject.toml"
cp "$PACKAGE_SRC/setup.py" "$STAGE/setup.py"
cp "$PACKAGE_SRC/README.md" "$STAGE/README.md"
cp "$PACKAGE_SRC/src/browser_use_core/"*.py "$STAGE/src/browser_use_core/"
cp "$BUILD_DIR/but$EXE_SUFFIX" "$STAGE/src/browser_use_core/bin/but$EXE_SUFFIX"
cp "$BUILD_DIR/browser-use-terminal$EXE_SUFFIX" "$STAGE/src/browser_use_core/bin/browser-use-terminal$EXE_SUFFIX"
cp -R "$ROOT/python/llm_browser_worker" "$STAGE/src/browser_use_core/python/llm_browser_worker"

find "$STAGE/src/browser_use_core/python" -type d -name __pycache__ -prune -exec rm -rf {} +
find "$STAGE/src/browser_use_core/python" -type f -name '*.pyc' -delete

if [[ "${BROWSER_USE_CORE_SKIP_RIPGREP:-0}" == "1" ]]; then
  mkdir -p "$STAGE/src/browser_use_core/bin/agent-tools"
  if [[ "$EXE_SUFFIX" == ".exe" ]]; then
    cp "$BUILD_DIR/browser-use-terminal$EXE_SUFFIX" "$STAGE/src/browser_use_core/bin/agent-tools/rg.exe"
  else
    printf '#!/bin/sh\n' >"$STAGE/src/browser_use_core/bin/agent-tools/rg"
  fi
else
  "$ROOT/scripts/install-agent-ripgrep.sh" "$STAGE/src/browser_use_core/bin/agent-tools" "$TARGET_TRIPLE"
fi

chmod 0755 "$STAGE/src/browser_use_core/bin/but$EXE_SUFFIX" "$STAGE/src/browser_use_core/bin/browser-use-terminal$EXE_SUFFIX"
if [[ -d "$STAGE/src/browser_use_core/bin/agent-tools" ]]; then
  find "$STAGE/src/browser_use_core/bin/agent-tools" -type f -exec chmod 0755 {} +
fi

if command -v uv >/dev/null 2>&1; then
  BROWSER_USE_CORE_PLAT_NAME="$PLAT_NAME" uv build --wheel --out-dir "$OUT_DIR" "$STAGE"
else
  BROWSER_USE_CORE_PLAT_NAME="$PLAT_NAME" python -m build --wheel --outdir "$OUT_DIR" "$STAGE"
fi

#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_TRIPLE="${TARGET_TRIPLE:-}"
OUT_DIR="${OUT_DIR:-"$ROOT/dist"}"
PACKAGE_NAME="browser-use-terminal"
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
    *)
      echo "unsupported OS: $(uname -s)" >&2
      exit 1
      ;;
  esac
fi

if [[ "$TARGET_TRIPLE_PROVIDED" -eq 1 || "$TARGET_TRIPLE" == *"-musl" ]]; then
  cargo build --release --target "$TARGET_TRIPLE" -p browser-use-tui -p browser-use-cli
  BUILD_DIR="$ROOT/target/$TARGET_TRIPLE/release"
else
  cargo build --release -p browser-use-tui -p browser-use-cli
  BUILD_DIR="$ROOT/target/release"
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/$PACKAGE_NAME/bin" "$STAGE/$PACKAGE_NAME/python"
cp "$BUILD_DIR/but" "$STAGE/$PACKAGE_NAME/bin/but"
cp "$BUILD_DIR/browser-use-terminal" "$STAGE/$PACKAGE_NAME/bin/browser-use-terminal"
ln -sf but "$STAGE/$PACKAGE_NAME/bin/browser"
ln -sf but "$STAGE/$PACKAGE_NAME/bin/browser-use"
"$ROOT/scripts/install-agent-ripgrep.sh" "$STAGE/$PACKAGE_NAME/bin/agent-tools" "$TARGET_TRIPLE"
cp -R "$ROOT/python/browser_harness" "$STAGE/$PACKAGE_NAME/python/browser_harness"
cp -R "$ROOT/python/browser_harness_skill" "$STAGE/$PACKAGE_NAME/python/browser_harness_skill"
cp -R "$ROOT/python/llm_browser_worker" "$STAGE/$PACKAGE_NAME/python/llm_browser_worker"
find "$STAGE/$PACKAGE_NAME/python" -type d -name __pycache__ -prune -exec rm -rf {} +
find "$STAGE/$PACKAGE_NAME/python" -type f -name '*.pyc' -delete
chmod 0755 "$STAGE/$PACKAGE_NAME/bin/but" "$STAGE/$PACKAGE_NAME/bin/browser-use-terminal"

mkdir -p "$OUT_DIR"
ARCHIVE="$OUT_DIR/$PACKAGE_NAME-$TARGET_TRIPLE.tar.gz"
tar -C "$STAGE" -czf "$ARCHIVE" "$PACKAGE_NAME"

if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "$ARCHIVE" >"$ARCHIVE.sha256"
elif command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$ARCHIVE" >"$ARCHIVE.sha256"
fi

echo "$ARCHIVE"

#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
INSTALL_SH="$ROOT/scripts/install/install.sh"

assert_eq() {
  local expected="$1"
  local actual="$2"
  local label="$3"

  if [[ "$actual" != "$expected" ]]; then
    printf 'FAIL: %s\nexpected: %s\nactual:   %s\n' "$label" "$expected" "$actual" >&2
    exit 1
  fi
}

assert_contains() {
  local needle="$1"
  local haystack="$2"
  local label="$3"

  if [[ "$haystack" != *"$needle"* ]]; then
    printf 'FAIL: %s\nmissing: %s\noutput:\n%s\n' "$label" "$needle" "$haystack" >&2
    exit 1
  fi
}

assert_not_contains() {
  local needle="$1"
  local haystack="$2"
  local label="$3"

  if [[ "$haystack" == *"$needle"* ]]; then
    printf 'FAIL: %s\nunexpected: %s\noutput:\n%s\n' "$label" "$needle" "$haystack" >&2
    exit 1
  fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

functions_file="$tmp/install-functions.sh"
awk '/^parse_args "\$@"/ { exit } { print }' "$INSTALL_SH" >"$functions_file"
# shellcheck disable=SC1090
. "$functions_file"

BIN_DIR="$tmp/bin"
BUT_HOME_DIR="$tmp/home"
STANDALONE_ROOT="$BUT_HOME_DIR/packages/standalone"
CURRENT_LINK="$STANDALONE_ROOT/current"
REPO="browser-use/terminal"
HOME="$tmp/user-home"
SHELL="/bin/bash"
os="linux"
tmp_dir="$tmp"
BUT_HOME="$BUT_HOME_DIR"
export HOME SHELL BUT_HOME

mkdir -p "$CURRENT_LINK/bin" "$BIN_DIR" "$STANDALONE_ROOT" "$HOME"

cat >"$CURRENT_LINK/bin/but" <<'EOF'
#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "but ${BUT_TEST_VERSION:-0.1.0}"
  exit 0
fi
echo "but $*"
EOF

cat >"$CURRENT_LINK/bin/browser-use-terminal" <<EOF
#!/bin/sh
echo "cli \$*" >>"$tmp/update.log"
if [ "\$1" = "update" ]; then
  if [ "\${BUT_TEST_UPDATE_FAIL:-0}" = "1" ]; then
    echo "forced update failure" >&2
    exit 42
  fi
  exit 0
fi
echo "cli \$*"
EOF

chmod +x "$CURRENT_LINK/bin/but" "$CURRENT_LINK/bin/browser-use-terminal"

release_fixture="$STANDALONE_ROOT/releases/0.1.0-test-target"
mkdir -p "$release_fixture/bin" "$release_fixture/python/llm_browser_worker"
touch "$release_fixture/bin/but" "$release_fixture/bin/browser-use-terminal" "$release_fixture/python/llm_browser_worker/worker.py"
chmod +x "$release_fixture/bin/but" "$release_fixture/bin/browser-use-terminal"
if release_dir_is_complete "$release_fixture" "0.1.0" "test-target"; then
  printf 'FAIL: release without managed rg should be incomplete\n' >&2
  exit 1
fi
mkdir -p "$release_fixture/bin/agent-tools"
touch "$release_fixture/bin/agent-tools/rg"
chmod +x "$release_fixture/bin/agent-tools/rg"
if ! release_dir_is_complete "$release_fixture" "0.1.0" "test-target"; then
  printf 'FAIL: release with managed rg should be complete\n' >&2
  exit 1
fi

update_visible_commands

assert_eq "$HOME/.zshrc" "$(SHELL=/bin/zsh os=darwin pick_profile)" "macOS zsh writes to interactive ~/.zshrc"
assert_eq "$HOME/.bashrc" "$(SHELL=/bin/bash os=darwin pick_profile)" "macOS bash writes to interactive ~/.bashrc"
assert_eq "$HOME/.zshrc" "$(SHELL=/usr/bin/zsh os=linux pick_profile)" "Linux zsh writes to interactive ~/.zshrc"
assert_eq "$HOME/.bashrc" "$(SHELL=/bin/bash os=linux pick_profile)" "Linux bash writes to interactive ~/.bashrc"

PATH_WITHOUT_BIN="$PATH"
case ":$PATH_WITHOUT_BIN:" in
  *":$BIN_DIR:"*)
    PATH_WITHOUT_BIN="$(printf '%s\n' "$PATH_WITHOUT_BIN" | sed "s#:$BIN_DIR:##; s#^$BIN_DIR:##; s#:$BIN_DIR\$##")"
    ;;
esac

profile="$HOME/.bashrc"
printf "%s\n" "alias browser='old-browser'" >"$profile"
PATH="$PATH_WITHOUT_BIN" add_to_path
profile_contents="$(cat "$profile")"
assert_contains "export PATH=\"$BIN_DIR:\$PATH\"" "$profile_contents" "installer profile block adds PATH"
assert_contains "unalias browser 2>/dev/null || true" "$profile_contents" "installer profile block clears existing browser alias"
assert_contains "alias browser=\"$BIN_DIR/browser\"" "$profile_contents" "installer profile block sets browser alias"
alias_output="$(bash --rcfile "$profile" -ic 'alias browser' 2>/dev/null)"
assert_contains "$BIN_DIR/browser" "$alias_output" "browser alias resolves to installed wrapper"

cat >"$profile" <<EOF
# >>> browser-use terminal installer >>>
export PATH="$BIN_DIR:\$PATH"
# <<< browser-use terminal installer <<<
EOF
PATH="$BIN_DIR:$PATH_WITHOUT_BIN" add_to_path
profile_contents="$(cat "$profile")"
assert_contains "export PATH=\"$BIN_DIR:\$PATH\"" "$profile_contents" "installer preserves existing managed PATH line"
assert_contains "alias browser=\"$BIN_DIR/browser\"" "$profile_contents" "installer updates existing managed block with browser alias"

: >"$tmp/update.log"
verify_visible_command
assert_not_contains "update --release latest" "$(cat "$tmp/update.log")" "installer verification skips auto-update"

: >"$tmp/update.log"
out="$(BUT_AUTO_UPDATE_INTERVAL_SECS=0 "$BIN_DIR/browser")"
assert_eq "but " "$out" "browser launches TUI after auto-update"
assert_contains "cli update --release latest" "$(cat "$tmp/update.log")" "browser auto-runs update"

: >"$tmp/update.log"
out="$(BUT_AUTO_UPDATE_INTERVAL_SECS=0 "$BIN_DIR/browser-use")"
assert_eq "but " "$out" "browser-use launches TUI after auto-update"
assert_contains "cli update --release latest" "$(cat "$tmp/update.log")" "browser-use auto-runs update"

: >"$tmp/update.log"
out="$(BUT_AUTO_UPDATE_INTERVAL_SECS=0 "$BIN_DIR/browser-use-terminal")"
assert_eq "but " "$out" "browser-use-terminal launches TUI with no args"
assert_contains "cli update --release latest" "$(cat "$tmp/update.log")" "browser-use-terminal auto-runs update"

: >"$tmp/update.log"
out="$(BUT_AUTO_UPDATE_INTERVAL_SECS=0 "$BIN_DIR/browser" update --check)"
assert_eq "" "$out" "browser routes args to management CLI"
assert_eq "cli update --check" "$(cat "$tmp/update.log")" "browser update --check does not preflight auto-update"

: >"$tmp/update.log"
out="$(BUT_AUTO_UPDATE=0 "$BIN_DIR/browser")"
assert_eq "but " "$out" "BUT_AUTO_UPDATE=0 still launches TUI"
assert_eq "" "$(cat "$tmp/update.log")" "BUT_AUTO_UPDATE=0 skips update"

: >"$tmp/update.log"
rm -f "$STANDALONE_ROOT/last_update_check"
"$BIN_DIR/browser" >/dev/null
"$BIN_DIR/browser" >/dev/null
assert_eq "cli update --release latest" "$(cat "$tmp/update.log")" "default throttle checks once"

: >"$tmp/update.log"
rm -f "$STANDALONE_ROOT/last_update_check"
if BUT_TEST_UPDATE_FAIL=1 BUT_REQUIRE_LATEST=1 "$BIN_DIR/browser" >/dev/null 2>"$tmp/fail.err"; then
  printf 'FAIL: BUT_REQUIRE_LATEST=1 should fail when update fails\n' >&2
  exit 1
fi
assert_contains "forced update failure" "$(cat "$STANDALONE_ROOT/last_update.log" "$tmp/fail.err" 2>/dev/null)" "fail-closed reports updater failure"

printf 'install wrapper smoke passed\n'

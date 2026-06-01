#!/bin/sh

set -eu

RELEASE="latest"
REPO="${BUT_RELEASE_REPO:-browser-use/terminal}"
BIN_DIR="${BUT_INSTALL_DIR:-$HOME/.local/bin}"
BUT_HOME_DIR="${BUT_HOME:-$HOME/.browser-use-terminal}"
STANDALONE_ROOT="$BUT_HOME_DIR/packages/standalone"
RELEASES_DIR="$STANDALONE_ROOT/releases"
CURRENT_LINK="$STANDALONE_ROOT/current"
LOCK_FILE="$STANDALONE_ROOT/install.lock"
LOCK_DIR="$STANDALONE_ROOT/install.lock.d"
LOCK_STALE_AFTER_SECS=600

path_action="already"
path_profile=""
lock_kind=""
tmp_dir=""

step() {
  printf '==> %s\n' "$1"
}

warn() {
  printf 'WARNING: %s\n' "$1" >&2
}

normalize_version() {
  case "$1" in
    "" | latest)
      printf 'latest\n'
      ;;
    browser-use-terminal-v*)
      printf '%s\n' "${1#browser-use-terminal-v}"
      ;;
    v*)
      printf '%s\n' "${1#v}"
      ;;
    *)
      printf '%s\n' "$1"
      ;;
  esac
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --release)
        if [ "$#" -lt 2 ]; then
          echo "--release requires a value." >&2
          exit 1
        fi
        RELEASE="$2"
        shift
        ;;
      --no-launch)
        :
        ;;
      --help | -h)
        cat <<EOF
Usage: install.sh [--release VERSION] [--no-launch]

Environment:
  BUT_RELEASE_REPO   GitHub repo containing releases. Default: browser-use/terminal
  BUT_INSTALL_DIR    Directory for visible commands. Default: \$HOME/.local/bin
  BUT_HOME           State/package root. Default: \$HOME/.browser-use-terminal
  BUT_AUTO_UPDATE    Set to 0 to skip automatic update checks in launchers.
  BUT_REQUIRE_LATEST Set to 1 to fail startup if the automatic update check fails.
EOF
        exit 0
        ;;
      *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
    esac
    shift
  done
}

download_file() {
  url="$1"
  output="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$output"
    return
  fi

  if command -v wget >/dev/null 2>&1; then
    wget -q -O "$output" "$url"
    return
  fi

  echo "curl or wget is required to install browser-use terminal." >&2
  exit 1
}

download_text() {
  url="$1"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url"
    return
  fi

  if command -v wget >/dev/null 2>&1; then
    wget -q -O - "$url"
    return
  fi

  echo "curl or wget is required to install browser-use terminal." >&2
  exit 1
}

release_tag() {
  resolved_version="$1"
  printf 'v%s\n' "$resolved_version"
}

release_url_for_asset() {
  asset="$1"
  resolved_version="$2"

  if [ "$resolved_version" = "latest" ]; then
    printf 'https://github.com/%s/releases/latest/download/%s\n' "$REPO" "$asset"
  else
    printf 'https://github.com/%s/releases/download/%s/%s\n' "$REPO" "$(release_tag "$resolved_version")" "$asset"
  fi
}

release_asset_digest() {
  asset="$1"
  resolved_version="$2"
  checksum="$(download_text "$(release_url_for_asset "$asset.sha256" "$resolved_version")")"
  digest="$(printf '%s\n' "$checksum" | awk 'NR == 1 { print $1 }')"

  case "$digest" in
    ????????????????????????????????????????????????????????????????)
      printf '%s\n' "$digest"
      ;;
    *)
      echo "Could not read SHA-256 digest for release asset $asset." >&2
      exit 1
      ;;
  esac
}

file_sha256() {
  path="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
    return
  fi

  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
    return
  fi

  if command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$path" | sed 's/^.*= //'
    return
  fi

  echo "sha256sum, shasum, or openssl is required to verify the download." >&2
  exit 1
}

verify_archive_digest() {
  archive_path="$1"
  expected_digest="$2"
  actual_digest="$(file_sha256 "$archive_path")"

  if [ "$actual_digest" != "$expected_digest" ]; then
    echo "Downloaded browser-use terminal archive checksum did not match release metadata." >&2
    echo "expected: $expected_digest" >&2
    echo "actual:   $actual_digest" >&2
    exit 1
  fi
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "$1 is required to install browser-use terminal." >&2
    exit 1
  fi
}

resolve_version() {
  normalized_version="$(normalize_version "$RELEASE")"

  if [ "$normalized_version" != "latest" ]; then
    printf '%s\n' "$normalized_version"
    return
  fi

  if command -v curl >/dev/null 2>&1; then
    final_url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")"
    resolved="${final_url##*/releases/tag/}"
    resolved="${resolved%%[?#]*}"
  else
    release_json="$(download_text "https://api.github.com/repos/$REPO/releases/latest")"
    resolved="$(printf '%s\n' "$release_json" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
  fi
  resolved="$(normalize_version "$resolved")"

  if [ -z "$resolved" ] || [ "$resolved" = "latest" ]; then
    echo "Failed to resolve the latest browser-use terminal release version." >&2
    exit 1
  fi

  printf '%s\n' "$resolved"
}

pick_profile() {
  case "${SHELL:-}" in
    */zsh)
      printf '%s\n' "$HOME/.zshrc"
      ;;
    */bash)
      printf '%s\n' "$HOME/.bashrc"
      ;;
    *)
      printf '%s\n' "$HOME/.profile"
      ;;
  esac
}

add_to_path() {
  path_action="already"
  path_profile=""

  profile="$(pick_profile)"
  path_profile="$profile"
  begin_marker="# >>> browser-use terminal installer >>>"
  end_marker="# <<< browser-use terminal installer <<<"
  alias_reset_line="unalias browser 2>/dev/null || true"
  alias_line="alias browser=\"$BIN_DIR/browser\""
  desired_path_line="export PATH=\"$BIN_DIR:\$PATH\""
  path_line=""

  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
      path_line="$desired_path_line"
      ;;
  esac

  if [ -f "$profile" ] &&
    grep -F "$begin_marker" "$profile" >/dev/null 2>&1 &&
    grep -F "$desired_path_line" "$profile" >/dev/null 2>&1; then
    path_line="$desired_path_line"
  fi

  if [ -f "$profile" ] && grep -F "$begin_marker" "$profile" >/dev/null 2>&1; then
    if { [ -z "$path_line" ] || grep -F "$path_line" "$profile" >/dev/null 2>&1; } &&
      grep -F "$alias_reset_line" "$profile" >/dev/null 2>&1 &&
      grep -F "$alias_line" "$profile" >/dev/null 2>&1; then
      path_action="configured"
      return
    fi

    if grep -F "$end_marker" "$profile" >/dev/null 2>&1; then
      rewrite_path_block "$profile" "$begin_marker" "$end_marker" "$path_line" "$alias_reset_line" "$alias_line"
      path_action="updated"
      return
    fi
  fi

  append_path_block "$profile" "$begin_marker" "$end_marker" "$path_line" "$alias_reset_line" "$alias_line"
  path_action="added"
}

append_path_block() {
  profile="$1"
  begin_marker="$2"
  end_marker="$3"
  path_line="$4"
  alias_reset_line="$5"
  alias_line="$6"

  {
    printf '\n%s\n' "$begin_marker"
    if [ -n "$path_line" ]; then
      printf '%s\n' "$path_line"
    fi
    printf '%s\n' "$alias_reset_line"
    printf '%s\n' "$alias_line"
    printf '%s\n' "$end_marker"
  } >>"$profile"
}

rewrite_path_block() {
  profile="$1"
  begin_marker="$2"
  end_marker="$3"
  path_line="$4"
  alias_reset_line="$5"
  alias_line="$6"
  tmp_profile="$tmp_dir/profile.$$.tmp"

  awk -v begin="$begin_marker" -v end="$end_marker" -v path_line="$path_line" -v alias_reset_line="$alias_reset_line" -v alias_line="$alias_line" '
    BEGIN {
      in_block = 0
      replaced = 0
    }
    $0 == begin {
      if (!replaced) {
        print begin
        if (path_line != "") {
          print path_line
        }
        print alias_reset_line
        print alias_line
        print end
        replaced = 1
      }
      in_block = 1
      next
    }
    in_block {
      if ($0 == end) {
        in_block = 0
      }
      next
    }
    {
      print
    }
    END {
      if (in_block != 0) {
        exit 1
      }
    }
  ' "$profile" >"$tmp_profile"
  mv "$tmp_profile" "$profile"
}

mkdir_lock_is_stale() {
  [ -d "$LOCK_DIR" ] || return 1

  pid="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"
  started_at="$(cat "$LOCK_DIR/started_at" 2>/dev/null || true)"
  now="$(date +%s 2>/dev/null || printf '0')"

  case "$started_at" in
    ''|*[!0-9]*)
      started_at=0
      ;;
  esac

  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    return 1
  fi

  if [ "$started_at" -eq 0 ] || [ "$now" -eq 0 ]; then
    return 0
  fi

  [ $((now - started_at)) -ge "$LOCK_STALE_AFTER_SECS" ]
}

acquire_install_lock() {
  mkdir -p "$STANDALONE_ROOT"

  if [ "$os" = "darwin" ] && command -v lockf >/dev/null 2>&1; then
    : >>"$LOCK_FILE"
    exec 9<>"$LOCK_FILE"
    lockf 9
    lock_kind="lockf"
    return
  fi

  if command -v flock >/dev/null 2>&1; then
    exec 9>"$LOCK_FILE"
    flock 9
    lock_kind="flock"
    return
  fi

  while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    if mkdir_lock_is_stale; then
      warn "Removing stale installer lock at $LOCK_DIR"
      rm -rf "$LOCK_DIR"
      continue
    fi
    sleep 1
  done

  printf '%s\n' "$$" >"$LOCK_DIR/pid"
  date +%s >"$LOCK_DIR/started_at" 2>/dev/null || true
  lock_kind="mkdir"
}

release_install_lock() {
  if [ "$lock_kind" = "mkdir" ]; then
    rm -rf "$LOCK_DIR" 2>/dev/null || true
  elif [ "$lock_kind" = "flock" ] || [ "$lock_kind" = "lockf" ]; then
    exec 9>&- 2>/dev/null || true
  fi
  lock_kind=""
}

cleanup_stale_install_artifacts() {
  mkdir -p "$RELEASES_DIR" "$STANDALONE_ROOT"

  find "$RELEASES_DIR" -mindepth 1 -maxdepth 1 -name '.staging.*' -exec rm -rf {} +
  find "$STANDALONE_ROOT" -mindepth 1 -maxdepth 1 -name '.current.*' -exec rm -f {} +

  if [ -d "$BIN_DIR" ]; then
    find "$BIN_DIR" -mindepth 1 -maxdepth 1 \( -name '.but.*' -o -name '.browser.*' -o -name '.browser-use.*' -o -name '.browser-use-terminal.*' \) -exec rm -f {} +
  fi
}

replace_path_with_symlink() {
  link_path="$1"
  link_target="$2"
  tmp_link="$3"

  rm -f "$tmp_link"
  ln -s "$link_target" "$tmp_link"

  if mv -Tf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi

  if mv -hf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi

  rm -f "$link_path"
  mv -f "$tmp_link" "$link_path"
}

version_from_binary() {
  binary_path="$1"

  if [ ! -x "$binary_path" ]; then
    return 1
  fi

  "$binary_path" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1
}

current_installed_version() {
  version="$(version_from_binary "$CURRENT_LINK/bin/but" || true)"
  if [ -n "$version" ]; then
    printf '%s\n' "$version"
    return 0
  fi

  return 0
}

print_launch_instructions() {
  case "$path_action" in
    added)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && browser-use"
      step "Future terminals: run browser, browser-use, browser-use-terminal, or but"
      step "PATH was added to $path_profile"
      ;;
    updated)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && browser-use"
      step "Future terminals: run browser, browser-use, browser-use-terminal, or but"
      step "PATH was updated in $path_profile"
      ;;
    configured)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && browser-use"
      step "Future terminals: run browser, browser-use, browser-use-terminal, or but"
      step "PATH is already configured in $path_profile"
      ;;
    *)
      step "Current terminal: browser-use"
      step "Future terminals: run browser, browser-use, browser-use-terminal, or but"
      ;;
  esac
}

payload_root() {
  extract_dir="$1"

  if [ -x "$extract_dir/browser-use-terminal/bin/but" ]; then
    printf '%s\n' "$extract_dir/browser-use-terminal"
    return 0
  fi

  if [ -x "$extract_dir/bin/but" ]; then
    printf '%s\n' "$extract_dir"
    return 0
  fi

  echo "Archive does not contain expected bin/but payload." >&2
  exit 1
}

install_release() {
  release_dir="$1"
  payload="$2"
  stage_release="$RELEASES_DIR/.staging.$(basename "$release_dir").$$"

  mkdir -p "$RELEASES_DIR"
  rm -rf "$stage_release"
  mkdir -p "$stage_release"
  cp -R "$payload/bin" "$stage_release/bin"
  if [ -d "$payload/python" ]; then
    cp -R "$payload/python" "$stage_release/python"
  else
    mkdir -p "$stage_release/python"
  fi
  chmod 0755 "$stage_release/bin/but" "$stage_release/bin/browser-use-terminal"
  if [ -d "$stage_release/bin/agent-tools" ]; then
    find "$stage_release/bin/agent-tools" -type f -exec chmod 0755 {} +
  fi

  if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
    rm -rf "$release_dir"
  fi
  mv "$stage_release" "$release_dir"
}

release_dir_is_complete() {
  release_dir="$1"
  expected_version="$2"
  expected_target="$3"

  [ -d "$release_dir" ] &&
    [ -x "$release_dir/bin/but" ] &&
    [ -x "$release_dir/bin/browser-use-terminal" ] &&
    [ -x "$release_dir/bin/agent-tools/rg" ] &&
    [ -f "$release_dir/python/llm_browser_worker/worker.py" ] &&
    [ "$(basename "$release_dir")" = "$expected_version-$expected_target" ]
}

update_current_link() {
  release_dir="$1"
  tmp_link="$STANDALONE_ROOT/.current.$$"

  replace_path_with_symlink "$CURRENT_LINK" "$release_dir" "$tmp_link"
}

write_launcher_wrapper() {
  name="$1"
  target="$2"
  wrapper_path="$BIN_DIR/$name"
  tmp_wrapper="$BIN_DIR/.$name.$$"

cat >"$tmp_wrapper" <<EOF
#!/bin/sh
BUT_HOME_DIR="\${BUT_HOME:-$BUT_HOME_DIR}"
CURRENT="\$BUT_HOME_DIR/packages/standalone/current"
export BUT_HOME="\$BUT_HOME_DIR"
export BUT_INSTALL_DIR="\${BUT_INSTALL_DIR:-$BIN_DIR}"
export BUT_RELEASE_REPO="\${BUT_RELEASE_REPO:-$REPO}"
export PYTHONPATH="\$CURRENT/python\${PYTHONPATH:+:\$PYTHONPATH}"
auto_update_browser_use_terminal() {
  case "\${BUT_AUTO_UPDATE:-1}" in
    0 | false | FALSE | off | OFF | no | NO)
      return
      ;;
  esac

  [ -x "\$CURRENT/bin/browser-use-terminal" ] || return

  stamp_dir="\$BUT_HOME_DIR/packages/standalone"
  stamp="\$stamp_dir/last_update_check"
  log="\$stamp_dir/last_update.log"
  interval="\${BUT_AUTO_UPDATE_INTERVAL_SECS:-72000}"
  case "\$interval" in
    "" | *[!0-9]*)
      interval=72000
      ;;
  esac

  now="\$(date +%s 2>/dev/null || printf '0')"
  [ "\$now" -gt 0 ] || return
  last=0
  if [ -f "\$stamp" ]; then
    last="\$(cat "\$stamp" 2>/dev/null || printf '0')"
    case "\$last" in
      "" | *[!0-9]*)
        last=0
        ;;
    esac
  fi
  if [ "\$interval" -gt 0 ] && [ \$((now - last)) -lt "\$interval" ]; then
    return
  fi

  mkdir -p "\$stamp_dir" 2>/dev/null || return
  printf '%s\n' "\$now" >"\$stamp" 2>/dev/null || true

  before="\$("\$CURRENT/bin/but" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1 || true)"
  if "\$CURRENT/bin/browser-use-terminal" update --release latest >"\$log" 2>&1; then
    after="\$("\$CURRENT/bin/but" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1 || true)"
    if [ -n "\$before" ] && [ -n "\$after" ] && [ "\$before" != "\$after" ]; then
      printf 'browser-use terminal updated: %s -> %s\n' "\$before" "\$after" >&2
    fi
    return
  fi

  if [ "\${BUT_REQUIRE_LATEST:-0}" = "1" ]; then
    cat "\$log" >&2 2>/dev/null || true
    exit 1
  fi
  printf 'browser-use terminal update check failed; launching current version.\n' >&2
}
auto_update_browser_use_terminal
exec "\$CURRENT/bin/$target" "\$@"
EOF
  chmod 0755 "$tmp_wrapper"
  mv -f "$tmp_wrapper" "$wrapper_path"
}

write_hybrid_wrapper() {
  name="$1"
  wrapper_path="$BIN_DIR/$name"
  tmp_wrapper="$BIN_DIR/.$name.$$"

cat >"$tmp_wrapper" <<EOF
#!/bin/sh
BUT_HOME_DIR="\${BUT_HOME:-$BUT_HOME_DIR}"
CURRENT="\$BUT_HOME_DIR/packages/standalone/current"
export BUT_HOME="\$BUT_HOME_DIR"
export BUT_INSTALL_DIR="\${BUT_INSTALL_DIR:-$BIN_DIR}"
export BUT_RELEASE_REPO="\${BUT_RELEASE_REPO:-$REPO}"
export PYTHONPATH="\$CURRENT/python\${PYTHONPATH:+:\$PYTHONPATH}"
if [ "\$#" -eq 0 ]; then
  auto_update_browser_use_terminal() {
    case "\${BUT_AUTO_UPDATE:-1}" in
      0 | false | FALSE | off | OFF | no | NO)
        return
        ;;
    esac

    [ -x "\$CURRENT/bin/browser-use-terminal" ] || return

    stamp_dir="\$BUT_HOME_DIR/packages/standalone"
    stamp="\$stamp_dir/last_update_check"
    log="\$stamp_dir/last_update.log"
    interval="\${BUT_AUTO_UPDATE_INTERVAL_SECS:-72000}"
    case "\$interval" in
      "" | *[!0-9]*)
        interval=72000
        ;;
    esac

    now="\$(date +%s 2>/dev/null || printf '0')"
    [ "\$now" -gt 0 ] || return
    last=0
    if [ -f "\$stamp" ]; then
      last="\$(cat "\$stamp" 2>/dev/null || printf '0')"
      case "\$last" in
        "" | *[!0-9]*)
          last=0
          ;;
      esac
    fi
    if [ "\$interval" -gt 0 ] && [ \$((now - last)) -lt "\$interval" ]; then
      return
    fi

    mkdir -p "\$stamp_dir" 2>/dev/null || return
    printf '%s\n' "\$now" >"\$stamp" 2>/dev/null || true

    before="\$("\$CURRENT/bin/but" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1 || true)"
    if "\$CURRENT/bin/browser-use-terminal" update --release latest >"\$log" 2>&1; then
      after="\$("\$CURRENT/bin/but" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1 || true)"
      if [ -n "\$before" ] && [ -n "\$after" ] && [ "\$before" != "\$after" ]; then
        printf 'browser-use terminal updated: %s -> %s\n' "\$before" "\$after" >&2
      fi
      return
    fi

    if [ "\${BUT_REQUIRE_LATEST:-0}" = "1" ]; then
      cat "\$log" >&2 2>/dev/null || true
      exit 1
    fi
    printf 'browser-use terminal update check failed; launching current version.\n' >&2
  }
  auto_update_browser_use_terminal
  exec "\$CURRENT/bin/but"
fi
exec "\$CURRENT/bin/browser-use-terminal" "\$@"
EOF
  chmod 0755 "$tmp_wrapper"
  mv -f "$tmp_wrapper" "$wrapper_path"
}

update_visible_commands() {
  mkdir -p "$BIN_DIR"
  write_launcher_wrapper but but
  write_hybrid_wrapper browser
  write_hybrid_wrapper browser-use
  write_hybrid_wrapper browser-use-terminal
}

verify_visible_command() (
  BUT_AUTO_UPDATE=0
  export BUT_AUTO_UPDATE
  "$BIN_DIR/but" --version >/dev/null
  "$BIN_DIR/browser" --version >/dev/null
  "$BIN_DIR/browser-use" --version >/dev/null
  "$BIN_DIR/browser-use-terminal" --version >/dev/null
)

parse_args "$@"

require_command mktemp
require_command tar

case "$(uname -s)" in
  Darwin)
    os="darwin"
    ;;
  Linux)
    os="linux"
    ;;
  *)
    echo "install.sh supports macOS and Linux." >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64)
    arch="x86_64"
    ;;
  arm64 | aarch64)
    arch="aarch64"
    ;;
  *)
    echo "Unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

if [ "$os" = "darwin" ] && [ "$arch" = "x86_64" ]; then
  if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
    arch="aarch64"
  fi
fi

if [ "$os" = "darwin" ]; then
  if [ "$arch" = "aarch64" ]; then
    target="aarch64-apple-darwin"
    platform_label="macOS (Apple Silicon)"
  else
    target="x86_64-apple-darwin"
    platform_label="macOS (Intel)"
  fi
else
  if [ "$arch" = "aarch64" ]; then
    target="aarch64-unknown-linux-musl"
    platform_label="Linux (ARM64)"
  else
    target="x86_64-unknown-linux-musl"
    platform_label="Linux (x64)"
  fi
fi

resolved_version="$(resolve_version)"
asset="browser-use-terminal-$target.tar.gz"
download_url="$(release_url_for_asset "$asset" "$resolved_version")"
release_name="$resolved_version-$target"
release_dir="$RELEASES_DIR/$release_name"
current_version="$(current_installed_version)"

if [ -n "$current_version" ] && [ "$current_version" != "$resolved_version" ]; then
  step "Updating browser-use terminal from $current_version to $resolved_version"
elif [ -n "$current_version" ]; then
  step "Refreshing browser-use terminal $current_version"
else
  step "Installing browser-use terminal"
fi
step "Detected platform: $platform_label"
step "Resolved version: $resolved_version"

tmp_dir="$(mktemp -d)"
cleanup() {
  release_install_lock
  if [ -n "$tmp_dir" ]; then
    rm -rf "$tmp_dir"
  fi
}
trap cleanup EXIT INT TERM

acquire_install_lock
cleanup_stale_install_artifacts

if ! release_dir_is_complete "$release_dir" "$resolved_version" "$target"; then
  if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
    warn "Found incomplete existing release at $release_dir; reinstalling."
  fi

  archive_path="$tmp_dir/$asset"
  extract_dir="$tmp_dir/extract"

  step "Downloading browser-use terminal"
  expected_digest="$(release_asset_digest "$asset" "$resolved_version")"
  download_file "$download_url" "$archive_path"
  verify_archive_digest "$archive_path" "$expected_digest"

  mkdir -p "$extract_dir"
  tar -xzf "$archive_path" -C "$extract_dir"

  payload="$(payload_root "$extract_dir")"
  step "Installing standalone package to $release_dir"
  install_release "$release_dir" "$payload"
fi

update_current_link "$release_dir"
update_visible_commands
add_to_path
verify_visible_command
release_install_lock

case "$path_action" in
  added | updated | configured)
    print_launch_instructions
    ;;
  *)
    step "$BIN_DIR is already on PATH"
    print_launch_instructions
    ;;
esac

printf 'browser-use terminal %s installed successfully.\n' "$resolved_version"

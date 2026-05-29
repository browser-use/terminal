use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const BUT_AGENT_TOOLS_DIR_ENV: &str = "BUT_AGENT_TOOLS_DIR";

const RG_BOOTSTRAP_SCRIPT: &str = r#"#!/bin/sh
set -fu

self="$0"

is_working_rg() {
  candidate="$1"
  first_line="$("$candidate" --version 2>/dev/null)" || return 1
  case "$first_line" in
    ripgrep\ *) return 0 ;;
    *) return 1 ;;
  esac
}

is_bootstrap_rg() {
  candidate="$1"
  if [ "$candidate" = "$self" ]; then
    return 0
  fi
  case "$candidate" in
    */browser-use-terminal-agent-tools-*/rg) return 0 ;;
  esac
  line_count=0
  while IFS= read -r line && [ "$line_count" -lt 80 ]; do
    case "$line" in
      *'browser-use terminal could not find a working ripgrep executable'*) return 0 ;;
    esac
    line_count=$((line_count + 1))
  done < "$candidate" 2>/dev/null || true
  return 1
}

find_working_rg() {
  old_ifs="$IFS"
  IFS=:
  for dir in ${PATH:-}; do
    if [ -z "$dir" ]; then
      dir=.
    fi
    candidate="$dir/rg"
    if is_bootstrap_rg "$candidate"; then
      continue
    fi
    if [ -x "$candidate" ] && is_working_rg "$candidate"; then
      IFS="$old_ifs"
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  IFS="$old_ifs"

  if [ -n "${HOME:-}" ]; then
    candidate="$HOME/.cargo/bin/rg"
    if [ -x "$candidate" ] && ! is_bootstrap_rg "$candidate" && is_working_rg "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
  fi

  for candidate in /opt/homebrew/bin/rg /usr/local/bin/rg /usr/bin/rg /bin/rg; do
    if [ -x "$candidate" ] && ! is_bootstrap_rg "$candidate" && is_working_rg "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  return 1
}

rg_bin="$(find_working_rg || true)"
if [ -z "$rg_bin" ]; then
  printf '%s\n' 'browser-use terminal could not find a working ripgrep executable for `rg`.' >&2
  printf '%s\n' 'Release and dev builds should include `bin/agent-tools/rg`; if this is a source checkout, run `scripts/dev-bin.sh` or `scripts/install-agent-ripgrep.sh target/debug/agent-tools`.' >&2
  printf '%s\n' 'If the only `rg` on PATH is a DotSlash launcher, `dotslash` must be available in this same agent shell.' >&2
  exit 127
fi

exec "$rg_bin" "$@"
"#;

pub(crate) fn apply_agent_tool_path_to_command(command: &mut Command) {
    if let Some(path) = agent_tool_path() {
        command.env("PATH", path);
    }
}

pub(crate) fn agent_tool_path() -> Option<OsString> {
    let mut paths = agent_tool_prefix_dirs()?;
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(paths).ok()
}

pub(crate) fn agent_tool_shell_path_restore() -> Option<String> {
    let prefix = agent_tool_prefix_dirs()?;
    let quoted = prefix
        .iter()
        .map(|dir| shell_single_quote(&dir.display().to_string()))
        .collect::<Vec<_>>()
        .join(":");
    Some(format!("export PATH={}:\"$PATH\"\n", quoted))
}

pub(crate) fn ripgrep_command_path() -> PathBuf {
    agent_tool_prefix_dirs()
        .and_then(|dirs| dirs.into_iter().next())
        .map(|bin_dir| bin_dir.join("rg"))
        .unwrap_or_else(|| PathBuf::from("rg"))
}

fn agent_tool_prefix_dirs() -> Option<Vec<PathBuf>> {
    let mut dirs = vec![ensure_agent_tool_bin_dir()?];
    dirs.extend(managed_agent_tool_dirs());
    Some(dedupe_paths(dirs))
}

fn managed_agent_tool_dirs() -> Vec<PathBuf> {
    managed_agent_tool_dirs_for(
        std::env::var_os(BUT_AGENT_TOOLS_DIR_ENV),
        std::env::current_exe().ok(),
    )
}

fn managed_agent_tool_dirs_for(
    explicit_dir: Option<OsString>,
    current_exe: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = explicit_dir {
        push_existing_dir(&mut dirs, PathBuf::from(dir));
    }
    if let Some(exe) = current_exe {
        if let Some(bin_dir) = exe.parent() {
            push_existing_dir(&mut dirs, bin_dir.join("agent-tools"));
        }
    }
    dedupe_paths(dirs)
}

fn push_existing_dir(dirs: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() {
        dirs.push(path);
    }
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        if !unique.iter().any(|existing| existing == &path) {
            unique.push(path);
        }
    }
    unique
}

fn ensure_agent_tool_bin_dir() -> Option<PathBuf> {
    let bin_dir = std::env::temp_dir().join(format!(
        "browser-use-terminal-agent-tools-{}",
        std::process::id()
    ));
    if write_rg_bootstrap_wrapper(&bin_dir.join("rg")).is_err() {
        return None;
    }
    Some(bin_dir)
}

fn write_rg_bootstrap_wrapper(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }

    let needs_write = std::fs::read_to_string(path)
        .map(|current| current != RG_BOOTSTRAP_SCRIPT)
        .unwrap_or(true);
    if needs_write {
        let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp_path, RG_BOOTSTRAP_SCRIPT)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(tmp_path, path)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn write_executable(path: &Path, content: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, content).expect("write executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod executable");
    }

    #[cfg(unix)]
    #[test]
    fn rg_bootstrap_skips_broken_launcher_and_execs_working_ripgrep() {
        let tmp = TempDir::new().expect("tmp");
        let wrapper_dir = tmp.path().join("wrapper");
        let second_wrapper_dir = tmp.path().join("second-wrapper");
        let broken_dir = tmp.path().join("broken");
        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(&wrapper_dir).expect("wrapper dir");
        std::fs::create_dir_all(&second_wrapper_dir).expect("second wrapper dir");
        std::fs::create_dir_all(&broken_dir).expect("broken dir");
        std::fs::create_dir_all(&real_dir).expect("real dir");

        write_rg_bootstrap_wrapper(&wrapper_dir.join("rg")).expect("write rg wrapper");
        write_rg_bootstrap_wrapper(&second_wrapper_dir.join("rg"))
            .expect("write second rg wrapper");
        write_executable(
            &broken_dir.join("rg"),
            "#!/bin/sh\nprintf 'env: dotslash: No such file or directory\\n' >&2\nexit 127\n",
        );
        write_executable(
            &real_dir.join("rg"),
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'ripgrep 99.0.0\\n'; exit 0; fi\nif [ \"$1\" = \"--files\" ]; then printf 'file.txt\\n'; exit 0; fi\nprintf 'fake rg %s\\n' \"$*\"\n",
        );

        let path = std::env::join_paths([wrapper_dir, second_wrapper_dir, broken_dir, real_dir])
            .expect("join path");
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg("command -v rg && rg --version && rg --files")
            .env("PATH", path)
            .stdin(Stdio::null())
            .output()
            .expect("run shell");

        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("/wrapper/rg"), "{stdout}");
        assert!(stdout.contains("ripgrep 99.0.0"), "{stdout}");
        assert!(stdout.contains("file.txt"), "{stdout}");
    }

    #[test]
    fn managed_agent_tool_dirs_include_explicit_and_release_relative_dirs() {
        let tmp = TempDir::new().expect("tmp");
        let explicit = tmp.path().join("explicit-tools");
        let release_bin = tmp.path().join("release").join("bin");
        let release_tools = release_bin.join("agent-tools");
        std::fs::create_dir_all(&explicit).expect("explicit tools");
        std::fs::create_dir_all(&release_tools).expect("release tools");
        let exe = release_bin.join("but");

        let dirs = managed_agent_tool_dirs_for(Some(explicit.clone().into_os_string()), Some(exe));

        assert_eq!(dirs, vec![explicit, release_tools]);
    }

    #[test]
    fn managed_agent_tool_dirs_skip_missing_dirs_and_dedupe() {
        let tmp = TempDir::new().expect("tmp");
        let release_bin = tmp.path().join("release").join("bin");
        let release_tools = release_bin.join("agent-tools");
        std::fs::create_dir_all(&release_tools).expect("release tools");
        let exe = release_bin.join("but");

        let dirs =
            managed_agent_tool_dirs_for(Some(release_tools.clone().into_os_string()), Some(exe));

        assert_eq!(dirs, vec![release_tools]);
    }
}

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from .binary import (
    BinaryNotFoundError,
    _agent_tools_dir_contains_ripgrep,
    _package_dir,
    _source_repo_root,
    agent_tools_dir,
    binary_path,
)

AGENT_TOOLS_DIR_ENV = "BUT_AGENT_TOOLS_DIR"


def main() -> None:
    _exec_or_run_from_source("browser-use-cli", "browser-use-terminal", sys.argv[1:])


def tui_main() -> None:
    _exec_or_run_from_source("browser-use-tui", "but", sys.argv[1:])


def _exec_or_run_from_source(package: str, binary: str, args: list[str]) -> None:
    os.environ.setdefault("BROWSER_USE_PYTHON", sys.executable)
    try:
        path = binary_path(binary)
    except BinaryNotFoundError:
        source_root = _source_repo_root(_package_dir())
        if source_root:
            os.chdir(source_root)
            _ensure_agent_ripgrep(source_root)
            _configure_agent_tools_env(source_root / "target" / "debug" / "agent-tools")
            raise SystemExit(subprocess.call(["cargo", "run", "-q", "-p", package, "--", *args]))
        raise
    _configure_agent_tools_env()
    os.execv(path, [path, *args])


def _configure_agent_tools_env(agent_tools_path: Path | None = None) -> None:
    env_path = os.environ.get(AGENT_TOOLS_DIR_ENV)
    if env_path:
        candidate = Path(env_path)
    elif agent_tools_path is not None:
        candidate = agent_tools_path
    else:
        try:
            candidate = Path(agent_tools_dir())
        except BinaryNotFoundError:
            return

    if not _agent_tools_dir_contains_ripgrep(candidate):
        return

    os.environ[AGENT_TOOLS_DIR_ENV] = str(candidate)
    _prepend_path(candidate)


def _prepend_path(directory: Path) -> None:
    directory_str = str(directory)
    path_parts = [part for part in os.environ.get("PATH", "").split(os.pathsep) if part and part != directory_str]
    os.environ["PATH"] = os.pathsep.join([directory_str, *path_parts])


def _ensure_agent_ripgrep(repo_root: Path) -> None:
    script = repo_root / "scripts" / "install-agent-ripgrep.sh"
    if not script.exists():
        return
    dest = repo_root / "target" / "debug" / "agent-tools"
    rg = dest / "rg"
    rg_exe = dest / "rg.exe"
    if rg.exists() or rg_exe.exists():
        return
    subprocess.run(
        [str(script), str(dest)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from .binary import BinaryNotFoundError, _package_dir, _source_repo_root, binary_path


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
            raise SystemExit(subprocess.call(["cargo", "run", "-q", "-p", package, "--", *args]))
        raise
    os.execv(path, [path, *args])


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

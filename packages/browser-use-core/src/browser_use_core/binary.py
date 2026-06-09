from __future__ import annotations

import os
from pathlib import Path


class BinaryNotFoundError(FileNotFoundError):
    """Raised when a packaged Browser Use terminal binary cannot be found."""


def binary_path(binary: str = "browser-use-terminal") -> str:
    """Return the absolute path to a packaged Browser Use terminal binary."""

    binary_name = _binary_name(binary)
    package_dir = _package_dir()
    candidates = _binary_candidates(package_dir, binary_name)

    for candidate in candidates:
        if candidate.exists():
            return str(candidate)

    searched = "\n".join(f"  - {path}" for path in candidates)
    raise BinaryNotFoundError(f"Could not find Browser Use binary '{binary_name}'. Searched:\n{searched}")


def agent_tools_dir() -> str:
    """Return the directory containing packaged agent helper tools."""

    candidates = _agent_tools_dir_candidates(_package_dir())

    for candidate in candidates:
        if _agent_tools_dir_contains_ripgrep(candidate):
            return str(candidate)

    searched = "\n".join(f"  - {path}" for path in candidates)
    raise BinaryNotFoundError(f"Could not find Browser Use agent tools directory. Searched:\n{searched}")


def _binary_candidates(package_dir: Path, binary_name: str) -> list[Path]:
    candidates = [package_dir / "bin" / binary_name]
    if _is_agent_tool_binary(binary_name):
        candidates.append(package_dir / "bin" / "agent-tools" / binary_name)

    source_root = _source_repo_root(package_dir)
    if source_root:
        candidates.extend(
            [
                source_root / "target" / "debug" / binary_name,
                source_root / "target" / "release" / binary_name,
            ]
        )
        if _is_agent_tool_binary(binary_name):
            candidates.extend(
                [
                    source_root / "target" / "debug" / "agent-tools" / binary_name,
                    source_root / "target" / "release" / "agent-tools" / binary_name,
                ]
            )

    return candidates


def _agent_tools_dir_candidates(package_dir: Path) -> list[Path]:
    candidates = [package_dir / "bin" / "agent-tools"]
    source_root = _source_repo_root(package_dir)
    if source_root:
        candidates.extend(
            [
                source_root / "target" / "debug" / "agent-tools",
                source_root / "target" / "release" / "agent-tools",
            ]
        )
    return candidates


def _agent_tools_dir_contains_ripgrep(directory: Path) -> bool:
    return (directory / _agent_tools_ripgrep_name()).exists()


def _agent_tools_ripgrep_name() -> str:
    return "rg.exe" if _is_windows() else "rg"


def _is_agent_tool_binary(binary_name: str) -> bool:
    return binary_name in {"rg", "rg.exe"}


def _package_dir() -> Path:
    return Path(__file__).resolve().parent


def _source_repo_root(package_dir: Path) -> Path | None:
    for parent in package_dir.parents:
        if (parent / "Cargo.toml").exists():
            return parent
    return None


def _binary_name(binary: str) -> str:
    if not binary or Path(binary).name != binary:
        raise ValueError("binary must be a bare file name")
    if _is_windows() and not binary.endswith(".exe"):
        return f"{binary}.exe"
    return binary


def _is_windows() -> bool:
    return os.name == "nt"

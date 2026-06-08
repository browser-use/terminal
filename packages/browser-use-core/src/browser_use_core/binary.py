from __future__ import annotations

import os
from pathlib import Path


class BinaryNotFoundError(FileNotFoundError):
    """Raised when a packaged Browser Use terminal binary cannot be found."""


def binary_path(binary: str = "browser-use-terminal") -> str:
    """Return the absolute path to a packaged Browser Use terminal binary."""

    binary_name = _binary_name(binary)
    package_dir = _package_dir()
    candidates = [package_dir / "bin" / binary_name]
    source_root = _source_repo_root(package_dir)
    if source_root:
        candidates.extend(
            [
                source_root / "target" / "debug" / binary_name,
                source_root / "target" / "release" / binary_name,
            ]
        )

    for candidate in candidates:
        if candidate.exists():
            return str(candidate)

    searched = "\n".join(f"  - {path}" for path in candidates)
    raise BinaryNotFoundError(f"Could not find Browser Use terminal binary '{binary_name}'. Searched:\n{searched}")


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

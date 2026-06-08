from __future__ import annotations

import pytest


def test_binary_path_prefers_packaged_binary(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    package_dir = tmp_path / "browser_use_core"
    binary = package_dir / "bin" / "browser-use-terminal"
    binary.parent.mkdir(parents=True)
    binary.write_text("#!/bin/sh\n")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))

    assert packaged.binary_path("browser-use-terminal") == str(binary)


def test_binary_path_falls_back_to_source_target(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    repo_root = tmp_path / "repo"
    package_dir = repo_root / "packages" / "browser-use-core" / "src" / "browser_use_core"
    package_dir.mkdir(parents=True)
    (repo_root / "Cargo.toml").write_text("[workspace]\n")
    binary = repo_root / "target" / "debug" / "browser-use-terminal"
    binary.parent.mkdir(parents=True)
    binary.write_text("#!/bin/sh\n")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))

    assert packaged.binary_path("browser-use-terminal") == str(binary)


def test_binary_path_adds_windows_exe_suffix(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    package_dir = tmp_path / "browser_use_core"
    binary = package_dir / "bin" / "browser-use-terminal.exe"
    binary.parent.mkdir(parents=True)
    binary.write_text("")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))
    monkeypatch.setattr(packaged, "_is_windows", lambda: True)

    assert packaged.binary_path("browser-use-terminal") == str(binary)


def test_binary_path_rejects_paths():
    from browser_use_core import binary_path

    with pytest.raises(ValueError):
        binary_path("../browser-use-terminal")

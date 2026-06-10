from __future__ import annotations

import os

import pytest


def test_binary_path_prefers_packaged_binary(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    package_dir = tmp_path / "browser_use_core"
    binary = package_dir / "bin" / "browser-use-terminal"
    binary.parent.mkdir(parents=True)
    binary.write_text("#!/bin/sh\n")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))

    assert packaged.binary_path("browser-use-terminal") == str(binary)


def test_binary_path_finds_packaged_ripgrep(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    package_dir = tmp_path / "browser_use_core"
    rg = package_dir / "bin" / "agent-tools" / "rg"
    rg.parent.mkdir(parents=True)
    rg.write_text("#!/bin/sh\n")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))

    assert packaged.binary_path("rg") == str(rg)


def test_agent_tools_dir_prefers_packaged_agent_tools(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    package_dir = tmp_path / "browser_use_core"
    agent_tools = package_dir / "bin" / "agent-tools"
    agent_tools.mkdir(parents=True)
    (agent_tools / "rg").write_text("#!/bin/sh\n")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))

    assert packaged.agent_tools_dir() == str(agent_tools)


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


def test_binary_path_falls_back_to_source_agent_tools(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    repo_root = tmp_path / "repo"
    package_dir = repo_root / "packages" / "browser-use-core" / "src" / "browser_use_core"
    package_dir.mkdir(parents=True)
    (repo_root / "Cargo.toml").write_text("[workspace]\n")
    rg = repo_root / "target" / "debug" / "agent-tools" / "rg"
    rg.parent.mkdir(parents=True)
    rg.write_text("#!/bin/sh\n")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))

    assert packaged.binary_path("rg") == str(rg)
    assert packaged.agent_tools_dir() == str(rg.parent)


def test_binary_path_adds_windows_exe_suffix(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    package_dir = tmp_path / "browser_use_core"
    binary = package_dir / "bin" / "browser-use-terminal.exe"
    binary.parent.mkdir(parents=True)
    binary.write_text("")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))
    monkeypatch.setattr(packaged, "_is_windows", lambda: True)

    assert packaged.binary_path("browser-use-terminal") == str(binary)


def test_agent_tools_dir_uses_windows_ripgrep_suffix(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged

    package_dir = tmp_path / "browser_use_core"
    agent_tools = package_dir / "bin" / "agent-tools"
    agent_tools.mkdir(parents=True)
    (agent_tools / "rg.exe").write_text("")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))
    monkeypatch.setattr(packaged, "_is_windows", lambda: True)

    assert packaged.binary_path("rg") == str(agent_tools / "rg.exe")
    assert packaged.agent_tools_dir() == str(agent_tools)


def test_binary_path_rejects_paths():
    from browser_use_core import binary_path

    with pytest.raises(ValueError):
        binary_path("../browser-use-terminal")


def test_cli_configures_agent_tools_env_before_exec(monkeypatch, tmp_path):
    import browser_use_core.binary as packaged
    import browser_use_core.cli as cli

    package_dir = tmp_path / "browser_use_core"
    terminal = package_dir / "bin" / "browser-use-terminal"
    agent_tools = package_dir / "bin" / "agent-tools"
    terminal.parent.mkdir(parents=True)
    agent_tools.mkdir(parents=True)
    terminal.write_text("#!/bin/sh\n")
    (agent_tools / "rg").write_text("#!/bin/sh\n")
    monkeypatch.setattr(packaged, "__file__", str(package_dir / "binary.py"))
    monkeypatch.delenv("BUT_AGENT_TOOLS_DIR", raising=False)
    monkeypatch.setenv("PATH", "/usr/bin")

    captured = {}

    def fake_execv(path, argv):
        captured["path"] = path
        captured["argv"] = argv
        captured["agent_tools_dir"] = os.environ.get("BUT_AGENT_TOOLS_DIR")
        captured["path_env"] = os.environ.get("PATH")
        raise RuntimeError("execv called")

    monkeypatch.setattr(cli.os, "execv", fake_execv)

    with pytest.raises(RuntimeError, match="execv called"):
        cli._exec_or_run_from_source("browser-use-cli", "browser-use-terminal", ["sdk-server"])

    assert captured["path"] == str(terminal)
    assert captured["argv"] == [str(terminal), "sdk-server"]
    assert captured["agent_tools_dir"] == str(agent_tools)
    assert captured["path_env"].split(os.pathsep)[0] == str(agent_tools)

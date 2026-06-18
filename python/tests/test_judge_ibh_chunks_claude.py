from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "judge-ibh-chunks-claude.py"


def load_module():
    spec = importlib.util.spec_from_file_location("judge_ibh_chunks_claude", SCRIPT)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def write_packet_chunk(judge_dir: Path, label: str, task_id: str) -> None:
    (judge_dir / f"packets_{label}.json").write_text(
        json.dumps(
            [
                {
                    "task_id": task_id,
                    "ok": True,
                    "task": "Find the value.",
                    "final_result": "42",
                    "event_log": str(judge_dir / "native-events" / f"{task_id}.jsonl"),
                    "cwd": str(judge_dir / "cwd"),
                    "artifact_root": str(judge_dir / "artifacts"),
                }
            ]
        )
    )
    (judge_dir / f"judge_brief_{label}.md").write_text("Judge this chunk.")


def test_extract_json_array_accepts_plain_and_fenced_output() -> None:
    module = load_module()

    assert module.extract_json_array('[{"task_id":"a"}]') == [{"task_id": "a"}]
    assert module.extract_json_array('```json\n[{"task_id":"b"}]\n```') == [{"task_id": "b"}]


def test_validate_rows_rejects_missing_task(tmp_path: Path) -> None:
    module = load_module()
    packets = tmp_path / "packets_001_001.json"
    packets.write_text(json.dumps([{"task_id": "a"}, {"task_id": "b"}]))

    rows = [
        {
            "task_id": "a",
            "runner_ok": True,
            "verdict": "pass",
            "score": 1,
            "reasoning": "supported",
            "evidence_checked": "result file",
            "failure_class": "none",
        }
    ]

    try:
        module.validate_rows(rows, packets)
    except ValueError as exc:
        assert "missing judged task ids: b" in str(exc)
    else:
        raise AssertionError("expected missing task validation error")


def test_judge_runner_writes_validated_chunks_with_fake_claude(tmp_path: Path) -> None:
    judge_dir = tmp_path / "judge"
    judge_dir.mkdir()
    (judge_dir / "judge_prompt.md").write_text("Locked rubric.")
    write_packet_chunk(judge_dir, "001_001", "alpha")
    write_packet_chunk(judge_dir, "002_002", "beta")

    fake_claude = tmp_path / "fake-claude"
    fake_claude.write_text(
        """#!/usr/bin/env python3
import json
import re
import sys
prompt = sys.argv[-1]
match = re.search(r"(/\\S*packets_[0-9_]+\\.json)", prompt)
packet_path = match.group(0)
with open(packet_path) as handle:
    packets = json.load(handle)
print(json.dumps([
    {
        "task_id": packet["task_id"],
        "runner_ok": bool(packet.get("ok")),
        "verdict": "pass",
        "score": 1,
        "reasoning": "fake judged",
        "evidence_checked": "fake evidence",
        "failure_class": "none"
    }
    for packet in packets
]))
"""
    )
    fake_claude.chmod(0o755)

    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--judge-dir",
            str(judge_dir),
            "--run-root",
            str(tmp_path),
            "--claude-bin",
            str(fake_claude),
            "--concurrency",
            "2",
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )

    assert result.returncode == 0, result.stderr + result.stdout
    assert json.loads((judge_dir / "chunk_001_001.json").read_text())[0]["task_id"] == "alpha"
    assert json.loads((judge_dir / "chunk_002_002.json").read_text())[0]["task_id"] == "beta"

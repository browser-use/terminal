from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "audit-ibh-run-completion.py"


def judge_row(task_id: str) -> dict[str, object]:
    return {
        "task_id": task_id,
        "runner_ok": True,
        "verdict": "pass",
        "score": 1,
        "reasoning": "supported",
        "evidence_checked": "result file and event log",
        "failure_class": "none",
    }


def create_complete_run(root: Path, run_id: str = "fake-run") -> None:
    ids = ["alpha", "beta"]
    manifest_dir = root / "state" / "dataset-runs"
    manifest_dir.mkdir(parents=True)
    (manifest_dir / f"{run_id}.json").write_text(
        json.dumps(
            {
                "selection": [
                    {"task_id": task_id, "confirmed_task": f"Task {task_id}"}
                    for task_id in ids
                ],
                "sessions": [
                    {
                        "task_id": task_id,
                        "ok": True,
                        "final_result": f"answer {task_id}",
                        "session": {
                            "id": f"session-{task_id}",
                            "cwd": str(root / "files" / task_id / "cwd"),
                            "artifact_root": str(root / "files" / task_id / "artifacts"),
                        },
                    }
                    for task_id in ids
                ],
            }
        )
    )
    packets = [
        {
            "task_id": task_id,
            "ok": True,
            "task": f"Task {task_id}",
            "final_result": f"answer {task_id}",
            "cwd": str(root / "files" / task_id / "cwd"),
            "artifact_root": str(root / "files" / task_id / "artifacts"),
        }
        for task_id in ids
    ]
    (root / "judge_packets.json").write_text(json.dumps(packets))

    judge = root / "judge"
    events = judge / "native-events"
    events.mkdir(parents=True)
    for task_id in ids:
        (events / f"task-{task_id}-events.jsonl").write_text(
            json.dumps({"type": "session.done", "payload": {"result": f"answer {task_id}"}})
            + "\n"
        )
    (judge / "packets_all.json").write_text(json.dumps(packets))
    (judge / "packets_001_002.json").write_text(json.dumps(packets))
    (judge / "chunk_001_002.json").write_text(json.dumps([judge_row(task_id) for task_id in ids]))
    (judge / "judge_aggregate.json").write_text(
        json.dumps(
            {
                "total": 2,
                "expected_total": 2,
                "passed": 2,
                "failed": 0,
                "problems": [],
                "results": [judge_row(task_id) for task_id in ids],
            }
        )
    )
    (root / "current-vs-raw-judged-delta.md").write_text("Current strict score | 2/2\n")


def run_audit(root: Path, *extra: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--run-root",
            str(root),
            "--run-id",
            "fake-run",
            "--expected-total",
            "2",
            *extra,
        ],
        cwd=REPO_ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def test_completion_audit_accepts_complete_judged_run(tmp_path: Path) -> None:
    create_complete_run(tmp_path)

    result = run_audit(tmp_path, "--require-judged")

    assert result.returncode == 0, result.stderr + result.stdout
    summary = json.loads(result.stdout)
    assert summary["ok"] is True
    assert summary["judge_packets"] == 2
    assert summary["native_event_logs"] == 2
    assert summary["judge_chunk_total"] == 2
    assert summary["aggregate_passed"] == 2


def test_completion_audit_rejects_missing_comparison(tmp_path: Path) -> None:
    create_complete_run(tmp_path)
    (tmp_path / "current-vs-raw-judged-delta.md").unlink()

    result = run_audit(tmp_path, "--require-judged")

    assert result.returncode == 1
    summary = json.loads(result.stdout)
    assert summary["ok"] is False
    assert any("comparison missing" in problem for problem in summary["problems"])

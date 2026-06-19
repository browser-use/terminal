from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "run-internal-bench-hard-openai.sh"


def wrapper_env(tmp_path: Path) -> dict[str, str]:
    dataset = tmp_path / "Internal_Bench_hard.json"
    dataset.write_text("[]\n")
    harness_src = tmp_path / "browser-harness-src"
    (harness_src / "browser_harness").mkdir(parents=True)
    env = os.environ.copy()
    env.update(
        {
            "ENV_FILE": str(tmp_path / "missing.env"),
            "DATASET": str(dataset),
            "BROWSER_HARNESS_SRC": str(harness_src),
            "OUT_BASE": str(tmp_path / "runs"),
        }
    )
    env.pop("OPENAI_API_KEY", None)
    env.pop("LLM_BROWSER_OPENAI_API_KEY", None)
    env.pop("BROWSER_USE_API_KEY", None)
    return env


def run_wrapper(tmp_path: Path, *args: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(SCRIPT), *args],
        cwd=REPO_ROOT,
        env=env or wrapper_env(tmp_path),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def test_internal_bench_wrapper_dry_run_is_cloud_no80_and_can_emit_judge_command(
    tmp_path: Path,
) -> None:
    result = run_wrapper(
        tmp_path,
        "--dry-run",
        "--judge",
        "--run-id",
        "dry-judge",
        "--root",
        str(tmp_path / "run-root"),
    )

    assert result.returncode == 0, result.stderr + result.stdout
    assert "dataset-run-openai" in result.stdout
    assert "--max-turns 10000" in result.stdout
    assert "--browser-mode cloud" in result.stdout
    assert "-c simple_harness=true" in result.stdout
    assert "-c disable_local_search=true" in result.stdout
    assert "judge_command:" in result.stdout
    assert "scripts/judge-ibh-chunks-claude.py" in result.stdout
    assert "--concurrency 5" in result.stdout
    assert "OPENAI_API_KEY" not in result.stdout
    assert "BROWSER_USE_API_KEY" not in result.stdout


def test_internal_bench_wrapper_rejects_80_turns_before_dry_run(tmp_path: Path) -> None:
    env = wrapper_env(tmp_path)
    env["MAX_TURNS"] = "80"

    result = run_wrapper(tmp_path, "--dry-run", env=env)

    assert result.returncode == 2
    assert "MAX_TURNS must stay >= 10000" in result.stderr
    assert "command:" not in result.stdout


def test_internal_bench_wrapper_loads_env_file_defaults_for_eval_and_judge(
    tmp_path: Path,
) -> None:
    env_file = tmp_path / "bench.env"
    env_file.write_text(
        "\n".join(
            [
                "MODEL=gpt-test",
                "CONCURRENCY=11",
                "JUDGE_MODEL=judge-test",
                "JUDGE_CONCURRENCY=3",
                "JUDGE_CLAUDE_BIN=/tmp/fake-claude",
            ]
        )
        + "\n"
    )
    env = wrapper_env(tmp_path)
    env["ENV_FILE"] = str(env_file)

    result = run_wrapper(
        tmp_path,
        "--dry-run",
        "--judge",
        "--run-id",
        "env-dry",
        "--root",
        str(tmp_path / "env-run"),
        env=env,
    )

    assert result.returncode == 0, result.stderr + result.stdout
    assert "--model gpt-test" in result.stdout
    assert "--concurrency 11" in result.stdout
    assert "--model judge-test" in result.stdout
    assert "--concurrency 3" in result.stdout
    assert "--claude-bin /tmp/fake-claude" in result.stdout


def write_fake_terminal(path: Path) -> None:
    path.write_text(
        """#!/usr/bin/env python3
import json
import sqlite3
import sys
from pathlib import Path

args = sys.argv[1:]
if args == ["auth", "status"]:
    print("Browser Use Cloud key: connected (stored)")
    print("OpenAI API key: connected (stored)")
    raise SystemExit(0)

state_dir = Path(args[args.index("--state-dir") + 1])
run_id = args[args.index("--run-id") + 1]
manifest_dir = state_dir / "dataset-runs"
files_root = state_dir / "dataset-run-files" / run_id
manifest_dir.mkdir(parents=True, exist_ok=True)
files_root.mkdir(parents=True, exist_ok=True)
ids = [f"task-{idx:03d}" for idx in range(1, 107)]
selection = []
sessions = []
db_path = state_dir / "state.db"
db_path.parent.mkdir(parents=True, exist_ok=True)
conn = sqlite3.connect(db_path)
try:
    conn.execute(
        "create table sessions (id text primary key, status text, created_ms integer, updated_ms integer, cwd text, artifact_root text)"
    )
    conn.execute(
        "create table events (seq integer, id text, session_id text, ts_ms integer, type text, payload_json text)"
    )
    seq = 1
    for index, task_id in enumerate(ids, start=1):
        session_id = f"session-{task_id}"
        task_root = files_root / f"task-{task_id}-attempt-1"
        cwd = task_root / "cwd"
        artifact_root = task_root / "artifacts"
        cwd.mkdir(parents=True, exist_ok=True)
        artifact_root.mkdir(parents=True, exist_ok=True)
        (cwd / "result.txt").write_text(f"retrieved answer for {task_id}\\n")
        selection.append({"task_id": task_id, "confirmed_task": f"Task {task_id}"})
        sessions.append(
            {
                "task_id": task_id,
                "ok": True,
                "final_result": f"retrieved answer for {task_id}",
                "session": {
                    "id": session_id,
                    "cwd": str(cwd),
                    "artifact_root": str(artifact_root),
                },
            }
        )
        conn.execute(
            "insert into sessions values (?, ?, ?, ?, ?, ?)",
            (session_id, "done", index, index, str(cwd), str(artifact_root)),
        )
        for event_type, payload in [
            ("dataset.case", {"task_id": task_id}),
            ("exec_command.end", {"output": f"retrieved answer for {task_id}"}),
            ("session.done", {"result": f"retrieved answer for {task_id}"}),
        ]:
            conn.execute(
                "insert into events values (?, ?, ?, ?, ?, ?)",
                (
                    seq,
                    f"event-{seq}",
                    session_id,
                    seq,
                    event_type,
                    json.dumps(payload),
                ),
            )
            seq += 1
    conn.commit()
finally:
    conn.close()

manifest = {
    "summary": {"count": len(ids), "passed": len(ids), "failed": 0},
    "selection": selection,
    "sessions": sessions,
}
(manifest_dir / f"{run_id}.json").write_text(json.dumps(manifest))
print(f"fake dataset run complete: {run_id}")
"""
    )
    path.chmod(0o755)


def write_fake_disconnected_terminal(path: Path) -> None:
    path.write_text(
        """#!/usr/bin/env python3
import sys

if sys.argv[1:] == ["auth", "status"]:
    print("Browser Use Cloud key: not connected")
    print("OpenAI API key: not connected")
    raise SystemExit(0)

raise SystemExit("unexpected command")
"""
    )
    path.chmod(0o755)


def write_fake_claude(path: Path) -> None:
    path.write_text(
        """#!/usr/bin/env python3
import json
import re
import sys

prompt = sys.stdin.read() or sys.argv[-1]
match = re.search(r"(/\\S*packets_[0-9_]+\\.json)", prompt)
if not match:
    raise SystemExit("packet path not found in prompt")
with open(match.group(1)) as handle:
    packets = json.load(handle)
print(json.dumps([
    {
        "task_id": packet["task_id"],
        "runner_ok": bool(packet.get("ok")),
        "verdict": "pass",
        "score": 1,
        "reasoning": "fake judge accepted saved evidence",
        "evidence_checked": "fake event log and result file",
        "failure_class": "none",
    }
    for packet in packets
]))
"""
    )
    path.chmod(0o755)


def write_fake_reference(path: Path) -> None:
    rows = [
        {
            "task_id": f"task-{idx:03d}",
            "runner_ok": True,
            "verdict": "pass",
            "score": 1,
            "reasoning": "fake reference",
            "evidence_checked": "fake reference",
            "failure_class": "none",
        }
        for idx in range(1, 107)
    ]
    path.write_text(json.dumps({"total": 106, "results": rows}))


def test_internal_bench_wrapper_reports_all_missing_credentials(tmp_path: Path) -> None:
    fake_terminal = tmp_path / "fake-browser-use-terminal"
    write_fake_disconnected_terminal(fake_terminal)
    env = wrapper_env(tmp_path)
    env["BROWSER_USE_TERMINAL_BIN"] = str(fake_terminal)

    result = run_wrapper(
        tmp_path,
        "--skip-build",
        "--run-id",
        "missing-creds",
        "--root",
        str(tmp_path / "missing-creds"),
        env=env,
    )

    assert result.returncode == 1
    assert "missing OPENAI_API_KEY or LLM_BROWSER_OPENAI_API_KEY" in result.stderr
    assert "missing BROWSER_USE_API_KEY" in result.stderr
    assert "command:" not in result.stdout


def test_internal_bench_wrapper_judge_preflight_requires_reference_aggregate(
    tmp_path: Path,
) -> None:
    fake_terminal = tmp_path / "fake-browser-use-terminal"
    fake_claude = tmp_path / "fake-claude"
    write_fake_terminal(fake_terminal)
    write_fake_claude(fake_claude)
    env = wrapper_env(tmp_path)
    env.update(
        {
            "BROWSER_USE_TERMINAL_BIN": str(fake_terminal),
            "JUDGE_CLAUDE_BIN": str(fake_claude),
            "REFERENCE_AGGREGATE": str(tmp_path / "missing-reference.json"),
        }
    )

    result = run_wrapper(
        tmp_path,
        "--skip-build",
        "--judge",
        "--run-id",
        "missing-reference",
        "--root",
        str(tmp_path / "missing-reference"),
        env=env,
    )

    assert result.returncode == 1
    assert "reference aggregate not found for --judge" in result.stderr
    assert "fake dataset run complete" not in result.stdout


def test_internal_bench_wrapper_judge_preflight_requires_claude_binary(
    tmp_path: Path,
) -> None:
    fake_terminal = tmp_path / "fake-browser-use-terminal"
    fake_reference = tmp_path / "reference-aggregate.json"
    write_fake_terminal(fake_terminal)
    write_fake_reference(fake_reference)
    missing_claude = tmp_path / "missing-claude"
    env = wrapper_env(tmp_path)
    env.update(
        {
            "BROWSER_USE_TERMINAL_BIN": str(fake_terminal),
            "JUDGE_CLAUDE_BIN": str(missing_claude),
            "REFERENCE_AGGREGATE": str(fake_reference),
        }
    )

    result = run_wrapper(
        tmp_path,
        "--skip-build",
        "--judge",
        "--run-id",
        "missing-claude",
        "--root",
        str(tmp_path / "missing-claude-run"),
        env=env,
    )

    assert result.returncode == 1
    assert "judge claude binary not found for --judge" in result.stderr
    assert "fake dataset run complete" not in result.stdout


def test_internal_bench_wrapper_can_run_prepare_judge_and_finalize_with_fakes(
    tmp_path: Path,
) -> None:
    fake_terminal = tmp_path / "fake-browser-use-terminal"
    fake_claude = tmp_path / "fake-claude"
    fake_reference = tmp_path / "reference-aggregate.json"
    write_fake_terminal(fake_terminal)
    write_fake_claude(fake_claude)
    write_fake_reference(fake_reference)

    env = wrapper_env(tmp_path)
    env.update(
        {
            "BROWSER_USE_TERMINAL_BIN": str(fake_terminal),
            "JUDGE_CLAUDE_BIN": str(fake_claude),
            "REFERENCE_AGGREGATE": str(fake_reference),
            "HEALTH_AFTER_SECONDS": "0",
            "OPENAI_API_KEY": "test-openai-key",
            "BROWSER_USE_API_KEY": "test-browser-use-key",
        }
    )
    root = tmp_path / "full-wrapper"

    result = run_wrapper(
        tmp_path,
        "--skip-build",
        "--judge",
        "--run-id",
        "fake-full",
        "--root",
        str(root),
        env=env,
    )

    assert result.returncode == 0, result.stderr + result.stdout
    packets = json.loads((root / "judge_packets.json").read_text())
    assert len(packets) == 106
    chunks = sorted((root / "judge").glob("chunk_*.json"))
    assert [path.name for path in chunks] == [
        "chunk_001_022.json",
        "chunk_023_044.json",
        "chunk_045_066.json",
        "chunk_067_088.json",
        "chunk_089_106.json",
    ]
    aggregate = json.loads((root / "judge" / "judge_aggregate.json").read_text())
    assert aggregate["passed"] == 106
    assert aggregate["failed"] == 0
    comparison = (root / "current-vs-raw-judged-delta.md").read_text()
    assert "Current strict score | 106/106" in comparison
    run_env = (root / "run-env.txt").read_text()
    assert "max_turns=10000" in run_env
    assert "browser_mode=cloud" in run_env
    assert "simple_harness=true" in run_env
    assert f"reference_aggregate={fake_reference}" in run_env

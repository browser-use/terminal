from __future__ import annotations

import json
import sqlite3
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PREPARE_SCRIPT = REPO_ROOT / "scripts" / "prepare-ibh-judge.py"


def run_prepare(
    *,
    run_root: Path,
    packets: Path,
    state_db: Path,
    out_dir: Path,
) -> dict[str, object]:
    result = subprocess.run(
        [
            sys.executable,
            str(PREPARE_SCRIPT),
            "--run-root",
            str(run_root),
            "--packets",
            str(packets),
            "--state-db",
            str(state_db),
            "--out-dir",
            str(out_dir),
            "--expected-total",
            "1",
            "--chunk-size",
            "1",
        ],
        cwd=REPO_ROOT,
        text=True,
        capture_output=True,
        check=True,
    )
    return json.loads(result.stdout)


def test_prepare_ibh_judge_uses_packet_event_log_when_sqlite_is_absent(
    tmp_path: Path,
) -> None:
    run_root = tmp_path / "run"
    task_root = run_root / "task-jsonl"
    cwd = task_root / "cwd"
    cwd.mkdir(parents=True)
    (cwd / "result.json").write_text(json.dumps({"answer": "retrieved value"}))
    event_log = task_root / "events.jsonl"
    event_log.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "meta",
                        "thread_id": "thread-jsonl",
                        "model": "gpt-5.5",
                    }
                ),
                json.dumps(
                    {
                        "type": "tool.output",
                        "payload": {"text": "retrieved value"},
                    }
                ),
            ]
        )
        + "\n"
    )
    packets = run_root / "judge_packets.json"
    packets.write_text(
        json.dumps(
            [
                {
                    "task_id": "jsonl",
                    "ok": True,
                    "task": "find a value",
                    "final_result": "retrieved value",
                    "cwd": str(cwd),
                    "artifact_root": str(task_root),
                    "event_log": str(event_log),
                }
            ]
        )
    )

    out_dir = tmp_path / "judge"
    summary = run_prepare(
        run_root=run_root,
        packets=packets,
        state_db=run_root / "state" / "state.db",
        out_dir=out_dir,
    )

    assert summary["packets"] == 1
    assert summary["event_logs"] == 1
    assert summary["missing_native_sessions"] == []
    enriched = json.loads((out_dir / "packets_all.json").read_text())[0]
    assert enriched["native_event_source"] == "packet_event_log"
    assert enriched["native_sqlite"] is None
    assert enriched["native_sessions"][0]["id"] == "thread-jsonl"
    assert enriched["native_sessions"][0]["event_count"] == 2
    assert Path(enriched["event_log"]).read_text() == event_log.read_text()
    assert any(
        file["relative_path"] == "result.json"
        for file in enriched["artifact_files"]
        if file["root"] == "cwd"
    )
    prompt = (out_dir / "judge_prompt.md").read_text()
    assert "packet `event_log` JSONL files copied into `native-events/`" in prompt


def test_prepare_ibh_judge_exports_native_sqlite_events(tmp_path: Path) -> None:
    run_root = tmp_path / "run"
    cwd = run_root / "cwd"
    artifact_root = run_root / "artifacts"
    cwd.mkdir(parents=True)
    artifact_root.mkdir(parents=True)
    state_dir = run_root / "state"
    state_dir.mkdir(parents=True)
    state_db = state_dir / "state.db"
    conn = sqlite3.connect(state_db)
    try:
        conn.execute(
            """
            create table sessions (
              id text primary key,
              status text,
              created_ms integer,
              updated_ms integer,
              cwd text,
              artifact_root text
            )
            """
        )
        conn.execute(
            """
            create table events (
              seq integer,
              id text,
              session_id text,
              ts_ms integer,
              type text,
              payload_json text
            )
            """
        )
        conn.execute(
            "insert into sessions values (?, ?, ?, ?, ?, ?)",
            ("session-sqlite", "done", 1, 2, str(cwd), str(artifact_root)),
        )
        conn.execute(
            "insert into events values (?, ?, ?, ?, ?, ?)",
            (
                1,
                "event-dataset",
                "session-sqlite",
                10,
                "dataset.case",
                json.dumps({"task_id": "sqlite"}),
            ),
        )
        conn.execute(
            "insert into events values (?, ?, ?, ?, ?, ?)",
            (
                2,
                "event-done",
                "session-sqlite",
                20,
                "session.done",
                json.dumps({"result": "sqlite result"}),
            ),
        )
        conn.commit()
    finally:
        conn.close()

    packets = run_root / "judge_packets.json"
    packets.write_text(
        json.dumps(
            [
                {
                    "task_id": "sqlite",
                    "ok": True,
                    "task": "find sqlite value",
                    "final_result": "sqlite result",
                    "cwd": str(cwd),
                    "artifact_root": str(artifact_root),
                }
            ]
        )
    )

    out_dir = tmp_path / "judge"
    summary = run_prepare(
        run_root=run_root,
        packets=packets,
        state_db=state_db,
        out_dir=out_dir,
    )

    assert summary["packets"] == 1
    assert summary["event_logs"] == 1
    assert summary["missing_native_sessions"] == []
    enriched = json.loads((out_dir / "packets_all.json").read_text())[0]
    assert enriched["native_event_source"] == "sqlite"
    assert enriched["native_sqlite"] == str(state_db.resolve())
    assert enriched["native_sessions"][0]["id"] == "session-sqlite"
    assert enriched["native_sessions"][0]["final_result"] == "sqlite result"
    exported = [
        json.loads(line)
        for line in Path(enriched["event_log"]).read_text().splitlines()
        if line
    ]
    assert [event["type"] for event in exported] == ["dataset.case", "session.done"]
    prompt = (out_dir / "judge_prompt.md").read_text()
    assert f"SQLite `{state_db.resolve()}`" in prompt

#!/usr/bin/env python3
"""Prepare Internal_Bench_hard packets for strict LLM judging.

Input is the runner-produced `judge_packets.json` plus either the run's SQLite
event store or per-task packet `event_log` JSONL files. Output is a judge
directory containing enriched packets, packet chunks, native per-task event
logs, and judge prompt/brief files.
"""

from __future__ import annotations

import argparse
import json
import shutil
import sqlite3
import sys
from pathlib import Path
from typing import Any


RUBRIC = """# GENERIC locked rubric -- Internal_Bench_hard
Score exactly 0 or 1, no partial credit.
1 = the core requested deliverable is present, credible, AND supported by evidence the agent actually retrieved this session.
0 = no answer; materially incomplete; wrong entity/scope/source; hallucinated; answered-from-memory with no retrieval; padded/duplicated rows to hit a count; only partial.
Rules:
- Runner pass != correctness. Runner fail/pending = 0 unless a COMPLETE requested artifact exists on disk.
- Short final prose + a complete correct saved file = 1.
- Honest null / "not found" WITH the checked source noted = acceptable (not an automatic fail).
- Exact counts, specific products, current prices, exact URLs, all-record extraction: require enough saved evidence to be credible.
- Do NOT penalize tool choice (python/http/api/browser all valid). Judge as of run date.
- FAIL signals: fabrication, wrong record/product/entity, missing core fields at scale, source/scope drift to unrelated sites, padding to hit a quantity, blank/blocked screenshot when the screenshot IS the deliverable, confident answer with zero supporting retrieval this session.
This is WebBench-style browser interaction. A task that required reading/filtering live page data but was answered from training knowledge with no retrieval = 0.

Native supervisor evidence instructions:
- Judge `{run_id}` on Internal_Bench_hard using the packet chunks in this directory.
- This is NOT a stock Codex `~/.codex/sessions` run. Do not require `codex_session_files`; they are intentionally empty.
- Each packet has `event_log`, a JSONL file produced either from the native Rust SQLite event store or from the packet's existing per-task event log. It also has `native_sessions`, `cwd`, `artifact_root`, and `artifact_files`.
- Browser-harness evidence appears in native events such as `exec_command.begin`, `exec_command.end`, `exec_command.output_delta`, `tool.output`, `tool.output_delta`, `browser_harness.command_started`, and `browser_harness.command_finished`. Retrieved stdout/stderr is usually in `exec_command.end.payload.output` or output-delta/tool-output payloads.
- For specific claimed values, IDs, prices, counts, URLs, names, products, or records, verify they appear in saved artifacts or in the native event log. If they appear only in `final_result`, treat them as unsupported.
- Saved files under `cwd` or `artifact_root` can be sufficient evidence, especially when a script wrote parsed data directly without echoing every field.
- `runner_ok` / packet `ok` only means the runner emitted done. Strict correctness is your call.
- `runner_state=pending` means the run stopped before final done. Score 1 only if the disk artifact is already a complete requested deliverable; otherwise score 0.

Output schema: write a JSON array of judgment objects with `task_id`, `runner_ok`, `verdict`, `score`, `reasoning`, `evidence_checked`, and `failure_class`. Scores must be 0 or 1 only.

Run-specific context:
- Run label: {run_label}
- Run root: `{run_root}`
- Run id: `{run_id}`
- Packet chunks: `{judge_dir}/packets_<LO>_<HI>.json`
- All packets: `{judge_dir}/packets_all.json`
- Native event source: {event_source}
- Native SQLite, if present: `{state_db}`
- Known runner-pending tasks: {pending_tasks}
- Known runner-failed tasks: {failed_tasks}
"""


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        raise SystemExit(f"missing file: {path}") from None
    except json.JSONDecodeError as exc:
        raise SystemExit(f"invalid JSON in {path}: {exc}") from None


def parse_payload(text: str) -> Any:
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return text


def safe_task_filename(task_id: str) -> str:
    keep = []
    for char in task_id:
        if char.isalnum() or char in ("-", "_"):
            keep.append(char)
        else:
            keep.append("_")
    return "".join(keep) or "unknown"


def scan_files(root: Path | None, label: str) -> list[dict[str, Any]]:
    if root is None or not root.exists() or not root.is_dir():
        return []
    files: list[dict[str, Any]] = []
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        try:
            relative = path.relative_to(root)
            size = path.stat().st_size
        except OSError:
            continue
        files.append(
            {
                "root": label,
                "path": str(path),
                "relative_path": str(relative),
                "size": size,
            }
        )
    return files


def query_task_sessions(conn: sqlite3.Connection) -> dict[str, list[dict[str, Any]]]:
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        """
        select
          s.id,
          s.status,
          s.created_ms,
          s.updated_ms,
          s.cwd,
          s.artifact_root,
          e.payload_json as dataset_payload,
          (select count(*) from events x where x.session_id = s.id) as event_count
        from sessions s
        join events e on e.session_id = s.id and e.type = 'dataset.case'
        order by s.created_ms, s.id
        """
    ).fetchall()

    sessions_by_task: dict[str, list[dict[str, Any]]] = {}
    for row in rows:
        dataset_payload = parse_payload(row["dataset_payload"])
        task_id = dataset_payload.get("task_id") if isinstance(dataset_payload, dict) else None
        if not task_id:
            continue
        session = {
            "id": row["id"],
            "status": row["status"],
            "created_ms": row["created_ms"],
            "updated_ms": row["updated_ms"],
            "cwd": row["cwd"],
            "artifact_root": row["artifact_root"],
            "event_count": row["event_count"],
            "final_result": "",
            "failure": "",
        }
        sessions_by_task.setdefault(str(task_id), []).append(session)

    for sessions in sessions_by_task.values():
        for session in sessions:
            done_rows = conn.execute(
                """
                select payload_json from events
                where session_id = ? and type = 'session.done'
                order by seq desc limit 1
                """,
                (session["id"],),
            ).fetchall()
            if done_rows:
                payload = parse_payload(done_rows[0]["payload_json"])
                if isinstance(payload, dict):
                    session["final_result"] = str(payload.get("result") or "")
            failed_rows = conn.execute(
                """
                select payload_json from events
                where session_id = ? and type = 'session.failed'
                order by seq desc limit 1
                """,
                (session["id"],),
            ).fetchall()
            if failed_rows:
                payload = parse_payload(failed_rows[0]["payload_json"])
                if isinstance(payload, dict):
                    session["failure"] = str(payload.get("error") or payload.get("failure") or payload)
                else:
                    session["failure"] = str(payload)
    return sessions_by_task


def export_event_log(conn: sqlite3.Connection, session_ids: list[str], path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if not session_ids:
        path.write_text("")
        return
    placeholders = ",".join("?" for _ in session_ids)
    rows = conn.execute(
        f"""
        select seq, id, session_id, ts_ms, type, payload_json
        from events
        where session_id in ({placeholders})
        order by seq
        """,
        session_ids,
    ).fetchall()
    with path.open("w") as handle:
        for row in rows:
            record = {
                "seq": row["seq"],
                "id": row["id"],
                "session_id": row["session_id"],
                "ts_ms": row["ts_ms"],
                "type": row["type"],
                "payload": parse_payload(row["payload_json"]),
            }
            handle.write(json.dumps(record, ensure_ascii=False) + "\n")


def count_jsonl_records(path: Path) -> int:
    try:
        with path.open() as handle:
            return sum(1 for line in handle if line.strip())
    except OSError:
        return 0


def first_jsonl_record(path: Path) -> dict[str, Any]:
    try:
        with path.open() as handle:
            for line in handle:
                if not line.strip():
                    continue
                record = parse_payload(line)
                if isinstance(record, dict):
                    return record
    except OSError:
        pass
    return {}


def packet_event_log_source(run_root: Path, packet: dict[str, Any], task_id: str) -> Path | None:
    raw_event_log = packet.get("event_log")
    if isinstance(raw_event_log, str) and raw_event_log:
        path = Path(raw_event_log)
        if path.is_file():
            return path
        if not path.is_absolute():
            candidate = run_root / path
            if candidate.is_file():
                return candidate
    candidate = run_root / f"task-{safe_task_filename(task_id)}" / "events.jsonl"
    if candidate.is_file():
        return candidate
    return None


def packet_sessions_from_event_log(
    *,
    packet: dict[str, Any],
    task_id: str,
    event_log: Path | None,
) -> list[dict[str, Any]]:
    first = first_jsonl_record(event_log) if event_log is not None else {}
    session_id = packet.get("session_id") or first.get("thread_id") or task_id
    return [
        {
            "id": str(session_id),
            "status": "passed" if packet.get("ok") else "failed",
            "created_ms": None,
            "updated_ms": None,
            "cwd": packet.get("cwd"),
            "artifact_root": packet.get("artifact_root"),
            "event_count": count_jsonl_records(event_log) if event_log is not None else 0,
            "final_result": str(packet.get("final_result") or ""),
            "failure": str(packet.get("error") or ""),
        }
    ]


def copy_packet_event_log(source: Path | None, target: Path) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    if source is None or not source.is_file():
        target.write_text("")
        return
    if source.resolve() == target.resolve():
        return
    shutil.copyfile(source, target)


def validate_packets(packets: list[dict[str, Any]], expected_total: int) -> None:
    seen: set[str] = set()
    duplicates: list[str] = []
    for index, packet in enumerate(packets):
        task_id = packet.get("task_id")
        if not isinstance(task_id, str) or not task_id:
            raise SystemExit(f"packet {index} has no task_id")
        if task_id in seen:
            duplicates.append(task_id)
        seen.add(task_id)
    if duplicates:
        raise SystemExit(f"duplicate task ids in packets: {', '.join(sorted(set(duplicates)))}")
    if len(packets) != expected_total:
        raise SystemExit(f"expected {expected_total} packets, found {len(packets)}")


def chunk_ranges(total: int, chunk_size: int) -> list[tuple[int, int]]:
    ranges = []
    start = 1
    while start <= total:
        end = min(total, start + chunk_size - 1)
        ranges.append((start, end))
        start = end + 1
    return ranges


def write_judge_prompt(
    *,
    judge_dir: Path,
    run_root: Path,
    run_id: str,
    run_label: str,
    state_db: Path | None,
    event_source: str,
    packets: list[dict[str, Any]],
) -> None:
    pending = sorted(packet["task_id"] for packet in packets if packet.get("runner_state") == "pending")
    failed = sorted(
        packet["task_id"]
        for packet in packets
        if packet.get("ok") is False and packet.get("runner_state") not in ("pending", "passed")
    )
    text = RUBRIC.format(
        run_id=run_id,
        run_label=run_label,
        run_root=run_root,
        judge_dir=judge_dir,
        state_db=state_db if state_db is not None else "(none)",
        event_source=event_source,
        pending_tasks=", ".join(pending) if pending else "(none)",
        failed_tasks=", ".join(failed) if failed else "(none)",
    )
    (judge_dir / "judge_prompt.md").write_text(text)


def write_chunk_briefs(judge_dir: Path, ranges: list[tuple[int, int]]) -> None:
    for start, end in ranges:
        brief = f"""# Judge packet chunk {start:03d}-{end:03d}

Read `judge_prompt.md` first and follow it exactly.

Judge every packet in `packets_{start:03d}_{end:03d}.json`.

Write only a JSON array to `chunk_{start:03d}_{end:03d}.json`.
Each item must contain `task_id`, `runner_ok`, `verdict`, `score`, `reasoning`,
`evidence_checked`, and `failure_class`. Scores must be 0 or 1.
"""
        (judge_dir / f"judge_brief_{start:03d}_{end:03d}.md").write_text(brief)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-root", required=True, type=Path)
    parser.add_argument("--run-id")
    parser.add_argument("--packets", type=Path)
    parser.add_argument("--state-db", type=Path)
    parser.add_argument("--out-dir", type=Path)
    parser.add_argument("--run-label")
    parser.add_argument("--expected-total", type=int, default=106)
    parser.add_argument("--chunk-size", type=int, default=22)
    args = parser.parse_args()

    run_root = args.run_root.resolve()
    run_id = args.run_id or run_root.name
    packets_path = (args.packets or (run_root / "judge_packets.json")).resolve()
    state_db = (args.state_db or (run_root / "state" / "state.db")).resolve()
    judge_dir = (args.out_dir or (run_root / "judge")).resolve()
    run_label = args.run_label or run_id

    packet_data = read_json(packets_path)
    if not isinstance(packet_data, list):
        raise SystemExit(f"{packets_path}: expected packet array")
    packets = [dict(packet) for packet in packet_data]
    validate_packets(packets, args.expected_total)

    judge_dir.mkdir(parents=True, exist_ok=True)
    event_dir = judge_dir / "native-events"
    sqlite_available = state_db.is_file()
    sessions_by_task: dict[str, list[dict[str, Any]]] = {}
    conn: sqlite3.Connection | None = None
    if sqlite_available:
        conn = sqlite3.connect(state_db)
        conn.row_factory = sqlite3.Row
        sessions_by_task = query_task_sessions(conn)

    enriched = []
    try:
        for packet in packets:
            task_id = str(packet["task_id"])
            event_log = event_dir / f"task-{safe_task_filename(task_id)}-events.jsonl"
            if conn is not None:
                sessions = sessions_by_task.get(task_id, [])
                export_event_log(conn, [session["id"] for session in sessions], event_log)
            else:
                source_event_log = packet_event_log_source(run_root, packet, task_id)
                copy_packet_event_log(source_event_log, event_log)
                sessions = packet_sessions_from_event_log(
                    packet=packet,
                    task_id=task_id,
                    event_log=source_event_log,
                )

            cwd = Path(packet["cwd"]) if packet.get("cwd") else None
            artifact_root = Path(packet["artifact_root"]) if packet.get("artifact_root") else None
            artifact_files = scan_files(cwd, "cwd") + scan_files(artifact_root, "artifact_root")

            runner_state = packet.get("runner_state")
            if not runner_state:
                runner_state = "passed" if packet.get("ok") else "failed"

            enriched_packet = {
                **packet,
                "runner_state": runner_state,
                "event_log": str(event_log),
                "native_sqlite": str(state_db) if sqlite_available else None,
                "native_event_source": "sqlite" if sqlite_available else "packet_event_log",
                "native_sessions": sessions,
                "codex_session_files": packet.get("codex_session_files") or [],
                "artifact_files": artifact_files,
            }
            enriched.append(enriched_packet)
    finally:
        if conn is not None:
            conn.close()

    (judge_dir / "packets_all.json").write_text(json.dumps(enriched, indent=2, ensure_ascii=False) + "\n")
    ranges = chunk_ranges(len(enriched), args.chunk_size)
    for start, end in ranges:
        chunk = enriched[start - 1 : end]
        (judge_dir / f"packets_{start:03d}_{end:03d}.json").write_text(
            json.dumps(chunk, indent=2, ensure_ascii=False) + "\n"
        )
    write_judge_prompt(
        judge_dir=judge_dir,
        run_root=run_root,
        run_id=run_id,
        run_label=run_label,
        state_db=state_db if sqlite_available else None,
        event_source=(
            f"SQLite `{state_db}`"
            if sqlite_available
            else "packet `event_log` JSONL files copied into `native-events/`"
        ),
        packets=enriched,
    )
    write_chunk_briefs(judge_dir, ranges)

    print(
        json.dumps(
            {
                "judge_dir": str(judge_dir),
                "packets": len(enriched),
                "chunks": [f"{start:03d}_{end:03d}" for start, end in ranges],
                "event_logs": len(list(event_dir.glob("task-*-events.jsonl"))),
                "missing_native_sessions": sorted(
                    packet["task_id"] for packet in enriched if not packet["native_sessions"]
                ),
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

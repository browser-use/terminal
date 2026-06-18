#!/usr/bin/env python3
"""Audit whether an Internal_Bench_hard run root is complete enough to trust.

This is a mechanical artifact check. It does not judge task correctness; it
verifies that the runner, packet prep, locked judge chunks, aggregate, and
current-vs-reference comparison all exist and have matching task counts.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        raise ValueError(f"missing file: {path}") from None
    except json.JSONDecodeError as exc:
        raise ValueError(f"invalid JSON in {path}: {exc}") from None


def task_ids_from_array(path: Path, data: Any) -> list[str]:
    if not isinstance(data, list):
        raise ValueError(f"{path}: expected JSON array")
    ids: list[str] = []
    for index, item in enumerate(data):
        if not isinstance(item, dict):
            raise ValueError(f"{path}: item {index} is not an object")
        task_id = item.get("task_id")
        if not isinstance(task_id, str) or not task_id:
            raise ValueError(f"{path}: item {index} has no task_id")
        ids.append(task_id)
    return ids


def duplicate_ids(ids: list[str]) -> list[str]:
    seen: set[str] = set()
    duplicates: set[str] = set()
    for task_id in ids:
        if task_id in seen:
            duplicates.add(task_id)
        seen.add(task_id)
    return sorted(duplicates)


def add_count_check(
    problems: list[str],
    label: str,
    count: int,
    expected_total: int,
) -> None:
    if count != expected_total:
        problems.append(f"{label} count {count} != expected {expected_total}")


def audit_run(
    *,
    run_root: Path,
    run_id: str,
    expected_total: int,
    require_judged: bool,
) -> dict[str, Any]:
    problems: list[str] = []
    state_dir = run_root / "state"
    manifest = state_dir / "dataset-runs" / f"{run_id}.json"
    packets_path = run_root / "judge_packets.json"
    judge_dir = run_root / "judge"
    packets_all_path = judge_dir / "packets_all.json"
    aggregate_path = judge_dir / "judge_aggregate.json"
    comparison_path = run_root / "current-vs-raw-judged-delta.md"

    summary: dict[str, Any] = {
        "run_root": str(run_root),
        "run_id": run_id,
        "expected_total": expected_total,
        "require_judged": require_judged,
        "ok": False,
        "problems": problems,
    }

    try:
        manifest_json = read_json(manifest)
    except ValueError as exc:
        problems.append(str(exc))
        manifest_json = None
    if isinstance(manifest_json, dict):
        selection = manifest_json.get("selection")
        sessions = manifest_json.get("sessions")
        if isinstance(selection, list):
            summary["manifest_selection"] = len(selection)
            add_count_check(problems, "manifest selection", len(selection), expected_total)
        else:
            problems.append(f"{manifest}: .selection is not an array")
        if isinstance(sessions, list):
            task_ids = [
                str(session.get("task_id"))
                for session in sessions
                if isinstance(session, dict) and session.get("task_id")
            ]
            summary["manifest_sessions"] = len(sessions)
            summary["manifest_unique_session_tasks"] = len(set(task_ids))
            add_count_check(problems, "manifest unique session task", len(set(task_ids)), expected_total)
        else:
            problems.append(f"{manifest}: .sessions is not an array")

    try:
        packets = read_json(packets_path)
        packet_ids = task_ids_from_array(packets_path, packets)
    except ValueError as exc:
        problems.append(str(exc))
        packet_ids = []
    summary["judge_packets"] = len(packet_ids)
    summary["judge_packet_unique_tasks"] = len(set(packet_ids))
    add_count_check(problems, "judge_packets", len(packet_ids), expected_total)
    if duplicates := duplicate_ids(packet_ids):
        problems.append(f"duplicate judge packet task ids: {', '.join(duplicates)}")

    if not judge_dir.is_dir():
        problems.append(f"judge dir missing: {judge_dir}")
        packet_chunk_paths: list[Path] = []
        chunk_paths: list[Path] = []
    else:
        packet_chunk_paths = sorted(
            path for path in judge_dir.glob("packets_*.json") if path.name != "packets_all.json"
        )
        chunk_paths = sorted(judge_dir.glob("chunk_*.json"))

    try:
        packets_all = read_json(packets_all_path)
        packets_all_ids = task_ids_from_array(packets_all_path, packets_all)
    except ValueError as exc:
        problems.append(str(exc))
        packets_all_ids = []
    summary["packets_all"] = len(packets_all_ids)
    add_count_check(problems, "packets_all", len(packets_all_ids), expected_total)

    packet_chunk_total = 0
    packet_chunk_labels: list[str] = []
    for path in packet_chunk_paths:
        try:
            ids = task_ids_from_array(path, read_json(path))
        except ValueError as exc:
            problems.append(str(exc))
            continue
        packet_chunk_total += len(ids)
        packet_chunk_labels.append(path.stem.removeprefix("packets_"))
    summary["packet_chunks"] = len(packet_chunk_paths)
    summary["packet_chunk_total"] = packet_chunk_total
    add_count_check(problems, "packet chunks total", packet_chunk_total, expected_total)

    event_logs = sorted((judge_dir / "native-events").glob("task-*-events.jsonl"))
    summary["native_event_logs"] = len(event_logs)
    add_count_check(problems, "native event logs", len(event_logs), expected_total)
    empty_event_logs = [str(path) for path in event_logs if path.stat().st_size == 0]
    if empty_event_logs:
        problems.append(f"empty native event logs: {', '.join(empty_event_logs[:5])}")

    if require_judged:
        chunk_total = 0
        chunk_labels: list[str] = []
        chunk_task_ids: list[str] = []
        for path in chunk_paths:
            try:
                ids = task_ids_from_array(path, read_json(path))
            except ValueError as exc:
                problems.append(str(exc))
                continue
            chunk_total += len(ids)
            chunk_task_ids.extend(ids)
            chunk_labels.append(path.stem.removeprefix("chunk_"))
        summary["judge_chunks"] = len(chunk_paths)
        summary["judge_chunk_total"] = chunk_total
        add_count_check(problems, "judge chunks total", chunk_total, expected_total)
        if packet_chunk_labels and chunk_labels != packet_chunk_labels:
            problems.append(
                "judge chunk labels do not match packet chunk labels: "
                f"chunks={chunk_labels} packets={packet_chunk_labels}"
            )
        if set(chunk_task_ids) != set(packet_ids):
            problems.append("judge chunk task ids do not match judge packet task ids")

        try:
            aggregate = read_json(aggregate_path)
        except ValueError as exc:
            problems.append(str(exc))
            aggregate = None
        if isinstance(aggregate, dict):
            summary["aggregate_total"] = aggregate.get("total")
            summary["aggregate_expected_total"] = aggregate.get("expected_total")
            summary["aggregate_passed"] = aggregate.get("passed")
            summary["aggregate_failed"] = aggregate.get("failed")
            if aggregate.get("total") != expected_total:
                problems.append(f"aggregate total {aggregate.get('total')} != expected {expected_total}")
            if aggregate.get("expected_total") != expected_total:
                problems.append(
                    f"aggregate expected_total {aggregate.get('expected_total')} != expected {expected_total}"
                )
            if aggregate.get("problems"):
                problems.append(f"aggregate contains problems: {aggregate.get('problems')}")
            results = aggregate.get("results")
            if not isinstance(results, list):
                problems.append(f"{aggregate_path}: .results is not an array")
            else:
                aggregate_ids = task_ids_from_array(aggregate_path, results)
                if set(aggregate_ids) != set(packet_ids):
                    problems.append("aggregate task ids do not match judge packet task ids")

        if not comparison_path.is_file():
            problems.append(f"comparison missing: {comparison_path}")
        elif comparison_path.stat().st_size == 0:
            problems.append(f"comparison is empty: {comparison_path}")
        summary["comparison"] = str(comparison_path)

    summary["ok"] = not problems
    return summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-root", required=True, type=Path)
    parser.add_argument("--run-id")
    parser.add_argument("--expected-total", type=int, default=106)
    parser.add_argument("--require-judged", action="store_true")
    args = parser.parse_args()

    run_root = args.run_root.resolve()
    run_id = args.run_id or run_root.name
    summary = audit_run(
        run_root=run_root,
        run_id=run_id,
        expected_total=args.expected_total,
        require_judged=args.require_judged,
    )
    print(json.dumps(summary, indent=2, ensure_ascii=False))
    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())

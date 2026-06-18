#!/usr/bin/env python3
"""Aggregate Internal_Bench_hard judge chunk JSON files.

Expected judge directory shape:

  packets_001_022.json
  chunk_001_022.json
  ...
  packets_all.json

Each chunk file must be a JSON array of strict judge rows with:
task_id, runner_ok, verdict, score, reasoning, evidence_checked, failure_class.
The script writes judge_aggregate.json and judge_summary.md in the judge dir.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


REQUIRED_FIELDS = {
    "task_id",
    "runner_ok",
    "verdict",
    "score",
    "reasoning",
    "evidence_checked",
    "failure_class",
}


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        raise SystemExit(f"missing file: {path}") from None
    except json.JSONDecodeError as exc:
        raise SystemExit(f"invalid JSON in {path}: {exc}") from None


def expected_task_ids(judge_dir: Path) -> list[str]:
    task_ids: list[str] = []
    packet_files = sorted(path for path in judge_dir.glob("packets_*.json") if path.name != "packets_all.json")
    for path in packet_files:
        packets = read_json(path)
        if not isinstance(packets, list):
            raise SystemExit(f"{path}: expected packet array")
        for index, packet in enumerate(packets):
            if not isinstance(packet, dict) or not packet.get("task_id"):
                raise SystemExit(f"{path}: packet {index} has no task_id")
            task_ids.append(str(packet["task_id"]))
    return task_ids


def read_judge_rows(judge_dir: Path) -> tuple[list[dict[str, Any]], list[str]]:
    rows: list[dict[str, Any]] = []
    problems: list[str] = []
    chunk_files = sorted(judge_dir.glob("chunk_*.json"))
    if not chunk_files:
        problems.append("no chunk_*.json files found")
    for path in chunk_files:
        data = read_json(path)
        if not isinstance(data, list):
            problems.append(f"{path.name}: not a JSON array")
            continue
        for index, row in enumerate(data):
            if not isinstance(row, dict):
                problems.append(f"{path.name}[{index}]: not a JSON object")
                continue
            rows.append(row)
    return rows, problems


def build_aggregate(
    *,
    rows: list[dict[str, Any]],
    expected_ids: list[str],
    initial_problems: list[str],
    run_root: str | None,
    run_id: str | None,
    dataset: str | None,
) -> dict[str, Any]:
    ids = [str(row.get("task_id")) for row in rows]
    id_counts = Counter(ids)
    expected_counts = Counter(expected_ids)

    missing = [task_id for task_id in expected_ids if task_id not in id_counts]
    unexpected = sorted(task_id for task_id in id_counts if task_id not in expected_counts)
    duplicates = sorted(task_id for task_id, count in id_counts.items() if count > 1)
    expected_duplicates = sorted(task_id for task_id, count in expected_counts.items() if count > 1)
    non_binary = [str(row.get("task_id")) for row in rows if row.get("score") not in (0, 1)]

    missing_fields = []
    for row in rows:
        missing_for_row = sorted(REQUIRED_FIELDS - set(row))
        if missing_for_row:
            missing_fields.append({"task_id": row.get("task_id"), "missing": missing_for_row})

    failed = sorted(str(row["task_id"]) for row in rows if row.get("score") == 0)
    passed = sorted(str(row["task_id"]) for row in rows if row.get("score") == 1)

    classes: dict[str, list[str]] = defaultdict(list)
    for row in rows:
        if row.get("score") == 0:
            classes[str(row.get("failure_class") or "unknown")].append(str(row.get("task_id")))

    order = {task_id: index for index, task_id in enumerate(expected_ids)}
    ordered_rows = sorted(rows, key=lambda row: order.get(str(row.get("task_id")), 999999))
    expected_total = len(expected_ids)

    aggregate: dict[str, Any] = {
        "total": len(rows),
        "expected_total": expected_total,
        "passed": len(passed),
        "failed": len(failed),
        "score": len(passed) / expected_total if expected_total else 0,
        "failed_ids": failed,
        "passed_ids": passed,
        "missing_ids": missing,
        "unexpected_ids": unexpected,
        "duplicate_ids": duplicates,
        "duplicate_expected_ids": expected_duplicates,
        "non_binary_scores": non_binary,
        "missing_fields": missing_fields,
        "problems": initial_problems,
        "failure_classes": {key: sorted(value) for key, value in sorted(classes.items())},
        "results": ordered_rows,
    }
    if run_root:
        aggregate["run_root"] = run_root
    if run_id:
        aggregate["run_id"] = run_id
    if dataset:
        aggregate["dataset"] = dataset
    return aggregate


def write_outputs(judge_dir: Path, aggregate: dict[str, Any]) -> None:
    (judge_dir / "judge_aggregate.json").write_text(json.dumps(aggregate, indent=2, ensure_ascii=False) + "\n")

    failed = aggregate["failed_ids"]
    missing = aggregate["missing_ids"]
    duplicates = aggregate["duplicate_ids"]
    problems = aggregate["problems"]
    summary = [
        f"Score: {aggregate['passed']}/{aggregate['expected_total']} ({aggregate['score']:.1%})",
        "Failed ids: " + (", ".join(failed) if failed else "(none)"),
        "Missing ids: " + (", ".join(missing) if missing else "(none)"),
        "Duplicate ids: " + (", ".join(duplicates) if duplicates else "(none)"),
        "Problems: " + ("; ".join(problems) if problems else "(none)"),
        "",
        "Failure classes:",
    ]
    if aggregate["failure_classes"]:
        for failure_class, task_ids in aggregate["failure_classes"].items():
            summary.append(f"- {failure_class}: {len(task_ids)} ({', '.join(task_ids)})")
    else:
        summary.append("- none: 0")
    (judge_dir / "judge_summary.md").write_text("\n".join(summary) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("judge_dir", type=Path)
    parser.add_argument("--expected-total", type=int, default=106)
    parser.add_argument("--run-root")
    parser.add_argument("--run-id")
    parser.add_argument("--dataset", default="Internal_Bench_hard")
    parser.add_argument("--allow-problems", action="store_true")
    args = parser.parse_args()

    judge_dir = args.judge_dir.resolve()
    if not judge_dir.is_dir():
        raise SystemExit(f"judge dir not found: {judge_dir}")

    expected_ids = expected_task_ids(judge_dir)
    if len(expected_ids) != args.expected_total:
        raise SystemExit(f"expected {args.expected_total} packet ids, found {len(expected_ids)}")

    rows, problems = read_judge_rows(judge_dir)
    aggregate = build_aggregate(
        rows=rows,
        expected_ids=expected_ids,
        initial_problems=problems,
        run_root=args.run_root,
        run_id=args.run_id,
        dataset=args.dataset,
    )
    write_outputs(judge_dir, aggregate)

    validation_errors = []
    for key in ("missing_ids", "unexpected_ids", "duplicate_ids", "duplicate_expected_ids", "non_binary_scores", "missing_fields", "problems"):
        if aggregate[key]:
            validation_errors.append(f"{key}={aggregate[key]}")
    if aggregate["total"] != aggregate["expected_total"]:
        validation_errors.append(f"total={aggregate['total']} expected_total={aggregate['expected_total']}")

    print(
        json.dumps(
            {
                "total": aggregate["total"],
                "expected_total": aggregate["expected_total"],
                "passed": aggregate["passed"],
                "failed": aggregate["failed"],
                "score": aggregate["score"],
                "failed_ids": aggregate["failed_ids"],
                "validation_errors": validation_errors,
            },
            indent=2,
        )
    )

    if validation_errors and not args.allow_problems:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Compare two strict judge aggregate files task by task.

This is intentionally offline and artifact-format-light. It consumes the
`judge_aggregate.json` files produced by the Internal_Bench_hard locked judge
and emits a markdown delta report that can be reviewed next to the raw Codex +
browser-harness reference run.
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
        raise SystemExit(f"missing file: {path}") from None
    except json.JSONDecodeError as exc:
        raise SystemExit(f"invalid JSON in {path}: {exc}") from None


def is_pass(result: dict[str, Any]) -> bool:
    return int(result.get("score") or 0) == 1


def normalize_results(path: Path, expected_total: int | None) -> tuple[dict[str, Any], dict[str, dict[str, Any]]]:
    aggregate = read_json(path)
    if not isinstance(aggregate, dict):
        raise SystemExit(f"{path}: expected object aggregate")
    results = aggregate.get("results")
    if not isinstance(results, list):
        raise SystemExit(f"{path}: expected .results array")

    by_task: dict[str, dict[str, Any]] = {}
    duplicates: list[str] = []
    for index, item in enumerate(results):
        if not isinstance(item, dict):
            raise SystemExit(f"{path}: .results[{index}] is not an object")
        task_id = item.get("task_id")
        if not isinstance(task_id, str) or not task_id:
            raise SystemExit(f"{path}: .results[{index}] has no task_id")
        if task_id in by_task:
            duplicates.append(task_id)
        by_task[task_id] = item

    if duplicates:
        duplicate_list = ", ".join(sorted(set(duplicates)))
        raise SystemExit(f"{path}: duplicate task ids: {duplicate_list}")

    if expected_total is not None and len(by_task) != expected_total:
        raise SystemExit(f"{path}: expected {expected_total} tasks, found {len(by_task)}")

    declared_total = aggregate.get("total")
    if isinstance(declared_total, int) and declared_total != len(by_task):
        raise SystemExit(f"{path}: .total={declared_total} but .results has {len(by_task)} tasks")

    return aggregate, by_task


def cell(value: Any, *, limit: int | None = None) -> str:
    text = "" if value is None else str(value)
    text = " ".join(text.replace("\r", " ").replace("\n", " ").split())
    if limit is not None and len(text) > limit:
        text = text[: max(0, limit - 3)].rstrip() + "..."
    return text.replace("|", "\\|")


def score_text(aggregate: dict[str, Any], by_task: dict[str, dict[str, Any]]) -> str:
    passed = sum(1 for result in by_task.values() if is_pass(result))
    total = len(by_task)
    return f"{passed}/{total} ({passed / total * 100:.1f}%)"


def row_for(
    task_id: str,
    current: dict[str, Any] | None,
    reference: dict[str, Any] | None,
    *,
    include_reason: bool,
) -> str:
    current_score = "missing" if current is None else ("1" if is_pass(current) else "0")
    reference_score = "missing" if reference is None else ("1" if is_pass(reference) else "0")
    current_class = "" if current is None else cell(current.get("failure_class") or "none", limit=36)
    reference_class = "" if reference is None else cell(reference.get("failure_class") or "none", limit=36)
    if not include_reason:
        return f"| `{task_id}` | {current_score} | {reference_score} | {current_class} | {reference_class} |"
    current_reason = "" if current is None else cell(current.get("reasoning") or "", limit=140)
    reference_reason = "" if reference is None else cell(reference.get("reasoning") or "", limit=140)
    return (
        f"| `{task_id}` | {current_score} | {reference_score} | {current_class} | "
        f"{reference_class} | {current_reason} | {reference_reason} |"
    )


def build_report(
    *,
    current_path: Path,
    reference_path: Path,
    current_label: str,
    reference_label: str,
    current_aggregate: dict[str, Any],
    reference_aggregate: dict[str, Any],
    current: dict[str, dict[str, Any]],
    reference: dict[str, dict[str, Any]],
) -> str:
    all_ids = sorted(set(current) | set(reference))
    missing_current = [task_id for task_id in all_ids if task_id not in current]
    missing_reference = [task_id for task_id in all_ids if task_id not in reference]
    regressions = [
        task_id
        for task_id in all_ids
        if task_id in current and task_id in reference and not is_pass(current[task_id]) and is_pass(reference[task_id])
    ]
    improvements = [
        task_id
        for task_id in all_ids
        if task_id in current and task_id in reference and is_pass(current[task_id]) and not is_pass(reference[task_id])
    ]
    both_fail = [
        task_id
        for task_id in all_ids
        if task_id in current
        and task_id in reference
        and not is_pass(current[task_id])
        and not is_pass(reference[task_id])
    ]
    both_pass = [
        task_id
        for task_id in all_ids
        if task_id in current and task_id in reference and is_pass(current[task_id]) and is_pass(reference[task_id])
    ]

    lines = [
        "# Judged Run Comparison",
        "",
        "## Inputs",
        "",
        f"- Current: `{current_label}`",
        f"- Current aggregate: `{current_path}`",
        f"- Reference: `{reference_label}`",
        f"- Reference aggregate: `{reference_path}`",
        "",
        "## Summary",
        "",
        "| Metric | Value |",
        "| --- | ---: |",
        f"| Current strict score | {score_text(current_aggregate, current)} |",
        f"| Reference strict score | {score_text(reference_aggregate, reference)} |",
        f"| Current tasks | {len(current)} |",
        f"| Reference tasks | {len(reference)} |",
        f"| Both pass | {len(both_pass)} |",
        f"| Both fail | {len(both_fail)} |",
        f"| Current-only regressions | {len(regressions)} |",
        f"| Current-only improvements | {len(improvements)} |",
        f"| Missing in current | {len(missing_current)} |",
        f"| Missing in reference | {len(missing_reference)} |",
        "",
        "## Regressions Vs Reference",
        "",
    ]

    if regressions:
        lines.extend(
            [
                "| Task | Current | Reference | Current class | Reference class | Current reason | Reference reason |",
                "| --- | ---: | ---: | --- | --- | --- | --- |",
            ]
        )
        lines.extend(row_for(task_id, current.get(task_id), reference.get(task_id), include_reason=True) for task_id in regressions)
    else:
        lines.append("(none)")

    lines.extend(["", "## Improvements Vs Reference", ""])
    if improvements:
        lines.extend(
            [
                "| Task | Current | Reference | Current class | Reference class | Current reason | Reference reason |",
                "| --- | ---: | ---: | --- | --- | --- | --- |",
            ]
        )
        lines.extend(row_for(task_id, current.get(task_id), reference.get(task_id), include_reason=True) for task_id in improvements)
    else:
        lines.append("(none)")

    lines.extend(["", "## Shared Failures", ""])
    if both_fail:
        lines.extend(
            [
                "| Task | Current | Reference | Current class | Reference class | Current reason | Reference reason |",
                "| --- | ---: | ---: | --- | --- | --- | --- |",
            ]
        )
        lines.extend(row_for(task_id, current.get(task_id), reference.get(task_id), include_reason=True) for task_id in both_fail)
    else:
        lines.append("(none)")

    if missing_current or missing_reference:
        lines.extend(["", "## Missing Task IDs", ""])
        lines.append(f"- Missing in current: {', '.join(missing_current) if missing_current else '(none)'}")
        lines.append(f"- Missing in reference: {', '.join(missing_reference) if missing_reference else '(none)'}")

    lines.extend(
        [
            "",
            "## Full Task Matrix",
            "",
            "| Task | Current | Reference | Current class | Reference class |",
            "| --- | ---: | ---: | --- | --- |",
        ]
    )
    lines.extend(row_for(task_id, current.get(task_id), reference.get(task_id), include_reason=False) for task_id in all_ids)
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--current-aggregate", required=True, type=Path)
    parser.add_argument("--reference-aggregate", required=True, type=Path)
    parser.add_argument("--current-label", default="current")
    parser.add_argument("--reference-label", default="reference")
    parser.add_argument("--expected-total", type=int, default=106)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    current_aggregate, current = normalize_results(args.current_aggregate, args.expected_total)
    reference_aggregate, reference = normalize_results(args.reference_aggregate, args.expected_total)
    report = build_report(
        current_path=args.current_aggregate,
        reference_path=args.reference_aggregate,
        current_label=args.current_label,
        reference_label=args.reference_label,
        current_aggregate=current_aggregate,
        reference_aggregate=reference_aggregate,
        current=current,
        reference=reference,
    )

    if args.out is None:
        print(report)
    else:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(report)
        print(args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())

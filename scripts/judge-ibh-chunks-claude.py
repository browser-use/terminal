#!/usr/bin/env python3
"""Run locked Internal_Bench_hard judge chunks with Claude Code.

`prepare-ibh-judge.py` creates `packets_*.json`, `judge_prompt.md`, and
per-task native event logs. This script runs one Claude Code print-mode judge
per packet chunk, validates the returned strict JSON rows, and writes
`chunk_*.json` files for `aggregate-ibh-judgments.py`.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import subprocess
import sys
import tempfile
from dataclasses import dataclass
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

JSON_SCHEMA = {
    "type": "array",
    "items": {
        "type": "object",
        "additionalProperties": True,
        "required": sorted(REQUIRED_FIELDS),
        "properties": {
            "task_id": {"type": "string"},
            "runner_ok": {"type": "boolean"},
            "verdict": {"type": "string"},
            "score": {"type": "integer", "enum": [0, 1]},
            "reasoning": {"type": "string"},
            "evidence_checked": {"type": "string"},
            "failure_class": {"type": "string"},
        },
    },
}


@dataclass(frozen=True)
class JudgeChunk:
    packet_path: Path
    output_path: Path
    brief_path: Path
    label: str


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        raise SystemExit(f"missing file: {path}") from None
    except json.JSONDecodeError as exc:
        raise SystemExit(f"invalid JSON in {path}: {exc}") from None


def discover_chunks(judge_dir: Path) -> list[JudgeChunk]:
    packet_files = sorted(path for path in judge_dir.glob("packets_*.json") if path.name != "packets_all.json")
    chunks: list[JudgeChunk] = []
    for packet_path in packet_files:
        label = packet_path.stem.removeprefix("packets_")
        chunks.append(
            JudgeChunk(
                packet_path=packet_path,
                output_path=judge_dir / f"chunk_{label}.json",
                brief_path=judge_dir / f"judge_brief_{label}.md",
                label=label,
            )
        )
    return chunks


def extract_json_array(text: str) -> list[Any]:
    stripped = text.strip()
    if stripped.startswith("```"):
        lines = stripped.splitlines()
        if lines and lines[0].startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].startswith("```"):
            lines = lines[:-1]
        stripped = "\n".join(lines).strip()

    decoder = json.JSONDecoder()
    for index, char in enumerate(stripped):
        if char != "[":
            continue
        try:
            value, end = decoder.raw_decode(stripped[index:])
        except json.JSONDecodeError:
            continue
        trailing = stripped[index + end :].strip()
        if trailing:
            continue
        if not isinstance(value, list):
            raise ValueError("judge output JSON is not an array")
        return value
    raise ValueError("could not find a valid JSON array in judge output")


def validate_rows(rows: list[Any], packet_path: Path) -> list[dict[str, Any]]:
    packets = read_json(packet_path)
    if not isinstance(packets, list):
        raise ValueError(f"{packet_path}: packet file is not an array")
    expected_ids = [str(packet.get("task_id")) for packet in packets if isinstance(packet, dict)]
    expected = set(expected_ids)
    if len(expected) != len(expected_ids):
        raise ValueError(f"{packet_path}: duplicate task ids in packet chunk")

    normalized: list[dict[str, Any]] = []
    seen: set[str] = set()
    for index, row in enumerate(rows):
        if not isinstance(row, dict):
            raise ValueError(f"judge row {index} is not an object")
        missing = sorted(REQUIRED_FIELDS - set(row))
        if missing:
            raise ValueError(f"judge row {index} missing fields: {', '.join(missing)}")
        task_id = str(row["task_id"])
        if task_id not in expected:
            raise ValueError(f"unexpected judged task id {task_id}")
        if task_id in seen:
            raise ValueError(f"duplicate judged task id {task_id}")
        if row.get("score") not in (0, 1):
            raise ValueError(f"task {task_id}: score must be 0 or 1")
        if not isinstance(row.get("runner_ok"), bool):
            raise ValueError(f"task {task_id}: runner_ok must be boolean")
        seen.add(task_id)
        normalized.append(row)

    missing_ids = sorted(expected - seen)
    if missing_ids:
        raise ValueError(f"missing judged task ids: {', '.join(missing_ids)}")
    return sorted(normalized, key=lambda row: expected_ids.index(str(row["task_id"])))


def build_prompt(judge_dir: Path, chunk: JudgeChunk) -> str:
    return f"""You are a locked strict Internal_Bench_hard judge.

Follow this rubric exactly:
{judge_dir / "judge_prompt.md"}

Read this chunk brief:
{chunk.brief_path}

Judge every packet in this file:
{chunk.packet_path}

Use only the saved artifacts and native event logs referenced by the packets.
Do not browse the web, do not refetch live pages, and do not infer support from
the final answer alone. Verify claimed prices, IDs, counts, names, URLs, and
records against saved files or native event logs.

Return ONLY a JSON array. Do not wrap it in markdown and do not write prose
outside the array. Each object must contain:
task_id, runner_ok, verdict, score, reasoning, evidence_checked, failure_class.
Scores must be integer 0 or 1.
"""


def run_chunk(
    *,
    chunk: JudgeChunk,
    judge_dir: Path,
    run_root: Path,
    claude_bin: str,
    model: str,
    timeout_seconds: int,
    overwrite: bool,
    dry_run: bool,
) -> dict[str, Any]:
    if chunk.output_path.exists() and not overwrite:
        rows = validate_rows(read_json(chunk.output_path), chunk.packet_path)
        return {"chunk": chunk.label, "status": "skipped", "rows": len(rows), "path": str(chunk.output_path)}

    prompt = build_prompt(judge_dir, chunk)
    command = [
        claude_bin,
        "-p",
        "--model",
        model,
        "--permission-mode",
        "bypassPermissions",
        "--no-session-persistence",
        "--output-format",
        "text",
        "--json-schema",
        json.dumps(JSON_SCHEMA, separators=(",", ":")),
        "--add-dir",
        str(judge_dir),
        "--add-dir",
        str(run_root),
        prompt,
    ]

    if dry_run:
        return {
            "chunk": chunk.label,
            "status": "dry-run",
            "rows": len(read_json(chunk.packet_path)),
            "path": str(chunk.output_path),
            "command": command[:8] + ["...", prompt[:240] + "..."],
        }

    raw_path = chunk.output_path.with_suffix(".raw.txt")
    env = os.environ.copy()
    env.setdefault("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
    proc = subprocess.run(
        command,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout_seconds,
        env=env,
    )
    raw_path.write_text(proc.stdout)
    if proc.stderr:
        chunk.output_path.with_suffix(".stderr.txt").write_text(proc.stderr)
    if proc.returncode != 0:
        raise RuntimeError(f"Claude judge {chunk.label} exited {proc.returncode}; raw={raw_path}")

    rows = validate_rows(extract_json_array(proc.stdout), chunk.packet_path)
    chunk.output_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=chunk.output_path.parent, delete=False) as handle:
        json.dump(rows, handle, indent=2, ensure_ascii=False)
        handle.write("\n")
        tmp_name = handle.name
    Path(tmp_name).replace(chunk.output_path)
    return {"chunk": chunk.label, "status": "judged", "rows": len(rows), "path": str(chunk.output_path)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--judge-dir", required=True, type=Path)
    parser.add_argument("--run-root", type=Path)
    parser.add_argument("--claude-bin", default=os.environ.get("JUDGE_CLAUDE_BIN", "claude"))
    parser.add_argument("--model", default=os.environ.get("JUDGE_MODEL", "sonnet"))
    parser.add_argument("--concurrency", type=int, default=int(os.environ.get("JUDGE_CONCURRENCY", "5")))
    parser.add_argument("--timeout-seconds", type=int, default=int(os.environ.get("JUDGE_TIMEOUT_SECONDS", "3600")))
    parser.add_argument("--overwrite", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    judge_dir = args.judge_dir.resolve()
    if not judge_dir.is_dir():
        raise SystemExit(f"judge dir not found: {judge_dir}")
    if not (judge_dir / "judge_prompt.md").is_file():
        raise SystemExit(f"judge prompt not found: {judge_dir / 'judge_prompt.md'}")

    run_root = (args.run_root or judge_dir.parent).resolve()
    if not run_root.is_dir():
        raise SystemExit(f"run root not found: {run_root}")
    if args.concurrency < 1:
        raise SystemExit("--concurrency must be >= 1")

    chunks = discover_chunks(judge_dir)
    if not chunks:
        raise SystemExit(f"no packet chunks found in {judge_dir}")

    results: list[dict[str, Any]] = []
    failures: list[str] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=min(args.concurrency, len(chunks))) as executor:
        future_by_chunk = {
            executor.submit(
                run_chunk,
                chunk=chunk,
                judge_dir=judge_dir,
                run_root=run_root,
                claude_bin=args.claude_bin,
                model=args.model,
                timeout_seconds=args.timeout_seconds,
                overwrite=args.overwrite,
                dry_run=args.dry_run,
            ): chunk
            for chunk in chunks
        }
        for future in concurrent.futures.as_completed(future_by_chunk):
            chunk = future_by_chunk[future]
            try:
                result = future.result()
            except Exception as exc:  # noqa: BLE001 - report every failed chunk cleanly.
                failures.append(f"{chunk.label}: {exc}")
            else:
                results.append(result)

    print(json.dumps({"judge_dir": str(judge_dir), "results": sorted(results, key=lambda row: row["chunk"]), "failures": failures}, indent=2))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())

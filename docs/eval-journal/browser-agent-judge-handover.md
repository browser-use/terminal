# Browser Agent Eval Judging Handover

This document explains how to judge a completed browser-agent eval run end to end. It is written for a fresh model or engineer on another machine with no chat history.

The core principle:

```text
Runner pass is not correctness.
```

A runner pass usually means the agent emitted `done` or produced a captured final result. It does not mean the task was fulfilled. The benchmark score must come from judging the saved final answer, result files, screenshots, tool outputs, and event trace.

## Expected Run Layout

A completed run should look roughly like this:

```text
ROOT/
  judge_packets.json
  state/
    state.db
    dataset-runs/
      RUN_ID.json
    dataset-run-files/
      RUN_ID/
        task-<N>-attempt-1/
          cwd/
          artifacts/
  logs/
    dataset-run.log
```

Use a durable root, if available:

```text
/home/exedev/eval-runs/<run-id>
```

Do not rely only on `/tmp/<run-id>` because `/tmp` may be cleaned.

## Runner Health Check

First inspect mechanical runner completion:

```bash
jq '.summary | {run_id,dataset,count,passed,failed,pending,failed_ids,pending_ids}' \
  "$ROOT/state/dataset-runs/$RUN_ID.json"
```

Report this separately as the runner score. Do not call it the eval score.

Also inspect the packet shape:

```bash
python3 - <<'PY'
import json, sys
path = sys.argv[1]
arr = json.load(open(path))
print("packets", len(arr))
print("ok_false", [p["task_id"] for p in arr if not p.get("ok")])
print("first", {k: arr[0].get(k) for k in ["task_id", "ok", "error", "cwd", "artifact_root"]})
PY "$ROOT/judge_packets.json"
```

## Judge Packets

Each task should have a packet:

```json
{
  "task_id": "9",
  "ok": true,
  "task": "Original user task prompt",
  "final_result": "Agent final answer",
  "cwd": "/path/to/task-9-attempt-1/cwd",
  "artifact_root": "/path/to/task-9-attempt-1/artifacts",
  "error": null
}
```

If `judge_packets.json` does not exist, create it from:

- the dataset task prompt
- the runner manifest task/session mapping
- per-task `cwd` and `artifacts` directories
- final result from the manifest or events
- runner `ok` and error status

When moving runs between machines, normalize stale `/tmp/<run-id>` paths in `cwd` and `artifact_root` to the durable local root.

## Exact Judge Rubric

Use this same prompt for every run that will be compared.

```text
You are judging saved browser-agent eval results for real_v8.

Goal:
Produce a calibrated benchmark verdict, not a maximal forensic audit. Score whether the user got the requested core deliverable.

Rules:
- Score exactly 0 or 1. No partial credit.
- 1 = the core requested deliverable is present and credible.
- Minor formatting, wording, citation, or non-core field imperfections are okay.
- 0 = no answer, materially incomplete, wrong entity/scope/source, hallucinated, unsupported, or only a partial result.
- Runner pass is not correctness.
- Runner failure is 0 unless complete requested artifacts exist on disk.
- If the final prose is short but the correct saved file exists, score 1.
- If a result self-reports that the core extraction/count/list is incomplete, score 0 unless the task itself allowed partial/source-limited output.
- For exact counts, exact product matches, exact URLs, current prices, rankings, or all-record extraction, require enough saved evidence to make the result credible.
- Do not fail merely because the agent used Python, shell, HTTP, APIs, browser scripts, or did not use the visible browser. Any valid path is okay.
- Judge as of the run date. Do not penalize post-run website changes.
- Use live/source verification lightly when practical. Primary evidence is final_result, saved files, screenshots, tool outputs, and SQLite events.
- Do not over-penalize isolated missing secondary fields if the task's core user intent is still fulfilled and credible.
- Do fail missing fields, counts, source coverage, product matches, or screenshots when they materially undermine the requested core deliverable.

False-green checks that should fail:
- wrong entity type, e.g. city/category/location pages instead of requested individual listings
- wrong record, parcel, product, company, country, date range, or source
- empty/null/placeholder result presented as complete
- result file contradicts final_result or saved evidence
- missing core required fields at scale, where those fields are central to the deliverable
- source/scope drift to unrelated sites or generic sources when the task required specific sources
- padding/out-of-scope rows used to hit a quantity target
- blank/captcha/blocked screenshot when the screenshot itself is the deliverable

Failure class labels:
- runner-no-result
- provider-error
- runtime-failure
- empty-result
- wrong-record
- missing-required-fields
- source-scope-drift
- weak-evidence
- listing-vs-detail
- output-format-violation
- result-final-mismatch
- synthetic-or-unsupported
- source-limited
- task-too-broad
- site-blocked
```

## Subagent Judging

Split the 100 tasks into five chunks:

```text
001-020
021-040
041-060
061-080
081-100
```

Create five packet files:

```text
packets_001_020.json
packets_021_040.json
packets_041_060.json
packets_061_080.json
packets_081_100.json
```

Each subagent receives exactly one chunk plus the same judge prompt. Do not give one subagent all 100 tasks unless there is no alternative; it increases context pressure and makes the judge sloppier.

Subagent instruction template:

```text
Strictly judge real_v8 tasks <LO>-<HI> using the rubric in judge_prompt.md.

Read:
- <JUDGE_DIR>/judge_prompt.md
- <JUDGE_DIR>/packets_<LO>_<HI>.json

Use artifacts under:
- <ROOT>

Ignore stale /tmp paths in packet fields if a durable root exists.

Use immutable SQLite if needed:
sqlite3 'file:<ROOT>/state/state.db?mode=ro&immutable=1' "..."

For each task:
- read the task prompt
- read final_result
- inspect cwd/result.* files
- inspect artifacts/screenshots/tool outputs if needed
- use SQLite event trace if final_result/artifacts are ambiguous
- score exactly 0 or 1
- no partial credit

Runner pass is not correctness.
Runner failure is 0 unless complete requested artifacts exist on disk.
If final prose is short but a complete saved artifact exists, score 1.
If the result is materially incomplete, wrong entity/scope/source, unsupported, or only partial, score 0.

Write a JSON array of exactly <N> judgment objects to:
<JUDGE_DIR>/chunk_<LO>_<HI>.json

Final reply should only summarize:
Pass count, fail count, failed ids.
```

Expected chunk output:

```json
[
  {
    "task_id": "9",
    "runner_ok": true,
    "verdict": "FAILED",
    "score": 0,
    "reasoning": "The result returned location pages instead of individual property listings.",
    "evidence_checked": ["final_result", "cwd/result.json", "artifacts/example.png"],
    "failure_class": "listing-vs-detail"
  }
]
```

Allowed verdicts:

```text
FULFILLED
FAILED
```

Allowed scores:

```text
0
1
```

## SQLite Event Trace

Open SQLite read-only and immutable. This avoids lock/write errors when the run directory is copied or hardlinked:

```bash
sqlite3 "file:$ROOT/state/state.db?mode=ro&immutable=1" "
select type,count(*)
from events
group by type
order by count(*) desc
limit 20;"
```

Map a task id to session id:

```bash
sqlite3 "file:$ROOT/state/state.db?mode=ro&immutable=1" "
select s.id, s.status, json_extract(e.payload_json,'$.task_id') as task_id
from sessions s
join events e on e.session_id=s.id and e.type='dataset.case'
where json_extract(e.payload_json,'$.task_id')='<TASK_ID>';"
```

Inspect one task session:

```bash
sqlite3 "file:$ROOT/state/state.db?mode=ro&immutable=1" "
select id,type,substr(payload_json,1,2000)
from events
where session_id='<SESSION_ID>'
order by id;"
```

Use SQLite when:

- final result and result files disagree
- a task is `ok=false`
- a result claims it checked a page/date/entity and you need evidence
- browser/tool failures might explain an empty result
- screenshots are ambiguous
- you need to see whether `done` was emitted

Do not overuse SQLite when the saved final/result file is enough to judge.

## Aggregation

Aggregate from saved chunk JSON files, not from subagent chat summaries.

Validation requirements:

```text
100 judgments present
no missing task ids
no duplicate task ids
all scores are 0 or 1
all chunk files parse as JSON arrays
```

Minimal aggregation script:

```python
import json
from pathlib import Path
from collections import Counter

judge_dir = Path("PATH_TO_JUDGE_DIR")
chunk_files = [
    judge_dir / "chunk_001_020.json",
    judge_dir / "chunk_021_040.json",
    judge_dir / "chunk_041_060.json",
    judge_dir / "chunk_061_080.json",
    judge_dir / "chunk_081_100.json",
]

rows = []
problems = []

for path in chunk_files:
    if not path.exists():
        problems.append(f"missing {path}")
        continue
    data = json.loads(path.read_text())
    if not isinstance(data, list):
        problems.append(f"not list {path}")
        continue
    rows.extend(data)

ids = [str(r["task_id"]) for r in rows]
expected = [str(i) for i in range(1, 101)]

missing = [i for i in expected if i not in ids]
dupes = [k for k, v in Counter(ids).items() if v > 1]
non_binary = [r["task_id"] for r in rows if r.get("score") not in (0, 1)]

failed = sorted([str(r["task_id"]) for r in rows if r["score"] == 0], key=int)
passed = sorted([str(r["task_id"]) for r in rows if r["score"] == 1], key=int)

aggregate = {
    "total": len(rows),
    "passed": len(passed),
    "failed": len(failed),
    "score": len(passed) / len(rows) if rows else 0,
    "failed_ids": failed,
    "missing_ids": missing,
    "duplicate_ids": dupes,
    "non_binary_scores": non_binary,
    "problems": problems,
    "results": sorted(rows, key=lambda r: int(r["task_id"])),
}

(judge_dir / "judge_aggregate.json").write_text(json.dumps(aggregate, indent=2))

print(json.dumps({
    "score": aggregate["score"],
    "passed": aggregate["passed"],
    "failed": aggregate["failed"],
    "failed_ids": aggregate["failed_ids"],
    "problems": aggregate["problems"],
}, indent=2))
```

## Locked Or Panel Judge Comparison

If another judge produced a score, compare it after your own judgment is complete.

```python
locked_failed = {"33", "52", "68", "72", "74", "98"}
ours_failed = set(aggregate["failed_ids"])

ours_fail_locked_pass = sorted(ours_failed - locked_failed, key=int)
ours_pass_locked_fail = sorted(locked_failed - ours_failed, key=int)
```

Report:

```text
locked score
ours score
ours failed but locked passed
ours passed but locked failed
```

Do not use the locked judge fail list to bias the subagents before they judge. It is calibration data, not ground truth.

## Head-To-Head Run Comparison

Only compare runs through the same prompt and same aggregation code.

```python
old_failed = set(old_aggregate["failed_ids"])
new_failed = set(new_aggregate["failed_ids"])

fixed = sorted(old_failed - new_failed, key=int)
regressed = sorted(new_failed - old_failed, key=int)
failed_both = sorted(old_failed & new_failed, key=int)
delta = new_aggregate["score"] - old_aggregate["score"]
```

Report:

```text
old score
new score
delta
fixed tasks
regressed tasks
failed in both
```

Do not call a branch better because runner score improved. Call it better only if the same judge prompt and aggregation show a better correctness score.

## Final Report Shape

Use this concise format:

```text
Runner:
- passed/failed/pending
- ok=false ids

Judge:
- score
- failed ids
- validation: missing ids, duplicate ids, parse problems

Locked comparison:
- locked score
- disagreements

Failure summary:
- grouped by failure_class
- one-line explanation for each failed task

Artifacts:
- judge_aggregate.json
- judge_summary.md
- chunk files
```

Example:

```text
Runner: 100/100, ok=false none.
Judge: 90/100.
Failed ids: 8, 9, 21, 29, 52, 68, 72, 87, 91, 100.
Locked judge: 94/100, failed 33,52,68,72,74,98.
Disagreements: ours additionally failed 8,9,21,29,87,91,100; ours passed 33,74,98.
Validation: 100 judgments, no missing ids, no duplicate ids.
```

## Common Mistakes

- Treating runner score as correctness score.
- Comparing scores produced by different judge prompts.
- Letting subagents judge all 100 tasks in one context.
- Aggregating from chat summaries instead of saved JSON.
- Silently excluding provider/auth/quota failures.
- Failing tasks for minor non-core formatting issues.
- Passing tasks with wrong entity type or wrong source because they produced a large file.
- Ignoring saved artifacts when final prose is empty.
- Ignoring final/result contradictions.

## Practical Calibration

This judge is intentionally balanced:

- It is stricter than runner pass.
- It is less harsh than a maximal audit that fails every missing secondary field.
- It should fail wrong entity, wrong source, incomplete core extraction, exact-count mismatch, and unsupported all-null answers.
- It should pass correct saved artifacts even when final prose is terse.

The number is useful only when the same judge is reused across runs.

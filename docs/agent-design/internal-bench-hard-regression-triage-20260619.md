# Internal Bench Hard Regression Triage - 2026-06-19

Run under triage:

- run root:
  `/home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-053010`
- judged score: `91/106`
- raw reference:
  `/home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json`
- comparison:
  `/home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-053010/current-vs-raw-judged-delta.md`

## Current-Only Regressions

These tasks passed in the raw Codex + browser-harness reference and failed in
the current CodexEngine + simple browser-harness run.

| Task | Failure | Likely fix path |
| --- | --- | --- |
| `0kqsos` | Indeed access/detail evidence drift. Current canonical artifacts did not prove selected listings. | Rerun with access/profile variance isolated; require canonical detail evidence before final. |
| `6dpbhs` | Weak evidence. Final answer was likely correct, but the source page/XML was not saved for judge tracing. | Prompt/tool discipline: save source text or source artifact before finalizing archival answers. |
| `82kkzm` | Complete article text was replaced with summaries/`N/A` despite raw article text being partly recoverable. | Implemented audit block for self-withheld complete article text. |
| `jgzlma` | Galaxus extraction incomplete and task hit safety timeout. | Rerun Galaxus-first with higher one-task safety cap; keep top-20 per marketplace audit. |
| `l3gywi` | CarMax exact URLs were source-limited; current evidence looked weaker than reference. | Save compact extraction audit proving exact stock ids checked and source-limited `N/A` is intentional. |
| `m5zja8` | UNGM result included obvious non-IT tenders because broad UNSPSC/category matches were treated as IT projects. | Implemented UNGM IT scope-drift audit. |
| `q46nou` | Quora login wall caused fallback to an arbitrary Space/profile. | Completion check must require selected Space URL, contributor profile URL, and follower evidence tied to that Space. |
| `s3kkv9` | VRBO access/site variance caused blocked or wrong state. | Rerun with raw browser-script pattern and explicit date/private-pool verification. |
| `v18kgy` | Artifact explicitly declared `complete: false`, but the audit did not block it. | Implemented false-complete marker audit. |
| `zcotoh` | EIB task explicitly allowed `N/A`, but audit treated all unavailable fields as a hard failure. | Implemented task-allowed sentinel relaxation and prompt guidance. |

## Implemented In This Checkpoint

- `prompts/simple-harness-artifact-audit.py`
  - Allows all-`N/A` required fields only when the task explicitly permits
    unavailable-field sentinels.
  - Blocks `complete`, `is_complete`, or `ready_for_done` set to `false`.
  - Blocks complete-article tasks that self-mark full text unavailable or
    replace article text with summaries.
  - Blocks UNGM IT-project results that contain likely non-IT rows such as
    meta-analysis, policy-brief, communication, videography, or electoral
    process work without real IT evidence.
- `prompts/dataset-case-simple-harness-user.md`
  - Clarifies that task-permitted sentinels should not make an otherwise
    complete exact-record artifact incomplete.
- `scripts/run-internal-bench-hard-openai.sh`
  - Adds `--task-id ID` selection for focused reruns while defaulting full runs
    to `--all` and `EXPECTED_TOTAL=106`.
- `scripts/finalize-ibh-judged-run.sh` and `scripts/compare-judged-runs.py`
  - Allow strict subset current-vs-reference comparisons without weakening full
    reference validation.

## Verification

Commands run:

```bash
python3 -m py_compile prompts/simple-harness-artifact-audit.py scripts/compare-judged-runs.py scripts/audit-ibh-run-completion.py
bash -n scripts/run-internal-bench-hard-openai.sh scripts/finalize-ibh-judged-run.sh
uv run --with pytest python -m pytest -q python/tests/test_simple_harness_artifact_audit.py
OPENAI_API_KEY=dummy BROWSER_USE_API_KEY=dummy BROWSER_HARNESS_SRC=/home/exedev/repos/browser-harness/src scripts/run-internal-bench-hard-openai.sh --dry-run --run-id targeted-dry-run --root /tmp/ibh-targeted-dry-run --skip-build --task-id zcotoh --task-id m5zja8 --task-id v18kgy --task-id 82kkzm
OPENAI_API_KEY=dummy BROWSER_USE_API_KEY=dummy BROWSER_HARNESS_SRC=/home/exedev/repos/browser-harness/src scripts/run-internal-bench-hard-openai.sh --dry-run --judge --run-id full-dry-run-check --root /tmp/ibh-full-dry-run-check --skip-build
scripts/finalize-ibh-judged-run.sh --run-root /home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-053010 --run-id ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-053010 --judge-dir /home/exedev/eval-runs/ibh-simple-harness-openai-simple-harness-parity-test-20260618-20260619-053010/judge --reference-aggregate /home/exedev/eval-runs/ibh-purecodex-175254-rejudge-jsonl-20260613/judge_aggregate.json
```

Historical artifact audit checks:

- `zcotoh`: old `result.json` now passes because the task explicitly permitted
  `N/A` for unavailable fields.
- `82kkzm`: old `result.json` now fails for missing/withheld complete article
  text.
- `m5zja8`: old `result.json` now fails for non-IT UNGM scope drift.
- `v18kgy`: old `result.json` now fails on `complete: false`.

## Next Eval Step

Run a focused judged subset for:

```text
zcotoh m5zja8 v18kgy 82kkzm
```

If those improve or fail for a different reason, rerun the full 106-task
Internal_Bench_hard path and compare against the raw Codex + browser-harness
reference.

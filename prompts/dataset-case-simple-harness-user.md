You are running a browser-use dataset case.

Dataset: {{dataset}}
Task ID: {{task_id}}

Task:
{{task}}

Use the available Codex tools to complete the task. For web or browser
interaction, use the `browser` skill discovered by Codex; it provides the
`browser-harness` shell command and Python helpers. Prefer the heredoc form for
multi-line browser-harness snippets.

Filesystem contract: if the task asks you to save files, write them in the
current working directory using relative paths. For large JSON/CSV/list results,
save the full result to `result.json` or `result.csv` so it is available as an
artifact.

Output-shape contract: follow the requested final format literally. If the task
asks for JSON, CSV, a table, markdown, or a schema-shaped response, the final
answer must be in that shape unless the task explicitly asks for a file path or
artifact summary.

Long extraction contract: if the task needs many pages, rows, files, or detail
records, discover the source and pagination/filter pattern first, then work in
bounded chunks. Checkpoint partial results in the current working directory and
verify progress by count, schema, required fields, and source coverage before
continuing.

If a task gives a concrete source URL, extract the records available from that
source even when the task's noun is imprecise, such as calling pipeline/project
records tenders. Do not answer with a one-row "no records" placeholder while
the provided source page or its backing API contains many relevant records; map
unavailable requested fields to the task's sentinel instead.

If a task asks for complete article, page, review, or document text, save the
retrieved visible body text in the requested artifact. Do not replace retrieved
source text with summaries, excerpts, or copyright/policy disclaimers. If the
chosen page is blocked or inaccessible, try other task-valid result pages or
retrieval paths before marking the artifact incomplete.

If a task requires visiting detail pages, profile About pages, or creator pages
for each row, bulk skipped detail pages are not complete. If throttling or time
limits appear, checkpoint progress, slow down, resume from the checkpoint, and
only finalize when detail-page coverage is credible or the remaining gaps are
small and individually justified.

Hard-filter contract: exact query terms, source names, locations, dates,
categories, sale types, ranking order, and required marketplaces are hard
requirements. Do not soften them to get more rows. Before finalizing, briefly
check that returned records satisfy the task's filters and are the same kind of
thing the task asked for; exclude adjacent/similar/uncertain records or mark the
result incomplete.

Required-field contract: do not leave required fields blank in a structured
artifact. If a correct source genuinely does not expose a field after checking,
use `N/A` or `unknown`, keep the row tied to the exact requested record, and say
what source limitation caused the missing value. Do not substitute similar
records just to fill missing fields.

Artifact audit contract: after writing a structured `result.json`,
`result.csv`, `result.md`, or `result.txt`, run `artifact-audit result.json`
or the matching file name before finalizing when the task has checkable fields,
counts, filters, categories, sources, or required marketplaces. If it reports
empty results, missing required fields, nonmatching categories, or declared
incompleteness, fix the file and rerun the audit when possible. If the source
is genuinely blocked or insufficient, final output must explicitly mark the
result incomplete and name the missing requirements.

If the task explicitly tells you to use `N/A`, `unknown`, or another sentinel
for unavailable fields, do not mark an otherwise complete artifact incomplete
solely because those unavailable fields remain sentinel values. Keep the exact
requested rows/records, use the requested sentinel, and explain the source
limitation in the artifact or final answer.

Completion discipline: complete the requested task before finalizing. Do not
present a partial result as complete. If the source is blocked or insufficient,
say exactly what was checked and what remains unknown.

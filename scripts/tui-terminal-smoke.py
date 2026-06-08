#!/usr/bin/env python3
"""Real-terminal smoke tests for the Rust TUI.

This intentionally tests the app through tmux instead of Ratatui's TestBackend.
The goal is to catch bugs that only appear with a live terminal viewport:
duplicated panels in scrollback, broken bracketed paste, and stale redraws.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ARTIFACT_DIR = Path("/tmp/but-design-loop")
HISTORY_TITLE = "Browse and resume previous tasks"
STATUS_BAR_PREFIX = "GPT-5.5  ·"
TMUX = ["tmux", "-L", f"but-smoke-{os.getpid()}", "-f", "/dev/null"]


def run(cmd: list[str], *, check: bool = True, text: str | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=ROOT,
        input=text,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=check,
    )


def binary_version_label(binary: Path) -> str:
    output = run([str(binary), "--version"]).stdout.strip()
    version = output.rsplit(" ", 1)[-1]
    return f"v{version}"


def tmux(*args: str, check: bool = True) -> str:
    return run([*TMUX, *args], check=check).stdout


def tmux_send(session: str, *keys: str) -> None:
    run([*TMUX, "send-keys", "-t", session, *keys])


def tmux_send_literal(session: str, value: str) -> None:
    run([*TMUX, "send-keys", "-t", session, "-l", value])


def tmux_send_shift_enter(session: str) -> None:
    # Crossterm decodes the kitty/CSI-u enhanced keyboard encoding that the
    # TUI enables at startup. tmux's symbolic "S-Enter" is not reliable across
    # terminal builds and can arrive as plain Enter.
    tmux_send_literal(session, "\x1b[13;2u")


def tmux_send_shift_letter(session: str, letter: str) -> None:
    if len(letter) != 1 or not letter.isalpha():
        raise ValueError("shift-letter smoke helper expects one alphabetic character")
    tmux_send_literal(session, f"\x1b[{ord(letter.upper())};2u")


def tmux_send_alt_backspace(session: str) -> None:
    # CSI-u Alt+Backspace. This matches the enhanced keyboard protocol enabled
    # by the TUI and avoids relying on tmux's terminal-specific M-BSpace name.
    tmux_send_literal(session, "\x1b[127;3u")


def tmux_send_alt_left(session: str) -> None:
    tmux_send_literal(session, "\x1b[1;3D")


def tmux_send_alt_right(session: str) -> None:
    tmux_send_literal(session, "\x1b[1;3C")


def tmux_send_mouse_down(session: str, column: int, row: int) -> None:
    # SGR mouse coordinates are 1-based terminal cells.
    tmux_send_literal(session, f"\x1b[<0;{column + 1};{row + 1}M")


def tmux_send_mouse_up(session: str, column: int, row: int) -> None:
    # Real terminal clicks report a release after the press.
    tmux_send_literal(session, f"\x1b[<0;{column + 1};{row + 1}m")


def tmux_send_mouse_click(session: str, column: int, row: int) -> None:
    tmux_send_mouse_down(session, column, row)
    time.sleep(0.05)
    tmux_send_mouse_up(session, column, row)


def capture(session: str, name: str) -> str:
    text = tmux("capture-pane", "-t", session, "-p", "-S", "-200")
    ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
    (ARTIFACT_DIR / f"tui-terminal-smoke-{name}.txt").write_text(text)
    return text


def capture_visible(session: str, name: str) -> str:
    text = tmux("capture-pane", "-t", session, "-p")
    ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
    (ARTIFACT_DIR / f"tui-terminal-smoke-{name}.txt").write_text(text)
    return text


def wait_for(session: str, needle: str, name: str, timeout: float = 8.0) -> str:
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        last = capture(session, name)
        if needle in last:
            return last
        time.sleep(0.2)
    raise AssertionError(f"timed out waiting for {needle!r}\n\n{last}")


def capture_after_idle(session: str, name: str, delay: float = 0.5, *, visible_only: bool = False) -> str:
    time.sleep(delay)
    if visible_only:
        return capture_visible(session, name)
    return capture(session, name)


def assert_contains(text: str, needle: str, context: str) -> None:
    if needle not in text:
        raise AssertionError(f"{context}: expected {needle!r}\n\n{text}")


def assert_not_contains(text: str, needle: str, context: str) -> None:
    if needle in text:
        raise AssertionError(f"{context}: unexpected {needle!r}\n\n{text}")


def assert_stripped_line(text: str, expected: str, context: str) -> None:
    if not any(line.strip() == expected for line in text.splitlines()):
        raise AssertionError(f"{context}: expected stripped line {expected!r}\n\n{text}")


def assert_no_stripped_line(text: str, expected: str, context: str) -> None:
    if any(line.strip() == expected for line in text.splitlines()):
        raise AssertionError(f"{context}: unexpected stripped line {expected!r}\n\n{text}")


def assert_no_command_row(text: str, command: str, context: str) -> None:
    for line in text.splitlines():
        stripped = line.strip()
        if stripped == command or stripped.startswith(f"{command} "):
            raise AssertionError(f"{context}: unexpected command row {command!r}\n\n{text}")


def assert_count(text: str, needle: str, expected: int, context: str) -> None:
    count = text.count(needle)
    if count != expected:
        raise AssertionError(f"{context}: expected {expected} x {needle!r}, saw {count}\n\n{text}")


def assert_row_gap_at_most(text: str, before: str, after: str, max_rows: int, context: str) -> None:
    lines = text.splitlines()
    before_indexes = [idx for idx, line in enumerate(lines) if before in line]
    if not before_indexes:
        raise AssertionError(f"{context}: missing {before!r}\n\n{text}")
    before_idx = before_indexes[-1]
    after_idx = next((idx for idx in range(before_idx + 1, len(lines)) if after in lines[idx]), None)
    if after_idx is None:
        raise AssertionError(f"{context}: missing {after!r} after {before!r}\n\n{text}")
    rows_between = after_idx - before_idx - 1
    if rows_between > max_rows:
        raise AssertionError(
            f"{context}: expected at most {max_rows} rows between {before!r} and {after!r}, saw {rows_between}\n\n{text}"
        )


def assert_line_directly_followed_by(text: str, before: str, after: str, context: str) -> None:
    lines = text.splitlines()
    for idx, line in enumerate(lines[:-1]):
        if before in line and after in lines[idx + 1]:
            return
    raise AssertionError(
        f"{context}: expected a line containing {before!r} to be directly followed by {after!r}\n\n{text}"
    )


def assert_same_line_gap_at_most(
    text: str, before: str, after: str, max_gap: int, context: str
) -> None:
    invalid_gaps: list[int] = []
    for line in text.splitlines():
        if before not in line or after not in line:
            continue
        search_from = 0
        while True:
            before_idx = line.find(before, search_from)
            if before_idx < 0:
                break
            after_idx = line.find(after, before_idx + len(before))
            if after_idx >= 0:
                gap = after_idx - (before_idx + len(before))
                if gap <= max_gap:
                    return
                invalid_gaps.append(gap)
            search_from = before_idx + 1
    if invalid_gaps:
        closest_gap = min(invalid_gaps)
        raise AssertionError(
            f"{context}: expected {after!r} within {max_gap} cells after {before!r}, saw closest gap {closest_gap}\n\n{text}"
        )
    raise AssertionError(f"{context}: missing same line {before!r} and {after!r}\n\n{text}")


def first_text_column(text: str, needle: str, context: str) -> int:
    for line in text.splitlines():
        if needle in line:
            return len(line) - len(line.lstrip())
    raise AssertionError(f"{context}: missing {needle!r}\n\n{text}")


def assert_first_text_columns_close(
    text: str, before: str, after: str, max_delta: int, context: str
) -> None:
    before_column = first_text_column(text, before, context)
    after_column = first_text_column(text, after, context)
    delta = abs(before_column - after_column)
    if delta > max_delta:
        raise AssertionError(
            f"{context}: expected first text columns within {max_delta}, "
            f"saw {before_column} vs {after_column}\n\n{text}"
        )


def assert_row_near_bottom(text: str, needle: str, max_rows_from_bottom: int, context: str) -> None:
    lines = text.splitlines()
    indexes = [idx for idx, line in enumerate(lines) if needle in line]
    if not indexes:
        raise AssertionError(f"{context}: missing {needle!r}\n\n{text}")
    rows_from_bottom = len(lines) - indexes[-1] - 1
    if rows_from_bottom > max_rows_from_bottom:
        raise AssertionError(
            f"{context}: expected {needle!r} within {max_rows_from_bottom} rows of bottom, saw {rows_from_bottom}\n\n{text}"
        )


def assert_regex_count(text: str, pattern: str, expected: int, context: str) -> None:
    count = len(re.findall(pattern, text, flags=re.MULTILINE))
    if count != expected:
        raise AssertionError(f"{context}: expected {expected} x /{pattern}/, saw {count}\n\n{text}")


def assert_first_content_near_top(text: str, max_row: int, context: str) -> None:
    for idx, line in enumerate(text.splitlines()):
        if line.strip():
            if idx > max_row:
                raise AssertionError(
                    f"{context}: first visible content should be within {max_row} rows of top, saw row {idx}\n\n{text}"
                )
            return
    raise AssertionError(f"{context}: capture had no visible content\n\n{text}")


def assert_max_consecutive_blank_lines(text: str, max_blank_lines: int, context: str) -> None:
    longest = 0
    current = 0
    for line in text.splitlines():
        if line.strip():
            current = 0
        else:
            current += 1
            longest = max(longest, current)
    if longest > max_blank_lines:
        raise AssertionError(
            f"{context}: expected at most {max_blank_lines} consecutive blank visible lines, saw {longest}\n\n{text}"
        )


def assert_no_ansi(text: str, context: str) -> None:
    if re.search(r"\x1b\[[0-?]*[ -/]*[@-~]", text):
        raise AssertionError(f"{context}: output contained ANSI escapes\n\n{text!r}")


def latest_session_id(state_dir: Path) -> str:
    with sqlite3.connect(state_dir / "state.db") as conn:
        row = conn.execute("SELECT id FROM sessions ORDER BY updated_ms DESC LIMIT 1").fetchone()
    if row is None:
        raise AssertionError(f"missing session in {state_dir}/state.db")
    return str(row[0])


def append_store_event(state_dir: Path, session_id: str, event_type: str, payload: dict[str, object]) -> int:
    now_ms = int(time.time() * 1000)
    with sqlite3.connect(state_dir / "state.db") as conn:
        cursor = conn.execute(
            "INSERT INTO events(id, session_id, ts_ms, type, payload_json) VALUES (?, ?, ?, ?, ?)",
            (uuid.uuid4().hex, session_id, now_ms, event_type, json.dumps(payload)),
        )
        conn.execute("UPDATE sessions SET updated_ms = ? WHERE id = ?", (now_ms, session_id))
        return int(cursor.lastrowid)


def set_session_status(state_dir: Path, session_id: str, status: str) -> None:
    now_ms = int(time.time() * 1000)
    with sqlite3.connect(state_dir / "state.db") as conn:
        conn.execute(
            "UPDATE sessions SET status = ?, updated_ms = ? WHERE id = ?",
            (status, now_ms, session_id),
        )


def create_live_subagent(
    state_dir: Path,
    parent_id: str,
    *,
    nickname: str = "repo-explorer",
    status: str = "running",
) -> str:
    child_id = uuid.uuid4().hex[:12]
    now_ms = int(time.time() * 1000)
    artifact_root = state_dir / "artifacts" / child_id
    artifact_root.mkdir(parents=True, exist_ok=True)
    cwd = str(ROOT)

    def event_row(session_id: str, event_type: str, payload: dict[str, object]) -> tuple[str, str, int, str, str]:
        return (uuid.uuid4().hex, session_id, now_ms, event_type, json.dumps(payload))

    with sqlite3.connect(state_dir / "state.db") as conn:
        conn.execute(
            """
            INSERT INTO sessions(
                id, parent_id, cwd, artifact_root, status,
                created_ms, updated_ms, agent_path, agent_nickname, agent_role
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                child_id,
                parent_id,
                cwd,
                str(artifact_root),
                status,
                now_ms,
                now_ms,
                f"/root/{nickname}",
                nickname,
                "explorer",
            ),
        )
        conn.execute(
            """
            INSERT INTO agent_edges(parent_session_id, child_session_id, status, created_ms, updated_ms)
            VALUES (?, ?, ?, ?, ?)
            """,
            (parent_id, child_id, status, now_ms, now_ms),
        )
        conn.executemany(
            "INSERT INTO events(id, session_id, ts_ms, type, payload_json) VALUES (?, ?, ?, ?, ?)",
            [
                event_row(child_id, "session.created", {}),
                event_row(child_id, "agent.context", {"nickname": nickname, "role": "explorer"}),
                event_row(child_id, "file.read", {"path": "/repo/README.md"}),
                event_row(
                    parent_id,
                    "agent.spawned",
                    {
                        "child_session_id": child_id,
                        "nickname": nickname,
                        "role": "explorer",
                    },
                ),
                event_row(parent_id, "model.thinking_delta", {"text": "parent is waiting"}),
            ],
        )
        conn.execute("UPDATE sessions SET updated_ms = ? WHERE id IN (?, ?)", (now_ms, parent_id, child_id))
    return child_id


def assert_no_legacy_dashboard_chrome(text: str, context: str) -> None:
    assert_not_contains(text, "[box] Active objective", context)
    assert_not_contains(text, "[box] Task complete", context)
    assert_not_contains(text, "TERMINAL    RUNTIME", context)


def build_binary() -> Path:
    run(["cargo", "build", "-q", "-p", "browser-use-tui", "--bin", "but"])
    binary = ROOT / "target" / "debug" / "but"
    if not binary.exists():
        raise AssertionError(f"missing built binary: {binary}")
    return binary


def start_session(
    session: str,
    binary: Path,
    state_dir: Path,
    *,
    seed_demo: str = "running",
    select_latest: bool = True,
) -> None:
    tmux("kill-session", "-t", session, check=False)
    tmux("new-session", "-d", "-s", session, "-x", "120", "-y", "28")
    tmux("resize-window", "-t", session, "-x", "120", "-y", "28")
    select_arg = "--select-latest " if select_latest else ""
    command = (
        f"cd {ROOT} && BUT_TELEMETRY=0 OPENAI_API_KEY=but-smoke-key "
        f"LLM_BROWSER_OPENAI_API_KEY=but-smoke-key OPENROUTER_API_KEY=but-smoke-key "
        f"LLM_BROWSER_OPENAI_COMPAT_API_KEY=but-smoke-key {binary} "
        f"--state-dir {state_dir} --seed-demo {seed_demo} {select_arg}"
        "--agent none --browser 'Local Chrome' --height 28"
    )
    tmux_send(session, command, "C-m")
    first_visible_text = "Type to steer the agent" if select_latest else "Tell the browser what to do..."
    wait_for(session, first_visible_text, f"initial-{seed_demo}")


def smoke_interactive_terminal(binary: Path) -> None:
    session = f"but-smoke-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-"))
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Type to steer the agent", "initial-running")

        tmux_send(session, "Tab", "Down", "Down", "Down")
        history = wait_for(session, HISTORY_TITLE, "history")
        assert_count(history, HISTORY_TITLE, 1, "history should be live, not appended repeatedly")
        assert_not_contains(history, "^[[B", "arrow keys should be consumed by the TUI")

        tmux_send(session, "Escape")
        after_history = capture_after_idle(session, "main-after-history", visible_only=True)
        assert_contains(after_history, "Type to steer the agent", "history escape should restore main composer")
        assert_not_contains(after_history, HISTORY_TITLE, "history escape should close the overlay")

        tmux_send_literal(session, "alpha")
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "beta")
        multiline = wait_for(session, "beta", "shift-enter-newline")
        assert_contains(multiline, "> alpha", "multiline input first line")
        assert_contains(multiline, "  beta", "multiline input second line")
        assert_not_contains(multiline, "Follow-up\n    alpha", "shift-enter must not submit")
        assert_not_contains(multiline, "alpha|", "composer should use the terminal cursor, not a fake pipe")
        assert_not_contains(multiline, "beta|", "composer should use the terminal cursor, not a fake pipe")
        assert_no_legacy_dashboard_chrome(multiline, "multiline edit should not show old dashboard chrome")
        assert_count(multiline, STATUS_BAR_PREFIX, 1, "multiline edit should not append duplicate app screens")

        tmux_send(session, "C-u", "C-u")
        line_removed = capture_after_idle(session, "ctrl-u-removes-empty-composer-line", visible_only=True)
        assert_contains(line_removed, "> alpha", "ctrl-u should keep the previous composer line")
        assert_not_contains(line_removed, "  beta", "second ctrl-u should remove the cleared composer line")

        tmux_send(session, "C-c")
        wait_for(session, "Type to steer the agent", "main-after-clear")

        tmux_send_shift_letter(session, "A")
        shifted = wait_for(session, "> A", "shift-letter-uppercase")
        assert_not_contains(shifted, "> a", "shift-letter input should keep uppercase text")
        tmux_send(session, "C-c")
        wait_for(session, "Type to steer the agent", "main-after-shift-letter-clear")

        tmux_send_literal(session, "/stuff")
        slash_filter = wait_for(session, "> stuff", "slash-palette-filter")
        assert_contains(slash_filter, "No commands match.", "slash palette should own slash-prefixed input")
        tmux_send(session, "C-u")
        slash_cleared = capture_after_idle(session, "slash-palette-ctrl-u-clear", visible_only=True)
        assert_contains(slash_cleared, "/task", "ctrl-u should clear slash palette filter")
        assert_not_contains(slash_cleared, "No commands match.", "ctrl-u should clear no-match palette state")
        tmux_send(session, "Escape")
        after_slash_filter = capture_after_idle(session, "main-after-slash-palette-close", visible_only=True)
        assert_contains(after_slash_filter, "Type to steer the agent", "slash escape should restore main composer")
        assert_not_contains(after_slash_filter, "No commands match.", "slash escape should close the overlay")

        tmux_send_literal(session, "open http:/")
        nonleading_slash = capture_after_idle(session, "main-nonleading-slash-input", visible_only=True)
        assert_contains(nonleading_slash, "> open http:/", "slash after prompt text should stay in composer")
        assert_not_contains(nonleading_slash, "/task", "slash after prompt text should not open slash palette")
        assert_not_contains(
            nonleading_slash,
            "No commands match.",
            "slash after prompt text should not filter slash palette",
        )
        tmux_send(session, "C-c")
        wait_for(session, "Type to steer the agent", "main-after-nonleading-slash-clear")

        tmux_send_literal(session, "something-bla")
        wait_for(session, "> something-bla", "alt-backspace-hyphenated-word-before")
        tmux_send_alt_backspace(session)
        hyphenated_word = capture_after_idle(session, "alt-backspace-hyphenated-word", visible_only=True)
        assert_contains(hyphenated_word, "Type to steer the agent", "alt-backspace should delete to previous blank")
        assert_not_contains(hyphenated_word, "> something-bla", "alt-backspace should delete to previous blank")
        assert_not_contains(hyphenated_word, "> something-", "alt-backspace should delete to previous blank")

        tmux_send_literal(session, "before something-bla")
        wait_for(session, "> before something-bla", "alt-backspace-space-word-before")
        tmux_send_alt_backspace(session)
        spaced_word = capture_after_idle(session, "alt-backspace-space-word", visible_only=True)
        assert_contains(spaced_word, "> before ", "alt-backspace should stop at previous blank")
        assert_not_contains(spaced_word, "> before something-bla", "alt-backspace should stop at previous blank")
        tmux_send_alt_backspace(session)
        wait_for(session, "Type to steer the agent", "main-after-alt-backspace-word-clear")

        tmux_send_literal(session, "alpha beta gamma")
        wait_for(session, "> alpha beta gamma", "alt-arrow-word-before")
        tmux_send_alt_left(session)
        tmux_send_literal(session, "X")
        alt_left = wait_for(session, "> alpha beta Xgamma", "alt-left-word")
        assert_contains(
            alt_left,
            "> alpha beta Xgamma",
            "alt-left should move to previous word start",
        )
        tmux_send_alt_right(session)
        tmux_send_literal(session, "!")
        alt_right = capture_after_idle(session, "alt-right-word", visible_only=True)
        assert_contains(
            alt_right,
            "> alpha beta Xgamma!",
            "alt-right should move to next word end",
        )
        tmux_send(session, "C-c")
        wait_for(session, "Type to steer the agent", "main-after-alt-arrow-clear")

        bracketed = "\x1b[200~paste one\npaste two\x1b[201~"
        tmux_send_literal(session, bracketed)
        pasted = wait_for(session, "paste two", "bracketed-paste")
        assert_contains(pasted, "paste one", "bracketed paste first line")
        assert_contains(pasted, "paste two", "bracketed paste second line")
        assert_not_contains(pasted, "^[[200~", "bracketed paste markers should not leak")
        assert_not_contains(pasted, "paste two|", "paste should use the terminal cursor, not a fake pipe")
        assert_no_legacy_dashboard_chrome(pasted, "paste should not show old dashboard chrome")
        assert_count(pasted, STATUS_BAR_PREFIX, 1, "paste should not append duplicate app screens")

        tmux_send(session, "C-c")
        after_paste_clear = capture_after_idle(session, "main-after-paste-clear", visible_only=True)
        assert_contains(after_paste_clear, "Type to steer the agent", "clearing pasted text should not stop the task")
        assert_not_contains(after_paste_clear, "paste two", "ctrl+c should clear pasted composer text")

        tmux_send(session, "/")
        palette = wait_for(session, "/model", "slash-palette-open")
        assert_contains(palette, "/task", "slash palette should show the first product action")
        assert_contains(palette, "/profile", "slash palette should expose the default profile setting")
        assert_not_contains(palette, "/plan", "slash palette should not expose removed Plan mode")
        assert_not_contains(palette, "/mode ", "slash palette should not expose collaboration mode")
        assert_contains(palette, "/model", "slash palette should show the model command in the visible window")
        assert_not_contains(palette, "choose collaboration mode", "slash palette should not expose collaboration mode")
        assert_not_contains(palette, "/plan", "slash palette should not expose Plan mode")
        assert_contains(palette, "↑↓ navigate", "slash palette footer should be visible")
        assert_no_command_row(palette, "/mode", "slash palette should not expose collaboration mode")
        assert_not_contains(palette, "/plan", "slash palette should not expose Plan mode")
        assert_not_contains(palette, "/auth", "slash palette overflows extra actions into filtering")
        assert_not_contains(palette, "filter actions", "slash palette should not show a redundant filter prompt")
        assert_first_content_near_top(palette, 2, "slash palette should not be pushed down by previous viewport state")
        tmux_send_literal(session, "goal")
        goal_actions = wait_for(session, "> goal", "slash-palette-goal-filtered")
        assert_contains(goal_actions, "/goal", "slash palette should find goal management through filtering")
        assert_not_contains(goal_actions, "/model", "slash palette should hide non-matching model command")
        tmux_send(session, "C-u")
        wait_for(session, "/model", "slash-palette-filter-cleared-after-goal")
        tmux_send_literal(session, "auth")
        auth_actions = wait_for(session, "> auth", "slash-palette-auth-filtered")
        assert_contains(auth_actions, "/auth", "slash palette should find auth through filtering")
        assert_not_contains(auth_actions, "/model", "slash palette should hide non-matching model command")
        tmux_send(session, "C-u")
        wait_for(session, "/model", "slash-palette-filter-cleared")
        tmux_send_literal(session, "sync")
        sync_actions = wait_for(session, "> sync", "slash-palette-sync-filtered")
        assert_contains(sync_actions, "/sync-cookies", "slash palette should find cookie sync through filtering")
        assert_not_contains(sync_actions, "/model", "slash palette should hide non-matching model command")
        tmux_send(session, "C-u")
        wait_for(session, "/model", "slash-palette-filter-cleared-after-sync")
        tmux_send_literal(session, "bro")
        actions = wait_for(session, "> bro", "slash-palette-filtered")
        assert_contains(actions, "/browser", "slash palette should show matching command")
        assert_not_contains(actions, "/model", "slash palette should hide non-matching commands")

        tmux_send(session, "Escape")
        after_slash = capture_after_idle(session, "main-after-slash-palette", visible_only=True)
        assert_contains(after_slash, "Type to steer the agent", "slash escape should restore main composer")
        assert_not_contains(after_slash, "> bro", "slash escape should clear the slash filter")
        assert_not_contains(after_slash, "↑↓ navigate", "slash escape should close the overlay")
        tmux_send_literal(session, "/model")
        tmux_send(session, "Enter")
        model = wait_for(session, "Pick a recommended model or choose a provider", "model-panel")
        assert_contains(model, "recommended", "model surface should show recommended models")
        assert_contains(model, "providers", "model surface should show provider rows")
        assert_contains(model, "GPT-5.5", "model surface should show top recommended rows")
        assert_contains(model, "OpenRouter", "model surface should show provider rows")
        assert_contains(model, "DeepSeek", "model surface should show all visible provider rows")
        assert_not_contains(model, "Change OpenRouter API key", "model surface should keep key changes inside provider menu")
        assert_contains(model, "Enter:select", "model surface footer should be visible")
        assert_first_content_near_top(model, 2, "model surface should not be rendered in the compact dock")
        tmux_send(session, "Escape")
        after_model = capture_after_idle(session, "main-after-model-surface", visible_only=True)
        assert_contains(after_model, "Type to steer the agent", "model escape should restore main composer")
        assert_not_contains(after_model, "Pick a recommended model or choose a provider", "model escape should close the overlay")
        tmux_send(session, "F2")
        browser = wait_for(session, "Current browser", "browser-panel")
        assert_count(browser, "Current browser", 1, "browser panel should be live, not appended repeatedly")

        tmux("resize-window", "-t", session, "-x", "100", "-y", "22")
        resized_small = capture_after_idle(session, "resize-100x22", visible_only=True)
        assert_contains(resized_small, "Current browser", "resize should keep the live app visible")
        assert_count(resized_small, "Current browser", 1, "resize shrink should redraw in place")
        assert_not_contains(resized_small, "^[[", "resize shrink should not leak escape sequences")

        tmux("resize-window", "-t", session, "-x", "120", "-y", "28")
        resized_large = capture_after_idle(session, "resize-120x28", visible_only=True)
        assert_contains(resized_large, "Current browser", "resize grow should keep the live app visible")
        assert_count(resized_large, "Current browser", 1, "resize grow should redraw in place")

        for width, height in [(112, 26), (96, 22), (132, 31), (104, 24), (120, 28)]:
            tmux("resize-window", "-t", session, "-x", str(width), "-y", str(height))
        resized_burst = capture_after_idle(session, "resize-burst-120x28", visible_only=True)
        assert_contains(resized_burst, "Current browser", "resize burst should keep the live app visible")
        assert_count(resized_burst, "Current browser", 1, "resize burst should redraw in place")
        assert_not_contains(resized_burst, "^[[", "resize burst should not leak escape sequences")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_live_subagent_status_bar(binary: Path) -> None:
    session = f"but-smoke-subagent-status-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-subagent-status-"))
    try:
        start_session(session, binary, state_dir)
        parent_id = latest_session_id(state_dir)
        append_store_event(
            state_dir,
            parent_id,
            "model.turn.response",
            {"tool_call_count": 1},
        )
        create_live_subagent(state_dir, parent_id, status="running")
        visible = wait_for(session, "(1 subagent running)", "subagent-status-bar")
        assert_contains(visible, "Working...", "parent live status should remain compact and visible")
        assert_contains(visible, "(1 subagent running)", "subagent count should share the live status row")
        assert_not_contains(visible, "subagents  repo-explorer running", "subagent details should not render as a separate bar")
        assert_not_contains(visible, "parent is waiting", "raw parent thinking should stay out of the live status")
        assert_not_contains(visible, "read /repo/README.md", "child details should not leak into parent view")
        assert_line_directly_followed_by(
            visible,
            "Working...",
            "╭",
            "combined live status should sit directly above composer",
        )
        assert_no_ansi(visible, "subagent status capture should be plain terminal text")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_ready_resize_does_not_leave_stale_frames(binary: Path) -> None:
    session = f"but-smoke-ready-resize-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-ready-resize-"))
    expected_version = binary_version_label(binary)
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="done",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "ready-resize-start")
        for width, height in [(100, 22), (132, 31), (96, 24), (120, 28)]:
            tmux("resize-window", "-t", session, "-x", str(width), "-y", str(height))
        visible = capture_after_idle(session, "ready-resize-visible", visible_only=True)
        full = capture_after_idle(session, "ready-resize-scrollback")
        for name, text in [("visible", visible), ("scrollback", full)]:
            assert_contains(text, "Tell the browser what to do...", f"ready resize {name} should keep composer visible")
            assert_count(text, "Browser Use", 1, f"ready resize {name} should keep one header")
            assert_count(text, expected_version, 1, f"ready resize {name} should keep one version")
            assert_count(text, "press / for shortcuts", 1, f"ready resize {name} should keep one shortcut hint")
            assert_not_contains(text, "^[[", f"ready resize {name} should not leak escape sequences")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_composer_mouse_clicks_empty_and_line_end(binary: Path) -> None:
    session = f"but-smoke-composer-mouse-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-composer-mouse-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="running",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "composer-mouse-ready")
        tmux_send_literal(session, "first line")
        tmux_send_shift_enter(session)
        tmux_send_shift_enter(session)
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "third line")
        wait_for(session, "third line", "composer-mouse-before")

        before = capture_after_idle(session, "composer-mouse-before-idle", visible_only=True)
        lines = before.splitlines()
        first_row = next(idx for idx, line in enumerate(lines) if "first line" in line)
        third_row = next(idx for idx, line in enumerate(lines) if "third line" in line)
        blank_row = first_row + 1
        second_blank_row = first_row + 2
        first_col = lines[first_row].index("first line")
        third_col = lines[third_row].index("third line")

        tmux_send_mouse_click(session, third_col + 5, third_row)
        tmux_send_mouse_click(session, first_col, blank_row)
        tmux_send_literal(session, "A")
        wait_for(session, "  A", "composer-mouse-empty-row-first-cell")
        tmux_send_mouse_click(session, first_col + 10, second_blank_row)
        tmux_send_literal(session, "B")
        wait_for(session, "  B", "composer-mouse-empty-row-tenth-cell")
        after_blank = capture_after_idle(session, "composer-mouse-empty-row-visible", visible_only=True)
        assert_contains(after_blank, "  A", "first-cell empty row click should place cursor on the empty logical line")
        assert_contains(after_blank, "  B", "tenth-column empty row click should place cursor on the empty logical line")
        assert_not_contains(after_blank, "first lineA", "first-cell empty row click should not reuse the previous line")
        assert_not_contains(after_blank, "first lineB", "empty row click should not reuse the previous line")
        assert_not_contains(after_blank, "  AB", "tenth-column empty row click should not reuse the prior empty line")

        lines = after_blank.splitlines()
        first_row = next(idx for idx, line in enumerate(lines) if "first line" in line)
        far_col = min(len(lines[first_row]) - 4, first_col + 70)
        tmux_send_mouse_click(session, far_col, first_row)
        tmux_send_literal(session, "C")
        wait_for(session, "first lineC", "composer-mouse-line-end")
        after_first = capture_after_idle(session, "composer-mouse-line-end-visible", visible_only=True)
        assert_contains(after_first, "first lineC", "far-right line click should clip to that line end")
        assert_not_contains(after_first, "  AC", "far-right line click should not spill into the first empty line")
        assert_not_contains(after_first, "  BC", "far-right line click should not spill into the second empty line")

        tmux_send(session, "C-c")
        wait_for(session, "Tell the browser what to do...", "composer-mouse-padding-cleared")
        tmux_send_literal(session, "first line")
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "third line")
        wait_for(session, "third line", "composer-mouse-padding-before")
        padding_before = capture_after_idle(
            session,
            "composer-mouse-padding-before-visible",
            visible_only=True,
        )
        lines = padding_before.splitlines()
        first_row = next(idx for idx, line in enumerate(lines) if "first line" in line)
        padding_row = first_row + 2
        padding_col = len(lines[padding_row]) - 8
        tmux_send_mouse_click(session, padding_col, padding_row)
        tmux_send_literal(session, "P")
        padding_after = wait_for(session, "  P", "composer-mouse-padding-row")
        assert_contains(
            padding_after,
            "  P",
            "far-right click on a visual padding row should create that blank line",
        )
        assert_not_contains(
            padding_after,
            "third lineP",
            "visual padding row click should not reuse the previous line end",
        )

        tmux_send(session, "C-c")
        wait_for(session, "Tell the browser what to do...", "composer-mouse-cleared")
        tmux_send_literal(session, "first line")
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "          ")
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "third line")
        wait_for(session, "third line", "composer-mouse-hidden-space-before")
        hidden_space_before = capture_after_idle(
            session,
            "composer-mouse-hidden-space-before-visible",
            visible_only=True,
        )
        lines = hidden_space_before.splitlines()
        first_row = next(idx for idx, line in enumerate(lines) if "first line" in line)
        first_col = lines[first_row].index("first line")
        tmux_send_mouse_click(session, first_col + 10, first_row + 1)
        tmux_send_literal(session, "W")
        hidden_space_after = wait_for(session, "  W", "composer-mouse-hidden-space-row")
        assert_contains(
            hidden_space_after,
            "  W",
            "tenth-column click on a whitespace-only line should behave like an empty line",
        )
        assert_not_contains(
            hidden_space_after,
            "first lineW",
            "whitespace-only row click should not reuse the previous line",
        )

        tmux_send(session, "C-c")
        wait_for(session, "Tell the browser what to do...", "composer-mouse-overflow-cleared")
        for idx in range(1, 14):
            tmux_send_literal(session, f"line{idx}")
            if idx != 13:
                tmux_send_shift_enter(session)
        wait_for(session, "line13", "composer-mouse-overflow-before")
        overflow_before = capture_after_idle(
            session,
            "composer-mouse-overflow-before-visible",
            visible_only=True,
        )
        assert_contains(overflow_before, "line4", "overflowed composer should show line4 as first visible row")
        assert_contains(overflow_before, "line13", "overflowed composer should show the final line before click")
        assert_not_contains(
            overflow_before,
            "> line1",
            "overflowed composer should be scrolled to the tail before the click",
        )
        lines = overflow_before.splitlines()
        line4_row = next(idx for idx, line in enumerate(lines) if "line4" in line)
        line4_col = lines[line4_row].index("line4")
        tmux_send_mouse_click(session, min(len(lines[line4_row]) - 4, line4_col + 70), line4_row)
        overflow_after_click = capture_after_idle(
            session,
            "composer-mouse-overflow-after-click-visible",
            visible_only=True,
        )
        assert_not_contains(
            overflow_after_click,
            "> line1",
            "clicking an overflowed visible row should not scroll the composer to the start",
        )
        after_click_lines = overflow_after_click.splitlines()
        line4_after_row = next(idx for idx, line in enumerate(after_click_lines) if "line4" in line)
        if line4_after_row != line4_row:
            raise AssertionError(
                "clicking an overflowed visible row should keep that row under the mouse\n\n"
                f"{overflow_after_click}"
            )
        tmux_send_literal(session, "Z")
        wait_for(session, "line4Z", "composer-mouse-overflow-after-insert")
        overflow_after_insert = capture_after_idle(
            session,
            "composer-mouse-overflow-after-insert-visible",
            visible_only=True,
        )
        assert_not_contains(
            overflow_after_insert,
            "> line1",
            "typing after an overflowed row click should keep the clicked window pinned",
        )
        line4_insert_row = next(
            idx for idx, line in enumerate(overflow_after_insert.splitlines()) if "line4Z" in line
        )
        if line4_insert_row != line4_row:
            raise AssertionError(
                "typing after an overflowed row click should keep the clicked row stable\n\n"
                f"{overflow_after_insert}"
            )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_history_selection_emits_native_transcript(binary: Path) -> None:
    session = f"but-smoke-history-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-history-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="cancelled",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "history-start-ready")
        tmux_send(session, "Tab")
        wait_for(session, HISTORY_TITLE, "history-open-cancelled")
        tmux_send(session, "Enter")
        selected = wait_for(session, "Ask a follow-up", "history-select-cancelled")
        assert_contains(selected, "Find the top 5 Hacker", "selected task should be in native scrollback")
        assert_contains(selected, "stopped", "cancelled task should render as native transcript")
        assert_not_contains(selected, "+- stopped", "native transcript should use simple section labels")
        assert_not_contains(selected, "+- browser", "native transcript should use simple section labels")
        assert_row_gap_at_most(
            selected,
            "Progress is saved",
            "Ask a follow-up",
            5,
            "stopped status and composer should stay grouped together",
        )
        assert_not_contains(selected, "\x1b[", "native transcript should not leak escapes")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_tall_terminal_keeps_running_controls_attached_to_content(binary: Path) -> None:
    session = f"but-smoke-height-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-height-"))
    try:
        tmux("kill-session", "-t", session, check=False)
        tmux("new-session", "-d", "-s", session, "-x", "120", "-y", "40")
        command = (
            f"cd {ROOT} && {binary} "
            f"--state-dir {state_dir} --seed-demo running --select-latest "
            "--agent none --browser 'Local Chrome'"
        )
        tmux_send(session, command, "C-m")
        wait_for(session, "Type to steer the agent", "height-120x40-history")
        visible = capture_visible(session, "height-120x40")
        full = capture_after_idle(session, "height-120x40-scrollback")
        assert_no_legacy_dashboard_chrome(visible, "tall terminal should not show old dashboard chrome")
        assert_contains(
            visible,
            "Reading the page and preparing the next browser action",
            "tall terminal should show streaming answer text",
        )
        assert_count(visible, "Type to steer the agent", 1, "tall terminal should have one live composer")
        assert_row_gap_at_most(
            visible,
            "Reading the page and preparing the next browser action",
            "Type to steer",
            8,
            "running composer should stay attached to sparse running content",
        )
        assert_contains(
            full,
            "Find the top 5 Hacker News posts",
            "running task prompt should be native terminal scrollback",
        )
        assert_not_contains(
            full,
            "waiting for GPT-5.5",
            "transient model wait records should not pollute native scrollback",
        )
        assert_not_contains(
            full,
            "• answer draft",
            "streaming chunks should stay in the live viewport, not permanent scrollback",
        )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_escape_pauses_running_session(binary: Path) -> None:
    session = f"but-smoke-esc-pause-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-esc-pause-"))
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Type to steer the agent", "double-escape-running")
        tmux_send(session, "Escape")
        paused = wait_for(
            session,
            "What should the model do differently? If something went wrong, please use /feedback :)",
            "escape-paused",
        )
        paused = wait_for(
            session,
            "Ask a follow-up",
            "escape-paused-followup",
        )
        assert_contains(paused, "Ask a follow-up", "paused session should restore follow-up composer")
        assert_contains(paused, "• Conversation paused", "single escape should commit a paused transcript row")
        assert_not_contains(paused, "Conversation paused -", "paused session should not duplicate the title in detail copy")
        assert_not_contains(paused, "Session paused", "paused session should use conversation copy")
        assert_not_contains(paused, "Previous work", "paused session should not show redundant history action")
        assert_not_contains(paused, "Start a new task", "paused session should not show redundant next actions")
        assert_not_contains(paused, "esc again to edit messages", "single escape should not arm message selector")
        assert_not_contains(paused, "^[[", "escape pause should not leak escape sequences")
        with sqlite3.connect(state_dir / "state.db") as conn:
            paused_count = conn.execute(
                "SELECT COUNT(*) FROM events WHERE type = 'session.cancelled' "
                "AND payload_json LIKE '%\"reason\":\"session paused\"%'"
            ).fetchone()[0]
        if paused_count != 1:
            raise AssertionError(f"expected one paused cancellation event, saw {paused_count}")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_escape_reclaims_prompt_before_output(binary: Path) -> None:
    session = f"but-smoke-esc-reclaim-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-esc-reclaim-"))
    prompt = "take this back"
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="done",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "esc-reclaim-start-ready")
        tmux_send_literal(session, prompt)
        tmux_send(session, "Enter")
        wait_for(session, f"> {prompt}", "esc-reclaim-submitted")
        wait_for(session, "Working...", "esc-reclaim-working")
        time.sleep(0.5)

        tmux_send(session, "Escape")
        reclaimed = wait_for(session, "Message returned to composer.", "esc-reclaim-returned")
        assert_contains(
            reclaimed,
            f"> {prompt}",
            "single escape before output should return the submitted prompt to composer",
        )
        assert_not_contains(
            reclaimed,
            "esc again to edit messages",
            "single escape before output should reclaim instead of arming the message selector",
        )
        with sqlite3.connect(state_dir / "state.db") as conn:
            rollback_count = conn.execute(
                "SELECT COUNT(*) FROM events WHERE type = 'session.rollback' "
                "AND payload_json LIKE '%\"action\":\"take_back\"%'"
            ).fetchone()[0]
        if rollback_count != 1:
            raise AssertionError(f"expected one take_back rollback event, saw {rollback_count}")

        tmux_send(session, "C-u")
        cleared = capture_after_idle(session, "esc-reclaim-cleared", visible_only=True)
        assert_not_contains(
            cleared,
            prompt,
            "clearing the reclaimed composer text should remove the prompt from the visible terminal",
        )
        assert_not_contains(cleared, "^[[", "escape reclaim should not leak escape sequences")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_tab_queues_followup_after_current_turn(binary: Path) -> None:
    session = f"but-smoke-tab-queue-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-tab-queue-"))
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Type to steer the agent", "tab-queue-running")
        tmux_send_literal(session, "after current turn")
        tmux_send(session, "Tab")
        queued = wait_for(session, "queued follow-up", "tab-queue-pending")
        assert_contains(queued, "after current turn", "tab should keep the queued prompt visible")
        assert_contains(queued, "Type to steer", "tab should leave the active turn running")

        session_id = latest_session_id(state_dir)
        append_store_event(
            state_dir,
            session_id,
            "session.done",
            {"result": "Current turn finished"},
        )
        set_session_status(state_dir, session_id, "done")
        sent = wait_for(session, "sending", "tab-queue-sent")
        assert_contains(
            sent,
            "> after current turn",
            "queued follow-up should submit when the current turn finishes",
        )
        assert_not_contains(sent, "queued follow-up", "sent queued follow-up should stop rendering as queued")
        assert_not_contains(sent, "^[[", "tab queued follow-up should not leak escape sequences")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_enter_previews_followup_after_next_tool(binary: Path) -> None:
    session = f"but-smoke-enter-pending-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-enter-pending-"))
    prompt = "adjust after next tool"
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Type to steer the agent", "enter-pending-running")
        tmux_send_literal(session, prompt)
        tmux_send(session, "Enter")
        pending = wait_for(
            session,
            "Messages to be submitted after next tool call",
            "enter-pending-preview",
        )
        assert_contains(
            pending,
            "press esc to dequeue and edit",
            "active follow-up preview should show the reclaim affordance",
        )
        assert_contains(pending, f"↳ {prompt}", "active follow-up preview should show the prompt")
        assert_not_contains(
            pending,
            f"> {prompt}",
            "active follow-up should not render as a committed transcript prompt before drain",
        )
        pending_visible = capture_after_idle(
            session,
            "enter-pending-preview-visible",
            visible_only=True,
        )
        assert_contains(
            pending_visible,
            "• browser",
            "active follow-up preview should not hide the live browser block",
        )

        session_id = latest_session_id(state_dir)
        with sqlite3.connect(state_dir / "state.db") as conn:
            row = conn.execute(
                "SELECT seq, payload_json FROM events WHERE session_id = ? "
                "AND type = 'session.followup.pending' "
                "AND payload_json LIKE ? "
                "ORDER BY seq DESC LIMIT 1",
                (session_id, f'%"{prompt}"%'),
            ).fetchone()
        if row is None:
            raise AssertionError("missing active follow-up event")
        followup_seq = int(row[0])
        if '"delivery":"after_next_tool_call"' not in str(row[1]):
            raise AssertionError(f"active follow-up lacked after_next_tool_call delivery: {row[1]}")

        stream_text = "I'll inspect the repo before using tools."
        append_store_event(state_dir, session_id, "model.stream_delta", {"text": stream_text})
        wait_for(session, stream_text, "enter-pending-streaming")
        visible_streaming = capture_after_idle(
            session,
            "enter-pending-streaming-visible",
            visible_only=True,
        )
        assert_contains(
            visible_streaming,
            "Messages to be submitted after next tool call",
            "active follow-up preview should remain visible below streaming text",
        )
        assert_contains(
            visible_streaming,
            "• browser",
            "active follow-up preview should not replace the live browser block after streaming updates",
        )
        stream_idx = visible_streaming.find(stream_text)
        preview_idx = visible_streaming.find("Messages to be submitted after next tool call")
        if stream_idx < 0 or preview_idx < 0 or stream_idx > preview_idx:
            raise AssertionError(
                "active follow-up preview should be anchored below live streaming text\n\n"
                f"{visible_streaming}"
            )

        committed_seq = append_store_event(
            state_dir,
            session_id,
            "session.followup",
            {"text": prompt, "pending_from_seq": followup_seq},
        )
        append_store_event(
            state_dir,
            session_id,
            "agent.turn_queue_drained",
            {
                "phase": "after_tool_outputs",
                "session_messages": 1,
                "mailbox_messages": 0,
                "last_seq": committed_seq,
            },
        )
        drained = wait_for(session, f"> {prompt}", "enter-pending-drained")
        assert_not_contains(
            drained,
            "Messages to be submitted after next tool call",
            "drained active follow-up should stop rendering as pending",
        )
        assert_not_contains(drained, "^[[", "active follow-up preview should not leak escape sequences")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_escape_reclaims_pending_active_followup(binary: Path) -> None:
    session = f"but-smoke-esc-pending-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-esc-pending-"))
    prompt = "reclaim this steer"
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Type to steer the agent", "esc-pending-running")
        tmux_send_literal(session, prompt)
        tmux_send(session, "Enter")
        wait_for(
            session,
            "Messages to be submitted after next tool call",
            "esc-pending-preview",
        )
        tmux_send(session, "Escape")
        reclaimed = wait_for(session, f"> {prompt}", "esc-pending-reclaimed")
        assert_not_contains(
            reclaimed,
            "Messages to be submitted after next tool call",
            "escape should dequeue pending active follow-up instead of keeping it pending",
        )
        assert_not_contains(reclaimed, "esc again to edit messages", "escape should not arm message selector")

        session_id = latest_session_id(state_dir)
        with sqlite3.connect(state_dir / "state.db") as conn:
            cancelled = conn.execute(
                "SELECT COUNT(*) FROM events WHERE session_id = ? "
                "AND type = 'session.followup.cancelled'",
                (session_id,),
            ).fetchone()[0]
            interrupted = conn.execute(
                "SELECT COUNT(*) FROM events WHERE session_id = ? "
                "AND type = 'session.followup.interrupt_sent'",
                (session_id,),
            ).fetchone()[0]
            cancel_requested = conn.execute(
                "SELECT COUNT(*) FROM events WHERE session_id = ? "
                "AND type = 'session.cancel_requested'",
                (session_id,),
            ).fetchone()[0]
            status = conn.execute(
                "SELECT status FROM sessions WHERE id = ?",
                (session_id,),
            ).fetchone()[0]
        if cancelled != 1:
            raise AssertionError(f"expected one followup.cancelled marker, saw {cancelled}")
        if interrupted != 0:
            raise AssertionError(f"escape reclaim should not interrupt, saw {interrupted} interrupt marker(s)")
        if cancel_requested != 0:
            raise AssertionError(f"escape reclaim should not cancel the session, saw {cancel_requested}")
        if status != "running":
            raise AssertionError(f"pending steer reclaim should leave the session running, saw {status}")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_escape_reclaims_queued_followup(binary: Path) -> None:
    session = f"but-smoke-esc-queue-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-esc-queue-"))
    prompt = "return queued follow-up"
    try:
        start_session(session, binary, state_dir)
        wait_for(session, "Type to steer the agent", "esc-queue-running")
        tmux_send_literal(session, prompt)
        tmux_send(session, "Tab")
        wait_for(session, "queued follow-up", "esc-queue-pending")

        tmux_send(session, "Escape")
        reclaimed = wait_for(session, f"> {prompt}", "esc-queue-reclaimed")
        assert_contains(reclaimed, f"> {prompt}", "escape should put queued text back in composer")
        assert_not_contains(reclaimed, "esc again to edit messages", "escape should not arm message selector")
        visible_reclaimed = capture_after_idle(session, "esc-queue-reclaimed-visible", visible_only=True)
        assert_not_contains(
            visible_reclaimed,
            "• queued follow-up",
            "reclaimed queued follow-up should leave the live viewport queue preview",
        )

        session_id = latest_session_id(state_dir)
        with sqlite3.connect(state_dir / "state.db") as conn:
            cancelled = conn.execute(
                "SELECT COUNT(*) FROM events WHERE session_id = ? "
                "AND type = 'session.queued_followup.cancelled' "
                "AND payload_json LIKE '%\"reason\":\"reclaimed from escape\"%'",
                (session_id,),
            ).fetchone()[0]
        if cancelled != 1:
            raise AssertionError(f"expected one reclaimed queued follow-up, saw {cancelled}")

        append_store_event(state_dir, session_id, "session.done", {"result": "Current turn finished"})
        set_session_status(state_dir, session_id, "done")
        capture_after_idle(session, "esc-queue-after-done", visible_only=True)
        with sqlite3.connect(state_dir / "state.db") as conn:
            submitted = conn.execute(
                "SELECT COUNT(*) FROM events WHERE session_id = ? "
                "AND type = 'session.followup' "
                "AND payload_json LIKE ?",
                (session_id, f'%"{prompt}"%'),
            ).fetchone()[0]
        if submitted != 0:
            raise AssertionError(f"reclaimed queued follow-up should not submit, saw {submitted}")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_completed_history_uses_native_scrollback(binary: Path) -> None:
    session = f"but-smoke-long-history-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-long-history-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="long",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "long-history-start-ready")
        tmux_send(session, "Tab", "Enter")
        selected = wait_for(session, "scroll check line 60", "long-history-selected")
        visible = capture_after_idle(session, "long-history-selected-visible", visible_only=True)
        assert_contains(selected, "scroll check line 1", "native transcript should include first result line")
        assert_contains(selected, "scroll check line 60", "native transcript should include last result line")
        assert_contains(selected, "source", "native transcript should include source section")
        assert_contains(visible, "Ask a follow-up", "live viewport should redraw the composer after transcript insert")
        assert_contains(visible, "scroll check line 60", "live viewport should show the native transcript tail")
        assert_contains(visible, "https://news.ycombinator.com", "live viewport should show source above composer")
        assert_max_consecutive_blank_lines(
            visible,
            8,
            "long completed history should not leave a large blank gap in the visible terminal",
        )
        assert_not_contains(visible, "scroll check line 1", "live viewport should not echo the completed result")
        assert_row_gap_at_most(
            visible,
            "https://news.ycombinator.com",
            "Ask a follow-up",
            3,
            "native scrollback composer should sit directly after the transcript tail",
        )
        assert_not_contains(selected, "+- source", "native transcript should use simple section labels")
        assert_not_contains(selected, "+- result", "native transcript should use simple section labels")
        assert_not_contains(selected, "earlier steps", "native transcript should not compact activity")
        assert_not_contains(selected, "\x1b[", "native transcript should not leak escapes")

        tmux_send_literal(session, "continue")
        tmux_send(session, "Enter")
        running = wait_for(session, "> continue", "long-history-followup-running")
        visible_running = capture_after_idle(session, "long-history-followup-visible", visible_only=True)
        assert_contains(
            visible_running,
            "scroll check line 60",
            "prompt-only long follow-up should keep the previous transcript tail visible",
        )
        assert_contains(
            visible_running,
            "https://news.ycombinator.com",
            "prompt-only long follow-up should keep the completed source visible",
        )
        assert_contains(
            visible_running,
            "> continue",
            "prompt-only long follow-up should show the submitted prompt immediately",
        )
        assert_contains(
            visible_running,
            "sending",
            "prompt-only long follow-up should show a pending indicator immediately",
        )
        assert_row_gap_at_most(
            visible_running,
            "> continue",
            "sending",
            2,
            "prompt-only long follow-up indicator should sit near submitted text",
        )
        first_line = next((line.strip() for line in visible_running.splitlines() if line.strip()), "")
        if first_line == "> continue":
            raise AssertionError("submitted follow-up should not become the visible top anchor\n\n" + visible_running)
        assert_no_legacy_dashboard_chrome(visible_running, "follow-up should not show old dashboard chrome")
        assert_not_contains(running, "using browser", "internal browser helper starts should stay hidden")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_streaming_transcript_scrolls_above_composer(binary: Path) -> None:
    session = f"but-smoke-transcript-scroll-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-transcript-scroll-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="long",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "transcript-scroll-start-ready")
        tmux_send(session, "Tab", "Enter")
        wait_for(session, "scroll check line 60", "transcript-scroll-selected")
        tmux_send_literal(session, "continue")
        tmux_send(session, "Enter")
        wait_for(session, "> continue", "transcript-scroll-after-enter")

        session_id = latest_session_id(state_dir)
        append_store_event(
            state_dir,
            session_id,
            "session.startup_warning",
            {
                "message": "Model `gpt-5.5` is not in the active model catalog; using conservative fallback capabilities."
            },
        )
        append_store_event(
            state_dir,
            session_id,
            "model.turn.request",
            {"model": "GPT-5.5", "provider": "codex", "turn_idx": 1},
        )
        long_stream = "\n".join(f"live output line {idx:02}" for idx in range(1, 41))
        append_store_event(
            state_dir,
            session_id,
            "model.stream_delta",
            {"text": long_stream, "turn_idx": 1},
        )
        wait_for(session, "live output line 40", "transcript-scroll-streaming")
        visible = capture_after_idle(
            session,
            "transcript-scroll-streaming-visible",
            visible_only=True,
        )
        full = capture_after_idle(
            session,
            "transcript-scroll-streaming-full",
            visible_only=False,
        )
        assert_contains(
            full,
            "https://news.ycombinator.com",
            "streaming should keep native transcript tail in scrollback",
        )
        assert_contains(
            full,
            "> continue",
            "streaming should keep submitted prompt in scrollback",
        )
        assert_contains(
            full,
            "live output line 01",
            "streaming should preserve early live rows in native scrollback",
        )
        assert_contains(
            visible,
            "live output line 40",
            "streaming should show the latest live row",
        )
        assert_contains(
            visible,
            "Type to steer the agent",
            "streaming should keep composer docked below the transcript body",
        )
        append_store_event(
            state_dir,
            session_id,
            "session.done",
            {"result": long_stream},
        )
        time.sleep(0.5)
        done_full = capture(session, "transcript-scroll-done-full")
        assert_contains(
            done_full,
            "Model `gpt-5.5` is not in the active model catalog",
            "final committed answer should preserve deferred startup warnings",
        )
        assert_count(
            done_full,
            "live output line 01",
            1,
            "final committed answer should not duplicate already streamed native rows",
        )
        assert_count(
            done_full,
            "live output line 40",
            1,
            "final committed answer should not duplicate the streamed tail row",
        )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_prompt_only_followup_keeps_completed_transcript(binary: Path) -> None:
    session = f"but-smoke-prompt-only-followup-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-prompt-only-followup-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="done",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "prompt-only-followup-start-ready")
        tmux_send(session, "Tab", "Enter")
        wait_for(session, "Top 5 Hacker News posts", "prompt-only-followup-selected")
        tmux_send_literal(session, "yo")
        tmux_send(session, "Enter")
        wait_for(session, "> yo", "prompt-only-followup-running")
        visible = capture_after_idle(
            session,
            "prompt-only-followup-visible",
            visible_only=True,
        )
        assert_contains(
            visible,
            "Top 5 Hacker News posts",
            "prompt-only follow-up should keep the completed answer visible",
        )
        assert_contains(
            visible,
            "https://news.ycombinator.com",
            "prompt-only follow-up should keep the completed source visible",
        )
        assert_contains(
            visible,
            "> yo",
            "prompt-only follow-up should show the submitted prompt immediately",
        )
        assert_contains(
            visible,
            "sending",
            "prompt-only follow-up should show a pending indicator immediately",
        )
        assert_row_gap_at_most(
            visible,
            "> yo",
            "sending",
            2,
            "prompt-only follow-up indicator should sit near submitted text",
        )
        first_line = next((line.strip() for line in visible.splitlines() if line.strip()), "")
        if first_line == "> yo":
            raise AssertionError("submitted follow-up should not become the visible top anchor\n\n" + visible)

        session_id = latest_session_id(state_dir)
        append_store_event(
            state_dir,
            session_id,
            "model.turn.request",
            {"model": "GPT-5.5", "provider": "codex", "turn_idx": 1},
        )
        wait_for(session, "thinking", "prompt-only-followup-live-thinking")
        live_frames = []
        for idx in range(40):
            live_visible = capture_after_idle(
                session,
                f"prompt-only-followup-live-{idx:02d}",
                delay=0.03,
                visible_only=True,
            )
            live_frames.append(live_visible)
        for live_visible in live_frames:
            assert_contains(
                live_visible,
                "Top 5 Hacker News posts",
                "follow-up live activity should not resize away from the completed transcript",
            )
            assert_contains(
                live_visible,
                "https://news.ycombinator.com",
                "follow-up live activity should keep the completed source visible",
            )
            assert_contains(
                live_visible,
                "> yo",
                "follow-up live activity should keep the submitted prompt visible",
            )
            assert_contains(
                live_visible,
                "thinking",
                "follow-up live activity should keep the thinking indicator visible",
            )
            assert_row_gap_at_most(
                live_visible,
                "> yo",
                "thinking",
                2,
                "follow-up thinking indicator should sit near submitted text",
            )
            assert_not_contains(
                live_visible,
                "waiting for GPT-5.5",
                "follow-up live activity should not show model wait text",
            )
            first_line = next((line.strip() for line in live_visible.splitlines() if line.strip()), "")
            if first_line == "> yo":
                raise AssertionError("submitted follow-up should not become the visible top anchor\n\n" + live_visible)

        append_store_event(
            state_dir,
            session_id,
            "file.read",
            {"path": "Cargo.toml"},
        )
        wait_for(session, "read Cargo.toml", "prompt-only-followup-deferred-read")
        deferred_read_visible = capture_after_idle(
            session,
            "prompt-only-followup-deferred-read-visible",
            visible_only=True,
        )
        assert_contains(
            deferred_read_visible,
            "read Cargo.toml",
            "follow-up activity should show as the active tail before streaming starts",
        )

        streaming_text = (
            "I'll inspect the repo structure and key docs/config first, then\n"
            "summarize what it appears to be and how it's organized."
        )
        append_store_event(
            state_dir,
            session_id,
            "model.stream_delta",
            {"text": streaming_text, "turn_idx": 1},
        )
        wait_for(session, "summarize what", "prompt-only-followup-streaming")
        fresh_streaming_visible = capture_visible(
            session,
            "prompt-only-followup-streaming-fresh-visible",
        )
        assert_not_contains(
            fresh_streaming_visible,
            "Thinking...",
            "fresh streaming follow-up should not show the thinking indicator while text is arriving",
        )
        append_store_event(
            state_dir,
            session_id,
            "model.response.output_item.completed",
            {"item_type": "message", "phase": "commentary", "turn_idx": 1},
        )
        wait_for(session, "Thinking...", "prompt-only-followup-commentary-complete")
        streaming_visible = capture_after_idle(
            session,
            "prompt-only-followup-streaming-visible",
            visible_only=True,
        )
        streaming_full = capture_after_idle(
            session,
            "prompt-only-followup-streaming-full",
            visible_only=False,
        )
        assert_contains(
            streaming_visible,
            "> yo",
            "streaming follow-up should first commit the submitted prompt into native scrollback",
        )
        assert_contains(
            streaming_visible,
            "I'll inspect the repo structure and key docs/config first, then",
            "streaming follow-up should keep live output visible",
        )
        assert_contains(
            streaming_visible,
            "summarize what it appears to be and how it's organized.",
            "streaming follow-up should keep the active tail visible",
        )
        assert_contains(
            streaming_visible,
            "read Cargo.toml",
            "streaming follow-up should keep the previous activity tail visible",
        )
        assert_contains(
            streaming_full,
            "read Cargo.toml",
            "streaming follow-up should flush the previous activity tail into native scrollback",
        )
        assert_line_directly_followed_by(
            streaming_visible,
            "first, then",
            "summarize what",
            "streaming follow-up should not insert a separator inside the assistant paragraph",
        )
        assert_contains(
            streaming_visible,
            "Thinking...",
            "commentary before tool calls should keep the live thinking indicator visible",
        )
        assert_not_contains(
            streaming_visible,
            "Working...",
            "streaming follow-up should not show a redundant live heartbeat",
        )
        assert_not_contains(
            streaming_visible,
            "waiting for GPT-5.5",
            "streaming follow-up should not show model wait text",
        )
        append_store_event(
            state_dir,
            session_id,
            "model.turn.response",
            {"tool_call_count": 1, "turn_idx": 1},
        )
        append_store_event(
            state_dir,
            session_id,
            "file.read",
            {"path": "README.md"},
        )
        wait_for(session, "read README.md", "prompt-only-followup-tool-output")
        response_visible = capture_after_idle(
            session,
            "prompt-only-followup-tool-output-visible",
            visible_only=True,
        )
        assert_not_contains(
            response_visible,
            "• note",
            "model turn responses with tool calls should not render note rows",
        )
        assert_count(
            response_visible,
            "I'll inspect the repo structure and key docs/config first, then",
            1,
            "streaming text before a tool-call response should commit once above the tool row",
        )
        assert_line_directly_followed_by(
            response_visible,
            "first, then",
            "summarize what",
            "committed streaming text should keep paragraph lines adjacent above the tool row",
        )
        assert_count(
            response_visible,
            "summarize what it appears to be and how it's organized.",
            1,
            "streaming text before a tool-call response should commit once above the tool row",
        )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_short_completed_history_has_live_preview(binary: Path) -> None:
    session = f"but-smoke-short-done-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-short-done-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="done",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "short-done-start-ready")
        tmux_send(session, "Tab", "Enter")
        selected = wait_for(session, "Top 5 Hacker News posts", "short-done-selected")
        visible = capture_after_idle(session, "short-done-selected-visible", visible_only=True)
        assert_contains(selected, "Top 5 Hacker News posts", "selected task should be replayed to native scrollback")
        assert_contains(visible, "Top 5 Hacker News posts", "live viewport should not be blank for completed history")
        assert_contains(visible, "https://news.ycombinator.com", "live viewport should show completed source")
        assert_first_text_columns_close(
            visible,
            "https://news.ycombinator.com",
            "Ask a follow-up",
            2,
            "completed transcript and composer should share a content gutter",
        )
        assert_row_gap_at_most(
            visible,
            "https://news.ycombinator.com",
            "Ask a follow-up",
            3,
            "short completed composer should stay attached to the result",
        )
        tmux_send_literal(session, "alpha")
        tmux_send_shift_enter(session)
        tmux_send_literal(session, "beta")
        multiline = wait_for(session, "beta", "short-done-multiline-followup")
        visible_multiline = capture_after_idle(
            session,
            "short-done-multiline-followup-visible",
            visible_only=True,
        )
        assert_contains(
            multiline,
            "Top 5 Hacker News posts",
            "completed transcript should remain while editing multiline follow-up",
        )
        assert_contains(visible_multiline, "> alpha", "completed follow-up first line")
        assert_contains(visible_multiline, "  beta", "completed follow-up second line")
        assert_not_contains(
            visible_multiline,
            "Follow-up\n    alpha",
            "shift-enter in completed history must not submit",
        )
        assert_no_legacy_dashboard_chrome(
            visible_multiline,
            "completed multiline edit should not show old dashboard chrome",
        )
        tmux_send(session, "C-c")
        wait_for(session, "Ask a follow-up", "short-done-after-multiline-clear")
        tmux_send(session, "/")
        slash = wait_for(session, "/task", "short-done-slash-palette")
        assert_contains(slash, "/history", "slash palette should open on completed history")
        assert_contains(slash, "Example story", "slash palette should layer over completed transcript")
        assert_contains(slash, "Ask a follow-up", "slash palette should layer over composer")
        assert_not_contains(slash, "filter actions", "slash palette should not show a redundant filter prompt")
        assert_row_gap_at_most(
            slash,
            "/task",
            "/history",
            2,
            "slash palette should render command rows together",
        )
        tmux_send(session, "Escape")
        after_slash = capture_after_idle(session, "short-done-after-slash-close", visible_only=True)
        assert_contains(after_slash, "Ask a follow-up", "completed slash escape should restore composer")
        assert_not_contains(after_slash, "/task", "completed slash escape should close the overlay")
        tmux_send_literal(session, "/auth")
        tmux_send(session, "Enter")
        auth = wait_for(session, "Sign in to a model provider", "short-done-auth-panel")
        assert_contains(auth, "Find the top 5 Hacker News posts", "auth panel should layer over completed transcript")
        assert_contains(auth, "Ask a follow", "auth panel should leave completed composer in place")
        assert_contains(auth, "Enter:select", "auth panel footer should be visible")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_main_resize_does_not_duplicate_transcript(binary: Path) -> None:
    session = f"but-smoke-main-resize-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-main-resize-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="long",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "main-resize-start-ready")
        tmux_send(session, "Tab", "Enter")
        wait_for(session, "scroll check line 60", "main-resize-selected")
        tmux("resize-window", "-t", session, "-x", "96", "-y", "24")
        small = capture_after_idle(session, "main-resize-96x24")
        tmux("resize-window", "-t", session, "-x", "140", "-y", "34")
        large = capture_after_idle(session, "main-resize-140x34")
        for name, text in [("small", small), ("large", large)]:
            assert_not_contains(text, "^[[", f"main resize {name} should not leak escapes")
            assert_contains(text, "> Find the top 5 Hacker News posts", f"main resize {name} should keep transcript visible")
            assert_contains(text, "Ask a follow-up", f"main resize {name} should keep composer visible")
            assert_no_legacy_dashboard_chrome(text, f"main resize {name} should not show old dashboard chrome")
            if text.count("> Find the top 5 Hacker News posts") > 1:
                raise AssertionError(
                    f"main resize {name} should not replay the full transcript more than once\n\n{text}"
                )
            if len(re.findall(r"scroll check line 1$", text, flags=re.MULTILINE)) > 1:
                raise AssertionError(
                    f"main resize {name} should not duplicate the start of the transcript\n\n{text}"
                )
            if text.count("scroll check line 60") > 2:
                raise AssertionError(
                    f"main resize {name} should only replay one transcript plus one live tail preview\n\n{text}"
                )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_session_switch_clears_previous_transcript(binary: Path) -> None:
    session = f"but-smoke-switch-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-switch-"))
    transient_task = "temporary switch task should disappear"
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="long",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "switch-start-ready")
        tmux_send_literal(session, transient_task)
        tmux_send(session, "Enter")
        wait_for(session, "temporary switch task sh", "switch-transient-created")

        tmux_send(session, "Tab")
        wait_for(session, HISTORY_TITLE, "switch-history-open")
        tmux_send(session, "Down", "Enter")
        selected = wait_for(session, "scroll check line 60", "switch-long-selected")
        visible = wait_for(session, "Ask a follow-up", "switch-long-selected-visible")

        assert_contains(selected, "scroll check line 1", "selected transcript should be replayed after switch")
        assert_contains(selected, "scroll check line 60", "selected transcript should include full result after switch")
        assert_not_contains(visible, transient_task, "session switch should clear the previous visible transcript")
        assert_contains(visible, "Ask a follow-up", "session switch should redraw the composer after replay")
        assert_not_contains(visible, "^[[", "session switch clear should not leak escape sequences")
        assert_first_content_near_top(visible, 2, "selected long transcript should not drift down after switch")
        assert_max_consecutive_blank_lines(
            visible,
            8,
            "selected long transcript should not leave a large blank gap after switch",
        )

        tmux_send(session, "Tab")
        wait_for(session, HISTORY_TITLE, "switch-history-reopen-transient")
        tmux_send(session, "Enter")
        transient_visible = wait_for(session, transient_task, "switch-transient-selected-visible")
        assert_first_content_near_top(
            transient_visible,
            2,
            "switching back to another session should reset the inline viewport origin",
        )

        tmux_send(session, "Tab")
        wait_for(session, HISTORY_TITLE, "switch-history-reopen-long")
        tmux_send(session, "Down", "Enter")
        long_again = wait_for(session, "scroll check line 60", "switch-long-selected-again-visible")
        assert_first_content_near_top(
            long_again,
            2,
            "switching repeatedly should not accumulate blank rows above transcript",
        )
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_large_composer_input_is_responsive(binary: Path) -> None:
    session = f"but-smoke-large-input-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-large-input-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="running",
            select_latest=True,
        )
        wait_for(session, "Type to steer the agent", "large-input-start")
        wait_for(session, "Type to steer the agent", "large-input-composer-ready")
        large_text = "x" * 1200
        started = time.time()
        for offset in range(0, len(large_text), 50):
            tmux_send_literal(session, large_text[offset : offset + 50])
        typed = wait_for(session, "x" * 80, "large-input-typed", timeout=1.5)
        elapsed = time.time() - started
        if elapsed > 1.5:
            raise AssertionError(f"large input took too long to appear: {elapsed:.2f}s")
        assert_not_contains(typed, "^[[200~", "large input should not leak bracketed paste markers")
        assert_not_contains(typed, "^[[", "large input should not leak escape sequences")
        if typed.count(STATUS_BAR_PREFIX) > 1:
            raise AssertionError("large input should not duplicate app screens\n\n" + typed)
        assert_no_legacy_dashboard_chrome(typed, "large input should not show old dashboard chrome")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_failed_retry_switches_to_live_running(binary: Path) -> None:
    session = f"but-smoke-failed-retry-{os.getpid()}"
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-failed-retry-"))
    try:
        start_session(
            session,
            binary,
            state_dir,
            seed_demo="failed",
            select_latest=False,
        )
        wait_for(session, "Tell the browser what to do...", "failed-retry-start-ready")
        tmux_send(session, "Tab", "Enter")
        wait_for(session, "error", "failed-retry-initial")
        initial = capture_after_idle(session, "failed-retry-initial")
        assert_contains(initial, "OpenRouter API key is missing", "failed status should render")
        assert_contains(initial, "Ask a follow-up", "failed composer should stay visible")
        tmux_send(session, "Down", "Down", "Enter")
        running = wait_for(session, "Type to steer the agent", "failed-retry-running")
        visible_running = capture_after_idle(session, "failed-retry-visible", visible_only=True)
        if visible_running.count("Type to steer the agent") > 1:
            raise AssertionError(
                "retry should replace the failure view with one live running viewport\n\n"
                + visible_running
            )
        assert_no_legacy_dashboard_chrome(visible_running, "retry should not show old dashboard chrome")
        assert_not_contains(running, "Choose a different model\n    Retry", "retry should not leave the failure action menu live")
    finally:
        tmux("kill-session", "-t", session, check=False)
        shutil.rmtree(state_dir, ignore_errors=True)


def smoke_completed_plain_output(binary: Path) -> None:
    state_dir = Path(tempfile.mkdtemp(prefix="but-tui-smoke-done-"))
    try:
        result = run(
            [
                str(binary),
                "--state-dir",
                str(state_dir),
                "--seed-demo",
                "long",
                "--select-latest",
                "--agent",
                "none",
                "--browser",
                "Local Chrome",
            ]
        ).stdout
        ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
        (ARTIFACT_DIR / "tui-terminal-smoke-completed-output.txt").write_text(result)
        assert_contains(result, "scroll check line 60", "completed result should print full plain transcript")
        assert_contains(result, "source", "completed result should include source section")
        assert_not_contains(result, "+-", "completed result should use simple section labels")
        assert_no_ansi(result, "completed result should be selectable plain text")
    finally:
        shutil.rmtree(state_dir, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--skip-build", action="store_true", help="reuse target/debug/but")
    args = parser.parse_args()

    if shutil.which("tmux") is None:
        print("tmux is required for real terminal smoke tests", file=sys.stderr)
        return 2

    binary = ROOT / "target" / "debug" / "but" if args.skip_build else build_binary()
    smoke_interactive_terminal(binary)
    smoke_live_subagent_status_bar(binary)
    smoke_ready_resize_does_not_leave_stale_frames(binary)
    smoke_history_selection_emits_native_transcript(binary)
    smoke_tall_terminal_keeps_running_controls_attached_to_content(binary)
    smoke_escape_pauses_running_session(binary)
    smoke_escape_reclaims_prompt_before_output(binary)
    smoke_tab_queues_followup_after_current_turn(binary)
    smoke_enter_previews_followup_after_next_tool(binary)
    smoke_escape_reclaims_pending_active_followup(binary)
    smoke_escape_reclaims_queued_followup(binary)
    smoke_completed_history_uses_native_scrollback(binary)
    smoke_streaming_transcript_scrolls_above_composer(binary)
    smoke_prompt_only_followup_keeps_completed_transcript(binary)
    smoke_short_completed_history_has_live_preview(binary)
    smoke_main_resize_does_not_duplicate_transcript(binary)
    smoke_session_switch_clears_previous_transcript(binary)
    smoke_large_composer_input_is_responsive(binary)
    smoke_failed_retry_switches_to_live_running(binary)
    smoke_completed_plain_output(binary)
    print("tui terminal smoke passed")
    print(f"captures: {ARTIFACT_DIR}/tui-terminal-smoke-*.txt")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

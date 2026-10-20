# Inspect commands before execution

Set `BUT_COMMAND_GUARD` to an executable and its arguments, encoded as a JSON
array. Terminal runs this trusted checker before starting an agent shell process
or sending nonempty input to a running process. Empty `write_stdin` polls do not
invoke the checker.

```sh
export BUT_COMMAND_GUARD='["/trusted/guard-env/bin/python","-I","-X","utf8","/trusted/veltra-command-guard.py"]'
browser
```

On Windows, use JSON-escaped paths or forward slashes:

```powershell
$env:BUT_COMMAND_GUARD = '["C:/trusted/guard-env/Scripts/python.exe","-I","-X","utf8","C:/trusted/veltra-command-guard.py"]'
browser
```

The executable is launched directly, without interpolating the pending command
into a shell script. Configuration is read when the execution manager is created;
an agent's per-command environment does not change it. Keep the checker and its
dependencies outside the agent's writable workspace.

## Checker contract

The checker receives the existing `PreToolUse` JSON format as UTF-8 on stdin:

```json
{
  "hookEventName": "PreToolUse",
  "callId": "call-123",
  "toolName": "exec_command",
  "toolInput": {
    "command": ["sh", "-c", "echo hello"],
    "cwd": "/workspace",
    "env": {}
  }
}
```

For process starts, `env` contains the proposed per-command environment overrides,
not the full inherited environment. The checker must consider these along with
the command and working directory; overrides can change what a command executes.

For `write_stdin`, `command` and `cwd` identify the original process, and
`toolInput` also contains `session_id` and the proposed `chars`. The checker must
return JSON with an explicit boolean decision and exit successfully:

```json
{"continue": true}
```

`{"continue": false}` denies execution. An invalid configuration, failed checker,
missing decision, malformed JSON, output exceeding 8 KiB, or a five-second timeout
also denies execution. Cancellation interrupts a pending check without starting
the command. Invalid configuration and checker failures produce fixed diagnostic
categories on Terminal's stderr; raw checker stdout, stderr, and response reasons
are suppressed because they can contain command secrets. These diagnostics are
not included in the agent's error message. There is no decision cache: each
process start and each nonempty input write is checked again.

Unlike general lifecycle hooks, this security gate requires an explicit allow
decision; a missing or failed check never permits the command.

## Use the local Veltra checker

The optional adapter in
[`scripts/veltra-command-guard.py`](../scripts/veltra-command-guard.py) uses
[`veltra-agent` 0.4.0](https://pypi.org/project/veltra-agent/)'s
command, outbound-port, filesystem-write, and environment-leak inspection
functions with the package's default policy. Install that version in a dedicated
environment and place the adapter in a trusted directory:

```sh
uv venv /trusted/guard-env
uv pip install --python /trusted/guard-env/bin/python veltra-agent==0.4.0
```

Configure `BUT_COMMAND_GUARD` as above. Python's `-I` option prevents imports from
the task's working directory and ignores Python environment overrides. Use
`-X utf8` as shown so Veltra's temporary audit file also supports non-ASCII
commands on Windows. The adapter reads at most 1 MiB of UTF-8 input and inspects without executing the
command. It denies per-command environment overrides because Veltra's text rules
cannot validate their execution effects. Terminal's inherited environment must
therefore be trusted. It uses a temporary directory for Veltra's policy lookup
and audit file so a task-controlled `veltra.config.json`
cannot disable the check and command text is not left in the workspace.

The adapter denies nonempty interactive input because Veltra's checker accepts
complete command strings. Independent fragments can hide a command split across
multiple writes. Submit complete commands with `exec_command` instead; polling
their output remains available.

Veltra 0.4.0 uses deterministic regular-expression rules. These rules can reject
known patterns but do not prove that arbitrary commands are safe or detect every
prompt injection. This gate does not inspect page navigation, Python tool code,
file tools, or commands launched outside Terminal's agent shell tools. Use OS
isolation and restricted credentials when those execution capabilities require
a stronger boundary.

The adapter uses Veltra's published local inspection API and does not send commands
to its hosted service. Malformed input or a checker dependency failure returns a
deny verdict with a nonzero exit status, distinguishing a failed check from an
ordinary policy denial without printing sensitive exception details.

With `BUT_COMMAND_GUARD` configured as above, run the optional end-to-end Rust test:

```sh
cargo test --workspace veltra_guard_integration -- --ignored
```

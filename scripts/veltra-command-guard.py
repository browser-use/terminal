"""Adapt Veltra's local, inspection-only checker to command guard JSON."""

import json
import os
import sys
import tempfile


def main() -> None:
    decision = {"continue": False, "reason": "Veltra command inspection failed."}
    try:
        raw = sys.stdin.buffer.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024:
            raise ValueError("Guard input is too large")
        request = json.loads(raw.decode("utf-8"))
        if request["hookEventName"] != "PreToolUse":
            raise ValueError("Unexpected guard event")
        tool_name = request["toolName"]
        if tool_name not in ("shell", "shell_command", "exec_command", "write_stdin"):
            raise ValueError("Unexpected command tool")
        tool_input = request["toolInput"]
        command = tool_input["command"]
        if (
            not isinstance(command, list)
            or not command
            or not all(type(arg) is str for arg in command)
        ):
            raise ValueError("Expected a command argument list")
        env = tool_input.get("env", {})
        if not isinstance(env, dict) or env:
            # Veltra's command-text rules cannot validate process environment overrides.
            decision["reason"] = (
                "Veltra does not support per-command environment overrides."
            )
        elif tool_name == "write_stdin":
            # Veltra accepts complete commands; separate input chunks can hide one.
            decision["reason"] = (
                "Veltra does not support interactive input; use a complete command."
            )
        else:
            from veltra_agent.config import DEFAULT_CONFIG
            from veltra_agent.guardrail import is_command_safe
            from veltra_agent.network import inspect_network_rules
            from veltra_agent.sandbox import inspect_filesystem_rules
            from veltra_agent.sanitizer import inspect_environment_leak

            # Do not load task-controlled policy files or leave command audit logs there.
            with tempfile.TemporaryDirectory(prefix="browser-use-veltra-") as audit_dir:
                original_dir = os.getcwd()
                try:
                    os.chdir(audit_dir)
                    text = " ".join(command)
                    policy = DEFAULT_CONFIG["guardrails"]
                    checks = (
                        is_command_safe(text),
                        inspect_network_rules(text, policy["allowed_outbound_ports"]),
                        inspect_filesystem_rules(
                            text, policy["enforce_read_only_root"]
                        ),
                        inspect_environment_leak(text),
                    )
                    allowed = all(result[0] is True for result in checks)
                finally:
                    os.chdir(original_dir)
            decision = {
                "continue": allowed is True,
                "reason": "Veltra command policy checked."
                if allowed is True
                else "Veltra denied the command.",
            }
    except Exception:  # noqa: BLE001 - scanner failures must deny without exposing payloads
        decision = {"continue": False, "reason": "Veltra command inspection failed."}
        print(json.dumps(decision))
        raise SystemExit(1) from None
    print(json.dumps(decision))


if __name__ == "__main__":
    main()

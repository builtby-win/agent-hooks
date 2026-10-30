"""Python bindings for the agent-hooks CLI (stdlib only)."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from typing import Any, Literal, Optional, Sequence, TypedDict

__all__ = ["InitResult", "binary_path", "init", "run"]


class HarnessResult(TypedDict, total=False):
    id: str
    status: Literal["installed", "current", "skipped", "failed"]
    error: str


class InitResult(TypedDict):
    path: str
    action: Literal["installed", "upgraded", "kept", "current"]
    version: str
    # Empty when hooks were not requested or no harness was detected.
    harnesses: list[HarnessResult]
    on_path: bool


def binary_path() -> str:
    """Path of the bundled binary, or ``AGENT_HOOKS_BIN`` if set."""
    override = os.environ.get("AGENT_HOOKS_BIN")
    if override:
        return override
    exe = "agent-hooks.exe" if sys.platform == "win32" else "agent-hooks"
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "_bin", exe)
    if not os.path.isfile(path):
        raise FileNotFoundError(
            f"agent-hooks: bundled binary not found at {path}. This install is not "
            "a platform wheel; install a platform-specific build of "
            "builtby-win-agent-hooks or set AGENT_HOOKS_BIN."
        )
    return path


def init(
    *,
    harness: str = "all",
    hooks: bool = True,
    min_version: Optional[str] = None,
    socket: Optional[str] = None,
) -> InitResult:
    """Install the binary at ~/.agent-hooks/bin and register hooks. Idempotent."""
    args = [binary_path(), "self-install", "--json", "--harness", harness]
    if not hooks:
        args.append("--no-hooks")
    if min_version:
        args += ["--min-version", min_version]
    if socket:
        args += ["--socket", socket]
    proc = subprocess.run(args, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(
            f"agent-hooks self-install failed (exit {proc.returncode}): "
            f"{proc.stderr.strip()}"
        )
    lines = proc.stdout.strip().splitlines()
    if not lines:
        raise RuntimeError("agent-hooks self-install produced no output")
    try:
        return json.loads(lines[-1])
    except ValueError as exc:
        raise RuntimeError(
            f"agent-hooks self-install: unparseable result {lines[-1]!r}"
        ) from exc


def run(args: Sequence[str], **subprocess_kwargs: Any) -> int:
    """Run the bundled binary; returns its exit code. stdio is inherited."""
    return subprocess.run([binary_path(), *args], **subprocess_kwargs).returncode

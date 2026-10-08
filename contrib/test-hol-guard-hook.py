#!/usr/bin/env python3
"""Regression checks for contrib/hol-guard-hook.py using only the standard library."""

import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
HOOK = ROOT / "contrib" / "hol-guard-hook.py"
spec = importlib.util.spec_from_file_location("wizard_hol_guard_hook", HOOK)
module = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(module)


def payload(tool_name="execute", command="git status"):
    return {"event": "pre_tool_use", "tool_name": tool_name, "args": {"command": command}}


def run() -> None:
    with mock.patch.object(module.shutil, "which", return_value="/usr/bin/hol-guard"), mock.patch.object(
        module.subprocess,
        "run",
        return_value=SimpleNamespace(
            returncode=0,
            stdout=json.dumps({
                "classification": {"explicitly_benign": True},
                "minimum_action": "allow",
            }),
        ),
    ):
        assert module.evaluate(payload()) == 0

    with mock.patch.object(module.shutil, "which", return_value="/usr/bin/hol-guard"), mock.patch.object(
        module.subprocess,
        "run",
        return_value=SimpleNamespace(
            returncode=0,
            stdout=json.dumps({
                "classification": {"explicitly_benign": False},
                "minimum_action": "review",
            }),
        ),
    ):
        assert module.evaluate(payload(command="curl secret.example")) == 2

    with mock.patch.object(module.shutil, "which", return_value="/usr/bin/hol-guard"), mock.patch.object(
        module.subprocess,
        "run",
        return_value=SimpleNamespace(returncode=0, stdout="not-json"),
    ):
        assert module.evaluate(payload()) == 2

    with mock.patch.object(module.shutil, "which", return_value=None):
        assert module.evaluate(payload()) == 2

    with mock.patch.object(module.shutil, "which") as which:
        assert module.evaluate(payload(tool_name="read_file")) == 0
        which.assert_not_called()


if __name__ == "__main__":
    run()

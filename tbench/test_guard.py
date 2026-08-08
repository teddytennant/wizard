"""Tests for the staleness guard in wizard_agent.

Stdlib only — this is a Rust repo with one Python adapter in it, and the guard is
not worth a pytest dependency and a pyproject to configure it. Run directly:

    python3 tbench/test_guard.py

Harbor is stubbed rather than installed: the guard never touches Harbor, and
requiring `uv tool install harbor` just to check a version comparison would mean
nobody runs this.
"""

from __future__ import annotations

import os
import sys
import tempfile
import types
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))


def _stub_harbor() -> None:
    class _BaseInstalledAgent:
        def __init__(self, *args, **kwargs):
            pass

    modules = {
        "harbor": {},
        "harbor.agents": {},
        "harbor.agents.installed": {},
        "harbor.agents.installed.base": {
            "BaseInstalledAgent": _BaseInstalledAgent,
            "with_prompt_template": lambda fn: fn,
        },
        "harbor.environments": {},
        "harbor.environments.base": {"BaseEnvironment": object},
        "harbor.models": {},
        "harbor.models.agent": {},
        "harbor.models.agent.context": {"AgentContext": object},
    }
    for name, attrs in modules.items():
        module = types.ModuleType(name)
        for key, value in attrs.items():
            setattr(module, key, value)
        sys.modules.setdefault(name, module)


_stub_harbor()

from tbench.wizard_agent import (  # noqa: E402
    ALLOW_STALE_ENV,
    BINARY_ENV,
    SOURCE_ENV,
    WizardAgent,
)


class GuardTestCase(unittest.TestCase):
    """Fixture: a throwaway source tree plus a fake binary that reports a version."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.source = root / "source"
        (self.source / "src").mkdir(parents=True)
        (self.source / "src" / "main.rs").write_text("fn main() {}\n")
        self.manifest = self.source / "Cargo.toml"
        self.manifest.write_text('[package]\nname = "wizard"\nversion = "2.0.0"\n')

        self.binary = root / "wizard"
        self._write_binary("2.0.0")

        # Built after the sources, as a real build would be.
        self._touch(self.binary, newest=True)

        self.agent = object.__new__(WizardAgent)
        self._saved_env = {
            k: os.environ.get(k) for k in (SOURCE_ENV, BINARY_ENV, ALLOW_STALE_ENV)
        }
        for key in self._saved_env:
            os.environ.pop(key, None)
        os.environ[SOURCE_ENV] = str(self.source)
        os.environ[BINARY_ENV] = str(self.binary)

    def tearDown(self) -> None:
        for key, value in self._saved_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        self.tmp.cleanup()

    def _write_binary(self, version: str) -> None:
        self.binary.write_text(f'#!/bin/sh\necho "wizard {version}"\n')
        self.binary.chmod(0o755)

    def _touch(self, path: Path, newest: bool) -> None:
        """Put `path` clearly after (or before) everything else in the fixture.

        "Everything else" has to include the binary, not just the source tree: the
        guard compares with a strict `>`, so a source file merely *tying* the
        binary's mtime is correctly treated as not-newer.
        """
        others = [
            p.stat().st_mtime
            for p in [*self.source.rglob("*"), self.binary]
            if p != path and p.is_file()
        ]
        base = max(others) if others else 0
        os.utime(path, (base + 100, base + 100) if newest else (base - 100, base - 100))

    # --- version comparison ------------------------------------------------

    def test_elided_trailing_zero_is_the_same_version(self):
        # `wizard --version` prints "1.1" where Cargo.toml carries "1.1.0".
        for printed, manifest in (("1.1", "1.1.0"), ("2.0", "2.0.0"), ("3", "3.0.0")):
            with self.subTest(printed=printed):
                self.assertTrue(WizardAgent._same_version(printed, manifest))

    def test_different_versions_are_not_the_same(self):
        for a, b in (("1.1.0", "2.0.0"), ("1.10.0", "1.1.0"), ("2.0.0", "2.0.0-rc1")):
            with self.subTest(a=a, b=b):
                self.assertFalse(WizardAgent._same_version(a, b))

    def test_non_numeric_versions_fall_back_to_string_equality(self):
        self.assertTrue(WizardAgent._same_version("dev", "dev"))
        self.assertFalse(WizardAgent._same_version("dev", "1.1.0"))

    # --- the guard ---------------------------------------------------------

    def test_matching_binary_and_source_pass(self):
        self.assertEqual(self.agent._binary(), self.binary)

    def test_version_mismatch_is_refused(self):
        self._write_binary("1.1")
        self._touch(self.binary, newest=True)
        with self.assertRaises(RuntimeError) as caught:
            self.agent._binary()
        message = str(caught.exception)
        self.assertIn("binary is Wizard 1.1", message)
        self.assertIn("2.0.0", message)
        # The error has to carry the fix, not just the complaint.
        self.assertIn("docker build", message)
        self.assertIn(str(self.source), message)

    def test_source_edited_after_build_is_refused(self):
        edited = self.source / "src" / "main.rs"
        self._touch(edited, newest=True)
        with self.assertRaises(RuntimeError) as caught:
            self.agent._binary()
        self.assertIn("src/main.rs", str(caught.exception))

    def test_allow_stale_downgrades_to_a_warning(self):
        self._write_binary("1.1")
        self._touch(self.binary, newest=True)
        os.environ[ALLOW_STALE_ENV] = "1"
        with self.assertLogs("tbench.wizard_agent", level="WARNING") as logs:
            self.assertEqual(self.agent._binary(), self.binary)
        self.assertIn("stale", "\n".join(logs.output).lower())

    def test_unreadable_manifest_is_refused(self):
        self.manifest.unlink()
        with self.assertRaises(RuntimeError) as caught:
            self.agent._binary()
        self.assertIn("no readable Cargo.toml", str(caught.exception))

    def test_missing_binary_names_the_build_command(self):
        self.binary.unlink()
        with self.assertRaises(FileNotFoundError) as caught:
            self.agent._binary()
        self.assertIn("docker build", str(caught.exception))


if __name__ == "__main__":
    unittest.main(verbosity=2)

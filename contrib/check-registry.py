#!/usr/bin/env python3
"""Validate the in-tree skills/tools registry and keep registry.json honest.

Walks registry/{skills,tools}/<author>/<name>/, checks each manifest.toml
against Manifest::validate rules, verifies the artifact sha256, and
compares the generated index to registry/registry.json so the file
cannot drift from the tree.

  contrib/check-registry.py          verify (default)
  contrib/check-registry.py --write  regenerate registry/registry.json
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tomllib
from datetime import datetime, timezone
from pathlib import Path

INDEX_VERSION = 1
INDEX_FILE = Path("registry/registry.json")
REGISTRY_ROOT = Path("registry")
KINDS = {"skill": "skills", "tool": "tools"}
DEFAULT_ARTIFACT = {"skill": "SKILL.md", "tool": "tool.lua"}
BUNDLED_SKILLS = {"coding", "evolve"}
NATIVE_TOOLS = {
    "read_file", "write_file", "edit_file", "list_files", "search_files",
    "execute", "git_status", "git_diff", "memory", "todo", "manual",
    "web_fetch", "web_search", "x_search", "generate_image", "task_output",
    "task_kill", "subagent_status", "subagent_kill", "run_command",
    "compact", "computer", "run_code",
}
RFC3339_RE = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$"
)

def fail(msg):
    print("error:", msg, file=sys.stderr)


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def expected_digest(checksum):
    return checksum.strip().removeprefix("sha256:").strip().lower()


def validate_segment(field, value):
    errors = []
    if not isinstance(value, str) or not value:
        return ["manifest has an empty %s" % field]
    if len(value) > 64:
        errors.append("manifest %s is longer than 64 characters" % field)
    if not value.isascii() or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.-" for c in value):
        errors.append("manifest %s has illegal characters" % field)
    if value.startswith(".") or ".." in value:
        errors.append("manifest %s may not start with a dot or contain .." % field)
    return errors


def artifact_name(manifest):
    named = manifest.get("artifact")
    if isinstance(named, str) and named.strip():
        return named
    return DEFAULT_ARTIFACT[manifest["kind"]]


def validate_manifest(manifest, rel_dir):
    errors = []
    kind = manifest.get("kind")
    if kind not in KINDS:
        return ["%s: kind must be skill or tool" % rel_dir]
    errors.extend(validate_segment("name", manifest.get("name")))
    errors.extend(validate_segment("author", manifest.get("author")))
    version = manifest.get("version")
    if not isinstance(version, str) or not version.strip():
        errors.append("%s: empty version" % rel_dir)
    description = manifest.get("description")
    if not isinstance(description, str) or not description.strip():
        errors.append("%s: empty description" % rel_dir)
    digest = expected_digest(str(manifest.get("checksum", "")))
    if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
        errors.append("%s: checksum is not a sha256 hex digest" % rel_dir)
    art = artifact_name(manifest)
    if "/" in art or "\\" in art or art == "..":
        errors.append("%s: artifact has a path in it: %s" % (rel_dir, art))
    caps = manifest.get("capabilities") or []
    if kind == "skill" and caps:
        errors.append("%s: a skill cannot be granted capabilities" % rel_dir)
    if kind == "tool":
        if not art.lower().endswith(".lua"):
            errors.append("%s: registry tools must be LuaJIT scripts" % rel_dir)
        if len(caps) != len(set(caps)):
            errors.append("%s: duplicate capability" % rel_dir)
    name = manifest.get("name")
    if kind == "skill" and name in BUNDLED_SKILLS:
        errors.append("%s: name %s is reserved (bundled skill)" % (rel_dir, name))
    if name in NATIVE_TOOLS:
        errors.append("%s: name %s is a native tool name" % (rel_dir, name))
    return errors


def iter_entries():
    errors = []
    entries = []
    for kind, dirname in KINDS.items():
        root = REGISTRY_ROOT / dirname
        if not root.is_dir():
            continue
        for manifest_path in sorted(root.glob("*/*/manifest.toml")):
            rel_dir = manifest_path.parent.relative_to(REGISTRY_ROOT).as_posix()
            try:
                with manifest_path.open("rb") as fh:
                    manifest = tomllib.load(fh)
            except Exception as exc:
                errors.append("%s: cannot parse manifest.toml: %s" % (rel_dir, exc))
                continue
            errors.extend(validate_manifest(manifest, rel_dir))
            if manifest.get("kind") != kind:
                errors.append("%s: kind %r does not match directory %s" % (rel_dir, manifest.get("kind"), dirname))
            expected_path = "%s/%s/%s" % (dirname, manifest.get("author"), manifest.get("name"))
            if rel_dir != expected_path:
                errors.append("%s: path does not match kind/author/name (%s)" % (rel_dir, expected_path))
            art = artifact_name(manifest)
            art_path = manifest_path.parent / art
            if not art_path.is_file():
                errors.append("%s: missing artifact %s" % (rel_dir, art))
                continue
            digest = sha256_hex(art_path.read_bytes())
            want = expected_digest(str(manifest.get("checksum", "")))
            if want and digest != want:
                errors.append("%s: checksum mismatch: manifest has %s, artifact hashes to %s" % (rel_dir, want, digest))
            entry = {
                "name": manifest.get("name"),
                "version": manifest.get("version"),
                "author": manifest.get("author"),
                "description": manifest.get("description"),
                "tags": list(manifest.get("tags") or []),
                "kind": manifest.get("kind"),
                "checksum": manifest.get("checksum"),
                "capabilities": list(manifest.get("capabilities") or []),
                "path": rel_dir,
            }
            if manifest.get("artifact"):
                entry["artifact"] = manifest["artifact"]
            if manifest.get("timeout_secs") is not None:
                entry["timeout_secs"] = manifest["timeout_secs"]
            if manifest.get("parameters") is not None:
                entry["parameters"] = manifest["parameters"]
            entries.append(entry)
    entries.sort(key=lambda e: (e.get("kind") or "", e.get("name") or ""))
    return entries, errors


def normalize_entries(entries):
    out = []
    for entry in entries:
        item = {
            "name": entry.get("name"),
            "version": entry.get("version"),
            "author": entry.get("author"),
            "description": entry.get("description"),
            "tags": list(entry.get("tags") or []),
            "kind": entry.get("kind"),
            "checksum": entry.get("checksum"),
            "capabilities": list(entry.get("capabilities") or []),
            "path": entry.get("path"),
        }
        if entry.get("artifact"):
            item["artifact"] = entry["artifact"]
        if entry.get("timeout_secs") is not None:
            item["timeout_secs"] = entry["timeout_secs"]
        if entry.get("parameters") is not None:
            item["parameters"] = entry["parameters"]
        out.append(item)
    out.sort(key=lambda e: (e.get("kind") or "", e.get("name") or ""))
    return out


def write_index(entries):
    generated_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    INDEX_FILE.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "version": INDEX_VERSION,
        "generated_at": generated_at,
        "entries": entries,
    }
    INDEX_FILE.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    print("wrote %s (%d entries)" % (INDEX_FILE, len(entries)))


def check_index(entries):
    errors = []
    if not INDEX_FILE.is_file():
        return ["missing %s; run contrib/check-registry.py --write" % INDEX_FILE]
    try:
        index = json.loads(INDEX_FILE.read_text(encoding="utf-8"))
    except Exception as exc:
        return ["%s is not valid JSON: %s" % (INDEX_FILE, exc)]
    if index.get("version") != INDEX_VERSION:
        errors.append("%s version is %r, expected %s" % (INDEX_FILE, index.get("version"), INDEX_VERSION))
    generated_at = index.get("generated_at")
    if not isinstance(generated_at, str) or not RFC3339_RE.match(generated_at):
        errors.append("%s generated_at is not RFC 3339: %r" % (INDEX_FILE, generated_at))
    published = normalize_entries(index.get("entries") or [])
    expected = normalize_entries(entries)
    if published != expected:
        errors.append("%s entries do not match the tree; run contrib/check-registry.py --write" % INDEX_FILE)
        have = {(e["kind"], e["name"], e["path"], e["checksum"]) for e in published}
        want = {(e["kind"], e["name"], e["path"], e["checksum"]) for e in expected}
        for row in sorted(want - have):
            errors.append("  missing from index: %s/%s at %s" % (row[0], row[1], row[2]))
        for row in sorted(have - want):
            errors.append("  extra in index: %s/%s at %s" % (row[0], row[1], row[2]))
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="regenerate registry/registry.json")
    args = parser.parse_args(argv)

    repo = Path(__file__).resolve().parents[1]
    # Run from the repo root so relative paths stay short in errors.
    import os
    os.chdir(repo)

    entries, errors = iter_entries()
    if not entries and not errors:
        errors.append("no registry entries found under registry/")
    if args.write:
        if errors:
            for err in errors:
                fail(err)
            return 1
        write_index(entries)
        return 0
    errors.extend(check_index(entries))
    if errors:
        for err in errors:
            fail(err)
        return 1
    print("registry ok: %d entries, index matches the tree" % len(entries))
    return 0


if __name__ == "__main__":
    sys.exit(main())

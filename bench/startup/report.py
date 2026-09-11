#!/usr/bin/env python3
"""Fold the per-agent JSON from run.sh into results.md and results.json."""
import datetime
import json
import os
import platform
import subprocess
import sys

out_dir, md_path, json_path = sys.argv[1:4]
ORDER = ["wizard", "claude", "codex", "opencode", "crush", "goose", "aider"]
LABEL = {"wizard": "wizard", "claude": "Claude Code", "codex": "Codex CLI", "opencode": "OpenCode",
         "crush": "Crush", "goose": "Goose", "aider": "Aider"}

rows = []
for name in ORDER:
    p = os.path.join(out_dir, name + ".json")
    if os.path.exists(p):
        rows.append(json.load(open(p)))


def mb(b):
    return "" if b in (None, 0) else f"{b / 1e6:.0f}"


def ms(x):
    return "" if x is None else f"{x:.0f}"


def version(d):
    v = d.get("version_output") or []
    return v[0].strip() if v else ""


def sh(cmd):
    try:
        return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=10).stdout.strip()
    except Exception:
        return ""


lines = ["# Startup benchmark", ""]
lines.append(f"Measured {datetime.date.today().isoformat()} on {platform.machine()}, {sh('nproc')} CPUs, "
             f"kernel {platform.release()}, {sh('docker --version')}. Container network: "
             f"{rows[0].get('network', '?') if rows else '?'}. One ubuntu:24.04 container per agent, "
             f"{rows[0].get('runs', '?') if rows else '?'} pty starts each.")
lines.append("")
lines.append("| agent | version | install | bundle | cold start median | p90 | first run | `--version` | RSS at prompt | peak RSS |")
lines.append("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|")
for d in rows:
    if d.get("install_failed"):
        lines.append(f"| {LABEL[d['name']]} | | install failed | | | | | | | |")
        continue
    lines.append("| {} | {} | {} MB | {} MB | {} ms | {} ms | {} ms | {} ms | {} MB | {} MB |".format(
        LABEL[d["name"]], version(d), mb(d.get("install_bytes")), mb(d.get("bundle_bytes")),
        ms(d.get("prompt_ms_median")), ms(d.get("prompt_ms_p90")), ms(d.get("prompt_ms_first")),
        ms(d.get("version_ms_median")), mb(d.get("rss_bytes_median")), mb(d.get("hwm_bytes_median"))))
lines.append("")
lines.append("install: bytes the install added on top of the shared base image (sum of its docker layers), "
             "runtimes included. bundle: the agent's own artifact (binary, npm package or venv). "
             "cold start: fork+exec to the first frame containing the prompt marker, over a pty that answers "
             "terminal queries like xterm. first run: the first of the starts, before the page cache is warm. "
             "RSS: VmRSS summed over the process tree 3 s after the prompt; peak is VmHWM summed the same way.")
lines.append("")
lines.append("## Per agent")
lines.append("")
for d in rows:
    lines.append(f"### {LABEL[d['name']]}")
    lines.append("")
    if d.get("install_failed"):
        lines.append("Install failed; see the build log.")
        lines.append("")
        continue
    lines.append(f"- command: `{d['cmd']}`, prompt marker: `{d['marker']}`")
    if d.get("failed_runs"):
        lines.append(f"- {d['failed_runs']} of {d['runs']} runs never showed the prompt within the timeout")
    lines.append(f"- runs (ms): {', '.join(str(int(x)) for x in d.get('prompt_ms', []))}")
    if d.get("procs"):
        lines.append(f"- processes at the prompt: {', '.join(d['procs'])}")
    if d.get("terminal_queries_answered"):
        qs = sorted(set(d["terminal_queries_answered"]))
        lines.append("- terminal queries answered: " + ", ".join("`" + q.replace("\x1b", "ESC") + "`" for q in qs))
    if d.get("note"):
        lines.append(f"- {d['note']}")
    lines.append("")

open(md_path, "w").write("\n".join(lines) + "\n")
json.dump({"generated": datetime.datetime.now().isoformat(timespec="seconds"), "agents": rows},
          open(json_path, "w"), indent=2)

#!/usr/bin/env python3
"""Fold the per-agent JSON from run.sh into a results .md and .json.

  report.py OUT_DIR results.md results.json [agent ...]

Only the named agents are reported (all seven when none are named), so a
subset run never picks up stale rows from an earlier full run.
"""
import datetime
import json
import os
import sys
import time

out_dir, md_path, json_path = sys.argv[1:4]
ORDER = ["wizard", "claude", "codex", "opencode", "crush", "goose", "aider"]
names = sys.argv[4:] or ORDER
LABEL = {"wizard": "wizard", "claude": "Claude Code", "codex": "Codex CLI", "opencode": "OpenCode",
         "crush": "Crush", "goose": "Goose", "aider": "Aider"}

rows = []
for name in ORDER:
    p = os.path.join(out_dir, name + ".json")
    if name in names and os.path.exists(p):
        rows.append(json.load(open(p)))

host = {}
hp = os.path.join(out_dir, "host.json")
if os.path.exists(hp):
    host = json.load(open(hp))


def mb(b):
    return "n/a" if b in (None, 0) else f"{b / 1e6:.0f} MB"


def ms(x):
    return "n/a" if x is None else f"{x:.0f} ms"


def version(d):
    v = d.get("version_output") or []
    return v[0].strip() if v else ""


first = rows[0] if rows else {}
# SOURCE_DATE_EPOCH lets a report be regenerated from an old run's JSON
# without moving its date.
now = datetime.datetime.fromtimestamp(int(os.environ.get("SOURCE_DATE_EPOCH") or time.time()))
kitty_answered = any(q.startswith("\x1b_G") for d in rows for q in d.get("terminal_queries_answered") or [])
lines = ["# Startup benchmark", ""]
lines.append(f"Measured {now.date().isoformat()} on {host.get('cpu_model', '?')}, "
             f"{host.get('nproc', '?')} CPUs, kernel {host.get('kernel', '?')}, {host.get('docker', '?')}. "
             f"One ubuntu:24.04 container per agent, {first.get('runs', '?')} pty starts each, "
             f"container network {first.get('network', '?')}.")
lines.append("")
rule = first.get("rule")
if rule:
    lines.append(f"Rule for every agent: {rule} What each one was told is under its row.")
else:
    lines.append("This run predates the single phone-home rule: what each agent was told is under its row, "
                 "and Claude Code, Codex and OpenCode were measured with their update, telemetry and policy "
                 "fetches still on.")
lines.append("")
lines.append("| agent | version | command | install | bundle | warm start (median) | warm p90 | cold start | `--version` | RSS | PSS | peak RSS | CPU at 3 s |")
lines.append("|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
for d in rows:
    if d.get("install_failed"):
        lines.append(f"| {LABEL[d['name']]} | | | install failed | | | | | | | | | |")
        continue
    if d.get("measure_failed"):
        lines.append(f"| {LABEL[d['name']]} | | `{d.get('cmd', '')}` | {mb(d.get('install_bytes'))} | | measurement failed | | | | | | | |")
        continue
    lines.append("| {} | {} | `{}` | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |".format(
        LABEL[d["name"]], version(d), d["cmd"], mb(d.get("install_bytes")), mb(d.get("bundle_bytes")),
        ms(d.get("warm_ms_median")), ms(d.get("warm_ms_p90")), ms(d.get("cold_ms")),
        ms(d.get("version_ms_median")), mb(d.get("rss_bytes_median")), mb(d.get("pss_bytes_median")),
        mb(d.get("hwm_bytes_median")), ms(d.get("cpu_ms_median"))))
lines.append("")
kitty = (", and says yes to the kitty graphics probe" if kitty_answered
         else "; no kitty graphics probe was answered in this run")
lines.append("install: bytes the install added on top of the shared base image (sum of its docker layers), "
             "runtimes included (Node for the npm install of Codex, uv's CPython for Aider). "
             "bundle: the agent's own artifact (a binary, the npm package, the uv tool venv). "
             "warm start: fork+exec to the first frame that contains the agent's prompt marker, median and p90 "
             f"of starts 2 to {first.get('runs', 'N')} in one container, page cache warm. "
             "cold start: start 1 in that container, before anything is in the page cache. "
             "All markers are the first frame with an input line, not a fully loaded agent: wizard's status bar "
             "still says connecting tools, Codex still says model loading, Goose is still loading extensions. "
             f"The pty answers DA1 (VT220, no sixel), DSR, cell size, DECRQM and OSC color queries{kitty}. "
             "RSS/PSS: VmRSS and Pss summed over the process tree 3 s after the prompt; peak is VmHWM summed "
             "the same way; CPU is utime+stime over the same tree, so a slow wall time can be read as work or as waiting.")
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
    if d.get("measure_failed"):
        lines.append("Measurement failed; see run.sh output.")
        lines.append("")
        continue
    lines.append(f"- command: `{d['cmd']}`, prompt marker: `{d['marker']}`")
    if d.get("env"):
        lines.append(f"- environment: `{d['env']}`")
    if d.get("failed_runs"):
        lines.append(f"- {d['failed_runs']} of {d['runs']} runs never showed the prompt within the timeout")
    lines.append(f"- cold {ms(d.get('cold_ms'))}, warm (ms): {', '.join(str(int(x)) for x in d.get('warm_ms', []))}")
    if d.get("procs"):
        lines.append(f"- processes at the prompt: {', '.join(d['procs'])}")
    if d.get("terminal_queries_answered"):
        qs = sorted(set(d["terminal_queries_answered"]))
        lines.append("- terminal queries answered: " + ", ".join("`" + q.replace("\x1b", "ESC") + "`" for q in qs))
    if d.get("note"):
        lines.append(f"- {d['note']}")
    lines.append("")
lines.append("## Host")
lines.append("")
if not host:
    lines.append("Host state was not recorded for this run; the load guard and this section came later.")
    lines.append("")
else:
    swap = host.get("swap_used_bytes") or 0
    lines.append(f"Load average at start {' '.join(host.get('loadavg', []))} on {host.get('nproc')} CPUs, "
                 f"PSI cpu `{host.get('psi_cpu_some', '')}`, "
                 f"{(host.get('mem_available_bytes') or 0) / 1e9:.1f} GB available of "
                 f"{(host.get('mem_total_bytes') or 0) / 1e9:.1f} GB, swap in use {swap / 1e9:.1f} GB, "
                 f"governor {host.get('governor') or 'n/a'}, RAPL package cap "
                 f"{host.get('rapl_cap_w') if host.get('rapl_cap_w') is not None else 'n/a'} W. "
                 "run.sh refuses to start above load nproc/4 or under 8 GB available unless LOAD_OK=1. "
                 + ("Page cache dropped before each agent's run 1 (DROP_CACHES=1)."
                    if host.get("drop_caches") else "Page cache not dropped: run 1 is cold only if the image layer had left it."))
    lines.append("")

open(md_path, "w").write("\n".join(lines) + "\n")
json.dump({"generated": now.isoformat(timespec="seconds"), "host": host, "agents": rows},
          open(json_path, "w"), indent=2)

#!/usr/bin/env python3
"""Print the host facts a reader needs to judge the numbers, as JSON."""
import json
import os
import platform
import subprocess


def read(path, default=""):
    try:
        return open(path).read().strip()
    except OSError:
        return default


def sh(cmd):
    try:
        return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=10).stdout.strip()
    except Exception:
        return ""


mem = {}
for line in read("/proc/meminfo").splitlines():
    k, _, v = line.partition(":")
    mem[k] = int(v.split()[0]) * 1024 if v.split() else 0

cpu_model = ""
for line in read("/proc/cpuinfo").splitlines():
    if line.startswith("model name"):
        cpu_model = line.split(":", 1)[1].strip()
        break

rapl = read("/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw")
psi = read("/proc/pressure/cpu").splitlines()
psi_some = psi[0] if psi else ""

info = {
    "cpu_model": cpu_model,
    "nproc": os.cpu_count(),
    "governor": read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
    "rapl_cap_w": int(rapl) / 1e6 if rapl.isdigit() else None,
    "kernel": platform.release(),
    "loadavg": read("/proc/loadavg").split()[:3],
    "psi_cpu_some": psi_some,
    "mem_total_bytes": mem.get("MemTotal"),
    "mem_available_bytes": mem.get("MemAvailable"),
    "swap_used_bytes": (mem.get("SwapTotal", 0) - mem.get("SwapFree", 0)),
    "docker": sh("docker --version"),
    "drop_caches": bool(os.environ.get("DROP_CACHES")),
}
print(json.dumps(info, indent=2))

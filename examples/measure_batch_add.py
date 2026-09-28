#!/usr/bin/env python3
"""Cross-platform resource and process tree measurement for batch_add benchmark.

Works across Linux, macOS, and Windows without external dependencies.
If `psutil` is installed, it can optionally use it; otherwise it relies on
platform-native standard library facilities (ctypes on Windows, /proc on Linux,
ps on macOS).
"""

import argparse
import ctypes
import datetime
import getpass
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent


def find_binary() -> Path:
    ext = ".exe" if sys.platform == "win32" else ""
    candidates = [
        REPO_ROOT / f"target/release/examples/batch_add{ext}",
        REPO_ROOT / f"target/debug/examples/batch_add{ext}",
    ]
    for c in candidates:
        if c.exists():
            return c
    raise FileNotFoundError(
        "Build the example first: cargo build -p agentpools-acp --example batch_add --all-features"
    )


def sample_tree_windows(root_pid: int) -> tuple[int, int, int, float, list[str]]:
    """Sample descendant process tree on Windows using ctypes (kernel32 & psapi)."""
    # Toolhelp32 structures
    TH32CS_SNAPPROCESS = 0x00000002

    class PROCESSENTRY32(ctypes.Structure):
        _fields_ = [
            ("dwSize", ctypes.c_ulong),
            ("cntUsage", ctypes.c_ulong),
            ("th32ProcessID", ctypes.c_ulong),
            ("th32DefaultHeapID", ctypes.c_void_p),
            ("th32ModuleID", ctypes.c_ulong),
            ("cntThreads", ctypes.c_ulong),
            ("th32ParentProcessID", ctypes.c_ulong),
            ("pcPriClassBase", ctypes.c_long),
            ("dwFlags", ctypes.c_ulong),
            ("szExeFile", ctypes.c_char * 260),
        ]

    kernel32 = ctypes.windll.kernel32
    psapi = ctypes.windll.psapi

    snapshot = kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    if snapshot == -1:
        return 0, 0, 0, 0.0, []

    entry = PROCESSENTRY32()
    entry.dwSize = ctypes.sizeof(PROCESSENTRY32)

    children: dict[int, list[int]] = {}
    names: dict[int, str] = {}

    success = kernel32.Process32First(snapshot, ctypes.byref(entry))
    while success:
        pid = entry.th32ProcessID
        ppid = entry.th32ParentProcessID
        name = entry.szExeFile.decode("latin1", errors="ignore").rstrip("\x00")
        children.setdefault(ppid, []).append(pid)
        names[pid] = name
        success = kernel32.Process32Next(snapshot, ctypes.byref(entry))

    kernel32.CloseHandle(snapshot)

    # Collect tree
    tree_pids: set[int] = {root_pid}
    queue = [root_pid]
    while queue:
        curr = queue.pop(0)
        for child_pid in children.get(curr, []):
            if child_pid not in tree_pids:
                tree_pids.add(child_pid)
                queue.append(child_pid)

    total_working_set = 0
    total_private = 0
    total_cpu_seconds = 0.0
    process_names = []

    PROCESS_QUERY_INFORMATION = 0x0400
    PROCESS_VM_READ = 0x0010

    class PROCESS_MEMORY_COUNTERS_EX(ctypes.Structure):
        _fields_ = [
            ("cb", ctypes.c_ulong),
            ("PageFaultCount", ctypes.c_ulong),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
            ("PrivateUsage", ctypes.c_size_t),
        ]

    class FILETIME(ctypes.Structure):
        _fields_ = [("dwLowDateTime", ctypes.c_uint32), ("dwHighDateTime", ctypes.c_uint32)]

    for pid in tree_pids:
        handle = kernel32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)
        if not handle:
            continue
        try:
            mem = PROCESS_MEMORY_COUNTERS_EX()
            mem.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS_EX)
            if psapi.GetProcessMemoryInfo(handle, ctypes.byref(mem), mem.cb):
                total_working_set += mem.WorkingSetSize
                total_private += mem.PrivateUsage

            creation = FILETIME()
            exit_time = FILETIME()
            kernel = FILETIME()
            user = FILETIME()
            if kernel32.GetProcessTimes(
                handle,
                ctypes.byref(creation),
                ctypes.byref(exit_time),
                ctypes.byref(kernel),
                ctypes.byref(user),
            ):
                kt = (kernel.dwHighDateTime << 32) + kernel.dwLowDateTime
                ut = (user.dwHighDateTime << 32) + user.dwLowDateTime
                total_cpu_seconds += (kt + ut) / 10_000_000.0

            if pid in names:
                process_names.append(names[pid])
        finally:
            kernel32.CloseHandle(handle)

    return len(tree_pids), total_working_set, total_private, total_cpu_seconds, sorted(process_names)


def sample_tree_linux(root_pid: int) -> tuple[int, int, int, float, list[str]]:
    """Sample descendant process tree on Linux via /proc."""
    # Map ppid -> children
    children: dict[int, list[int]] = {}
    names: dict[int, str] = {}
    rss_map: dict[int, int] = {}
    cpu_map: dict[int, float] = {}

    clk_tck = os.sysconf("SC_CLK_TCK") if hasattr(os, "sysconf") else 100

    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            stat_file = entry / "stat"
            if not stat_file.exists():
                continue
            content = stat_file.read_text()
            # Format: pid (comm) state ppid ... utime(14) stime(15)
            rparen = content.rfind(")")
            comm = content[content.find("(") + 1 : rparen]
            parts = content[rparen + 2 :].split()
            ppid = int(parts[1])
            utime = int(parts[11])
            stime = int(parts[12])
            rss_pages = int(parts[21])

            children.setdefault(ppid, []).append(pid)
            names[pid] = comm
            rss_map[pid] = rss_pages * 4096
            cpu_map[pid] = (utime + stime) / clk_tck
        except (OSError, IndexError, ValueError):
            continue

    tree_pids: set[int] = {root_pid}
    queue = [root_pid]
    while queue:
        curr = queue.pop(0)
        for c in children.get(curr, []):
            if c not in tree_pids:
                tree_pids.add(c)
                queue.append(c)

    total_rss = sum(rss_map.get(pid, 0) for pid in tree_pids)
    total_cpu = sum(cpu_map.get(pid, 0.0) for pid in tree_pids)
    process_names = sorted([names.get(pid, "unknown") for pid in tree_pids])
    return len(tree_pids), total_rss, total_rss, total_cpu, process_names


def sample_tree_generic(root_pid: int) -> tuple[int, int, int, float, list[str]]:
    """Generic fallback for macOS/BSD via ps."""
    try:
        out = subprocess.check_output(
            ["ps", "-Ao", "pid,ppid,rss,comm"], universal_newlines=True, stderr=subprocess.DEVNULL
        )
    except Exception:
        return 1, 0, 0, 0.0, []

    children: dict[int, list[int]] = {}
    names: dict[int, str] = {}
    rss_map: dict[int, int] = {}

    for line in out.splitlines()[1:]:
        parts = line.strip().split(None, 3)
        if len(parts) < 4:
            continue
        try:
            pid = int(parts[0])
            ppid = int(parts[1])
            rss_kb = int(parts[2])
            comm = parts[3]
            children.setdefault(ppid, []).append(pid)
            names[pid] = Path(comm).name
            rss_map[pid] = rss_kb * 1024
        except ValueError:
            continue

    tree_pids: set[int] = {root_pid}
    queue = [root_pid]
    while queue:
        curr = queue.pop(0)
        for c in children.get(curr, []):
            if c not in tree_pids:
                tree_pids.add(c)
                queue.append(c)

    total_rss = sum(rss_map.get(pid, 0) for pid in tree_pids)
    process_names = sorted([names.get(pid, "unknown") for pid in tree_pids])
    return len(tree_pids), total_rss, total_rss, 0.0, process_names


def sample_process_tree(root_pid: int) -> tuple[int, int, int, float, list[str]]:
    if sys.platform == "win32":
        return sample_tree_windows(root_pid)
    elif sys.platform.startswith("linux"):
        return sample_tree_linux(root_pid)
    else:
        return sample_tree_generic(root_pid)


def main():
    parser = argparse.ArgumentParser(description="Cross-platform measure_batch_add benchmark sampler")
    parser.add_argument("--mode", required=True, choices=["mock", "codex", "codex-shared"])
    parser.add_argument("--detailed-process-trace", action="store_true")
    args = parser.parse_args()

    binary = find_binary()

    codex_cli = os.environ.get("CODEX_PATH")
    if args.mode in ("codex", "codex-shared"):
        if "CODEX_ACP_ENTRY" not in os.environ:
            sys.exit("Error: Set CODEX_ACP_ENTRY to the installed codex-acp dist/index.js.")
        if not codex_cli:
            codex_cli = shutil.which("codex")
        if not codex_cli:
            sys.exit("Error: Cannot find codex in PATH. Set CODEX_PATH.")

        # Check login
        try:
            res = subprocess.run([codex_cli, "login", "status"], capture_output=True, text=True)
            if "Logged in" not in res.stdout:
                sys.exit(f"Error: Codex CLI is not logged in: {res.stdout.strip()}")
        except Exception as e:
            sys.exit(f"Error checking codex login status: {e}")

    output_root = REPO_ROOT / "target/agentpools-batch-add-resource"
    output_root.mkdir(parents=True, exist_ok=True)
    timestamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    run_name = f"{args.mode}-{timestamp}"

    stdout_path = output_root / f"{run_name}.stdout.txt"
    stderr_path = output_root / f"{run_name}.stderr.txt"
    summary_path = output_root / f"{run_name}.resources.json"

    peak_process_count = 0
    peak_working_set = 0
    peak_private_bytes = 0
    peak_names = []
    max_cpu_seconds = 0.0

    start_time = time.perf_counter()
    with open(stdout_path, "wb") as stdout_f, open(stderr_path, "wb") as stderr_f:
        proc = subprocess.Popen(
            [str(binary), f"--{args.mode}"],
            cwd=str(REPO_ROOT),
            stdout=stdout_f,
            stderr=stderr_f,
        )

        while True:
            poll_ret = proc.poll()
            count, ws, priv, cpu_sec, names = sample_process_tree(proc.pid)
            if count > peak_process_count:
                peak_process_count = count
            if ws > peak_working_set:
                peak_working_set = ws
                peak_names = names
            if priv > peak_private_bytes:
                peak_private_bytes = priv
            if cpu_sec > max_cpu_seconds:
                max_cpu_seconds = cpu_sec

            if poll_ret is not None:
                break
            time.sleep(0.25)

    wall_ms = int((time.perf_counter() - start_time) * 1000)

    summary = {
        "mode": args.mode,
        "user": getpass.getuser(),
        "codex_cli": codex_cli,
        "exit_code": proc.returncode,
        "wall_ms": wall_ms,
        "peak_process_count": peak_process_count,
        "peak_working_set_mb": round(peak_working_set / (1024 * 1024), 1),
        "peak_private_mb": round(peak_private_bytes / (1024 * 1024), 1),
        "observed_cpu_seconds": round(max_cpu_seconds, 1),
        "process_names_at_peak": peak_names,
        "stdout": str(stdout_path),
        "stderr": str(stderr_path),
    }

    summary_path.write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary, indent=2))
    sys.exit(proc.returncode)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""A guided performance run of a live cnde session.

Walks through scenarios (press Enter as each one ends), sampling every
process's CPU, memory and GPU use four times a second, and has the
compositor record where its own time goes (its `perf` module).

    python3 tools/perf/sample.py [--out DIR] [--only NAME ...] [--list]

Writes to DIR (default perf-runs/<time>):
    scenarios.csv   name,start_ms,end_ms (a row with no end: started, then the run was cut short)
    processes.csv   unix_ms,pid,name,cpu_pct,rss_kb,pss_kb,gpu_pct,vram_kb,gtt_kb
    system.csv      unix_ms,cpu_pct,mem_used_kb,gpu_busy_pct,vram_used_kb
    compositor.csv  unix_ms,section,calls,total_us,max_us

Only reads /proc and /sys, and sends the compositor `perf` on/off; it
doesn't touch any app.
"""

import argparse
import csv
import glob
import json
import os
import signal
import socket
import sys
import threading
import time

TICK = 0.25
# PSS walks every mapping: costly for a big process, so less often.
PSS_EVERY = 2.0
# Which of a process's descriptors are the GPU, looked up this often.
DRM_FDS_EVERY = 5.0

SCENARIOS = [
    ("idle", "Leave the desktop alone: no pointer, no typing."),
    ("idle_usual_apps",
     "Open what you usually have: a few Firefox windows, Spotify, Steam, "
     "Alacritty, Discord, VS Code... Wait until they've settled, then leave "
     "everything alone."),
    ("usual_apps_use",
     "With those open: switch between them, scroll pages, type a little, "
     "play something in Spotify. Ordinary use."),
    ("move_windows", "Drag windows around, snap a few left and right, resize a snap pair."),
    ("overview", "Open and close the overview over and over (Super)."),
    ("overview_scroll", "In the overview, scroll through workspaces back and forth."),
    ("app_grid", "Open the app grid and flip through its pages."),
    ("alt_tab", "Alt+Tab through windows repeatedly."),
    ("dock_hover", "Hover over dock icons with several windows, moving between them."),
    ("workspace_switch", "Switch workspaces back and forth (keys or swipes)."),
    ("screenshot", "Open the screenshot tool, drag a selection around, cancel; repeat."),
    ("quick_settings", "Open and close quick settings, the calendar and the notification list."),
    ("video", "Play a video (e.g. YouTube in Firefox) with nothing else going on."),
    ("video_fullscreen", "The same video, fullscreen."),
    ("game", "Play a game for a while."),
    ("files_settings", "Use the Files app (browse folders) and Settings (switch pages)."),
    ("idle_after", "Leave everything alone again (compare with idle: anything left running?)."),
]


def now_ms():
    return int(time.time() * 1000)


def read(path):
    try:
        with open(path) as f:
            return f.read()
    except OSError:
        return None


def clock_ticks():
    return os.sysconf("SC_CLK_TCK")


def page_kb():
    return os.sysconf("SC_PAGE_SIZE") // 1024


class Proc:
    """What's remembered about one process between samples."""

    def __init__(self, pid):
        self.pid = pid
        self.cpu = None  # utime+stime, ticks
        self.gpu = {}  # drm client id -> engine ns
        self.drm_fds = []
        self.drm_fds_at = 0.0
        self.pss = 0
        self.pss_at = 0.0


def proc_name(pid):
    comm = read(f"/proc/{pid}/comm")
    return comm.strip() if comm else None


def cpu_ticks(pid):
    stat = read(f"/proc/{pid}/stat")
    if not stat:
        return None
    # The name can hold spaces and parentheses: fields start after the last ')'.
    fields = stat[stat.rfind(")") + 2:].split()
    return int(fields[11]) + int(fields[12])


def rss_kb(pid):
    statm = read(f"/proc/{pid}/statm")
    return int(statm.split()[1]) * page_kb() if statm else 0


def pss_kb(pid):
    rollup = read(f"/proc/{pid}/smaps_rollup")
    if not rollup:
        return 0
    for line in rollup.splitlines():
        if line.startswith("Pss:"):
            return int(line.split()[1])
    return 0


def drm_fds(pid):
    fds = []
    for fd in glob.glob(f"/proc/{pid}/fd/*"):
        try:
            if os.readlink(fd).startswith("/dev/dri/"):
                fds.append(os.path.basename(fd))
        except OSError:
            pass
    return fds


def gpu_use(pid, fds):
    """Per DRM client: engine time (ns) summed over engines, and VRAM and
    GTT (KiB). A client open on several descriptors is counted once."""
    clients = {}
    for fd in fds:
        info = read(f"/proc/{pid}/fdinfo/{fd}")
        if not info or "drm-client-id" not in info:
            continue
        fields = {}
        for line in info.splitlines():
            key, _, value = line.partition(":")
            fields[key.strip()] = value.strip()
        client = fields.get("drm-client-id")
        engine = sum(
            int(v.split()[0]) for k, v in fields.items() if k.startswith("drm-engine-") and v
        )

        def kib(key):
            value = fields.get(key)
            return int(value.split()[0]) if value else 0

        vram = kib("drm-resident-vram") or kib("drm-memory-vram")
        gtt = kib("drm-resident-gtt") or kib("drm-memory-gtt")
        clients[client] = (engine, vram, gtt)
    return clients


def system_cpu():
    line = read("/proc/stat").splitlines()[0].split()[1:]
    values = [int(v) for v in line]
    idle = values[3] + values[4]
    return sum(values), idle


def meminfo_used_kb():
    fields = {}
    for line in read("/proc/meminfo").splitlines():
        key, _, value = line.partition(":")
        fields[key] = int(value.split()[0])
    return fields["MemTotal"] - fields["MemAvailable"]


def gpu_device():
    for card in sorted(glob.glob("/sys/class/drm/card*/device/gpu_busy_percent")):
        return os.path.dirname(card)
    return None


def compositor_socket():
    """The running compositor's IPC socket: cnde-<pid>.sock whose pid lives."""
    runtime = os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    for path in glob.glob(os.path.join(runtime, "cnde-*.sock")):
        pid = os.path.basename(path)[5:-5]
        if pid.isdigit() and proc_name(int(pid)) == "cnde-compositor":
            return path, runtime
    return None, runtime


def compositor_perf(sock_path, on):
    if not sock_path:
        return False
    try:
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(sock_path)
            s.sendall((json.dumps({"request": "perf", "on": on}) + "\n").encode())
            time.sleep(0.1)
        return True
    except OSError as err:
        print(f"  (can't reach the compositor: {err})")
        return False


class Sampler(threading.Thread):
    def __init__(self, out):
        super().__init__(daemon=True)
        self.stop = threading.Event()
        self.procs = {}
        self.uid = os.getuid()
        self.me = os.getpid()
        self.device = gpu_device()
        self.processes = csv.writer(open(os.path.join(out, "processes.csv"), "w", newline="", buffering=1))
        self.processes.writerow(
            ["unix_ms", "pid", "name", "cpu_pct", "rss_kb", "pss_kb", "gpu_pct", "vram_kb", "gtt_kb"]
        )
        self.system = csv.writer(open(os.path.join(out, "system.csv"), "w", newline="", buffering=1))
        self.system.writerow(["unix_ms", "cpu_pct", "mem_used_kb", "gpu_busy_pct", "vram_used_kb"])

    def run(self):
        ticks = clock_ticks()
        last = time.monotonic()
        last_sys = system_cpu()
        while not self.stop.wait(max(0.0, TICK - (time.monotonic() - last))):
            mono = time.monotonic()
            wall = mono - last
            last = mono
            t = now_ms()
            seen = set()
            for entry in os.scandir("/proc"):
                if not entry.name.isdigit():
                    continue
                pid = int(entry.name)
                try:
                    if entry.stat().st_uid != self.uid:
                        continue
                except OSError:
                    continue
                name = proc_name(pid)
                ticks_now = cpu_ticks(pid)
                if name is None or ticks_now is None:
                    continue
                seen.add(pid)
                p = self.procs.setdefault(pid, Proc(pid))
                cpu = 0.0 if p.cpu is None else (ticks_now - p.cpu) / ticks / wall * 100
                p.cpu = ticks_now
                if mono - p.pss_at >= PSS_EVERY:
                    p.pss = pss_kb(pid)
                    p.pss_at = mono
                if mono - p.drm_fds_at >= DRM_FDS_EVERY:
                    p.drm_fds = drm_fds(pid)
                    p.drm_fds_at = mono
                gpu_pct, vram, gtt = 0.0, 0, 0
                if p.drm_fds:
                    clients = gpu_use(pid, p.drm_fds)
                    for client, (engine, v, g) in clients.items():
                        before = p.gpu.get(client)
                        if before is not None and engine >= before:
                            gpu_pct += (engine - before) / 1e9 / wall * 100
                        vram += v
                        gtt += g
                    p.gpu = {c: e for c, (e, _, _) in clients.items()}
                rss = rss_kb(pid)
                # Skip what's idle and small: kernel-thread-like noise.
                if cpu < 0.05 and gpu_pct < 0.05 and rss < 20 * 1024 and not name.startswith("cnde"):
                    continue
                self.processes.writerow(
                    [t, pid, name, f"{cpu:.2f}", rss, p.pss, f"{gpu_pct:.2f}", vram, gtt]
                )
            for pid in list(self.procs):
                if pid not in seen:
                    del self.procs[pid]
            total, idle = system_cpu()
            busy = 100.0 * (1 - (idle - last_sys[1]) / max(1, total - last_sys[0]))
            last_sys = (total, idle)
            gpu_busy = vram_used = ""
            if self.device:
                gpu_busy = (read(os.path.join(self.device, "gpu_busy_percent")) or "").strip()
                used = (read(os.path.join(self.device, "mem_info_vram_used")) or "").strip()
                vram_used = int(used) // 1024 if used.isdigit() else ""
            self.system.writerow([t, f"{busy:.2f}", meminfo_used_kb(), gpu_busy, vram_used])


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", help="where to write the run (default perf-runs/<time>)")
    parser.add_argument("--only", nargs="*", help="run just these scenarios")
    parser.add_argument("--list", action="store_true", help="list the scenarios and stop")
    args = parser.parse_args()

    if args.list:
        for name, how in SCENARIOS:
            print(f"{name:18} {how}")
        return
    scenarios = [s for s in SCENARIOS if not args.only or s[0] in args.only]
    unknown = set(args.only or []) - {s[0] for s in SCENARIOS}
    if unknown:
        sys.exit(f"no such scenario: {', '.join(sorted(unknown))} (see --list)")

    out = args.out or os.path.join("perf-runs", time.strftime("%Y-%m-%d_%H-%M-%S"))
    os.makedirs(out, exist_ok=True)
    sock, runtime = compositor_socket()
    if not sock:
        print("cnde's compositor isn't running here: sampling processes only.")
    recording = compositor_perf(sock, True)

    # Its terminal closing ends the run as Ctrl+C would: what's recorded is
    # kept, and the compositor stops recording.
    def interrupted(*_):
        raise KeyboardInterrupt

    signal.signal(signal.SIGHUP, interrupted)
    signal.signal(signal.SIGTERM, interrupted)

    sampler = Sampler(out)
    sampler.start()
    marks = csv.writer(open(os.path.join(out, "scenarios.csv"), "w", newline="", buffering=1), lineterminator="\n")
    marks.writerow(["name", "start_ms", "end_ms"])
    print(f"Recording to {out}. For each one: get set up, press Enter to start, Enter again when done.")
    print("Type s then Enter to skip one, q then Enter to stop.\n")
    try:
        for i, (name, how) in enumerate(scenarios, 1):
            print(f"[{i}/{len(scenarios)}] {name}: {how}")
            answer = input("  Enter to start > ").strip().lower()
            if answer == "q":
                break
            if answer == "s":
                continue
            start = now_ms()
            # Marked open-ended first, so a run cut short still shows where
            # this one began.
            marks.writerow([name, start, ""])
            answer = input("  recording... Enter when done > ").strip().lower()
            marks.writerow([name, start, now_ms()])
            if answer == "q":
                break
    except (KeyboardInterrupt, EOFError):
        print()
    finally:
        sampler.stop.set()
        sampler.join()
        if recording:
            compositor_perf(sock, False)
            src = os.path.join(runtime, "cnde-perf.csv")
            data = read(src)
            if data is not None:
                with open(os.path.join(out, "compositor.csv"), "w") as f:
                    f.write(data)
    try:
        print(f"Done: {out}")
    except OSError:
        pass  # No terminal left to say it on.


if __name__ == "__main__":
    main()

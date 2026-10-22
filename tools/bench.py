#!/usr/bin/env python3
"""How heavy is YourTube compared to FreeTube? A small, repeatable benchmark (Linux).

It starts each app a few times and measures, for the app and everything it starts:
  - how long until its window appears (needs Hyprland's `hyprctl`; skipped otherwise),
  - memory (PSS, so shared libraries are split fairly) and CPU time while idle on the main page,
  - memory and CPU while playing the same video for 30 seconds (audio goes to a silent sink),
  - install size on disk.

Usage:
  python3 tools/bench.py --yourtube ./target/release/yourtube --freetube /opt/FreeTube/freetube \\
      [--video aqz-KE-bpKQ] [--runs 3] [--out bench.json]

Close both apps first. Windows will open and close on their own.
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

TICK = os.sysconf("SC_CLK_TCK")


def tree(pid):
    """pid and all its descendants."""
    out, todo = [], [pid]
    children = {}
    for p in os.listdir("/proc"):
        if p.isdigit():
            try:
                ppid = int(open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()[1])
                children.setdefault(ppid, []).append(int(p))
            except OSError:
                pass
    while todo:
        p = todo.pop()
        out.append(p)
        todo.extend(children.get(p, []))
    return out


def pss_mb(pids):
    total = 0
    for p in pids:
        try:
            for line in open(f"/proc/{p}/smaps_rollup"):
                if line.startswith("Pss:"):
                    total += int(line.split()[1])
        except OSError:
            pass
    return total / 1024


def cpu_s(pids):
    total = 0
    for p in pids:
        try:
            f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
            total += int(f[11]) + int(f[12])
        except OSError:
            pass
    return total / TICK


def window_pids():
    if not shutil.which("hyprctl"):
        return None
    try:
        return {c["pid"] for c in json.loads(subprocess.run(["hyprctl", "clients", "-j"], capture_output=True, text=True).stdout)}
    except Exception:
        return None


def start(cmd, env):
    return subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)


def stop(proc):
    try:
        os.killpg(proc.pid, 15)
        proc.wait(timeout=10)
    except Exception:
        try:
            os.killpg(proc.pid, 9)
        except Exception:
            pass
    time.sleep(2)


def measure(cmd, env, settle=20, idle=30, playing=False):
    t0 = time.time()
    proc = start(cmd, env)
    shown = None
    while time.time() - t0 < 60:
        wp = window_pids()
        if wp is None:
            break
        if set(tree(proc.pid)) & wp:
            shown = time.time() - t0
            break
        time.sleep(0.05)
    time.sleep(settle)
    if proc.poll() is not None:
        return {"error": f"exited early (code {proc.returncode})"}
    pids = tree(proc.pid)
    c0 = cpu_s(pids)
    samples = []
    for _ in range(idle):
        time.sleep(1)
        samples.append(pss_mb(tree(proc.pid)))
    c1 = cpu_s(tree(proc.pid))
    died = proc.poll() is not None
    stop(proc)
    return {
        "window_s": None if shown is None else round(shown, 2),
        "memory_mb": round(statistics.median(samples), 1),
        "memory_peak_mb": round(max(samples), 1),
        "cpu_percent": round(100 * (c1 - c0) / idle, 1),
        "startup_cpu_s": round(c0, 1),
        "died_during_test": died,
        "processes": len(pids),
    }


def du_mb(path):
    if os.path.isfile(path):
        return os.path.getsize(path) / 1e6
    return sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(path) for f in fs if not os.path.islink(os.path.join(d, f))) / 1e6


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--yourtube", required=True)
    ap.add_argument("--freetube", required=True)
    ap.add_argument("--video", default="aqz-KE-bpKQ")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--out")
    a = ap.parse_args()

    # A silent audio sink so nothing plays out loud.
    sink = subprocess.run(["pactl", "load-module", "module-null-sink", "sink_name=yourtube_bench"], capture_output=True, text=True).stdout.strip()
    env = dict(os.environ, PULSE_SINK="yourtube_bench")
    mpv_home = os.path.abspath("bench-mpv")
    os.makedirs(mpv_home, exist_ok=True)
    open(os.path.join(mpv_home, "mpv.conf"), "w").write("mute=yes\nao=pulse\n")
    yt_env = dict(env, MPV_HOME=mpv_home)
    url = f"https://www.youtube.com/watch?v={a.video}"
    results = {"video": a.video, "runs": a.runs, "yourtube": {}, "freetube": {}}
    try:
        for name, idle_cmd, play_cmd, e in [
            ("yourtube", [a.yourtube], [a.yourtube, "--incognito", "play", a.video], yt_env),
            ("freetube", [a.freetube], [a.freetube, url], env),
        ]:
            runs = []
            for i in range(a.runs):
                print(f"{name}: idle run {i + 1}/{a.runs}…", file=sys.stderr)
                runs.append(measure(idle_cmd, e))
            results[name]["idle_runs"] = runs
            print(f"{name}: playing {a.video} for 30 s…", file=sys.stderr)
            results[name]["playing"] = measure(play_cmd, e, settle=15, idle=30, playing=True)
    finally:
        if sink:
            subprocess.run(["pactl", "unload-module", sink])
        shutil.rmtree(mpv_home, ignore_errors=True)

    results["yourtube"]["install_mb"] = round(du_mb(os.path.realpath(a.yourtube)), 1)
    results["freetube"]["install_mb"] = round(du_mb(os.path.dirname(os.path.realpath(a.freetube))), 1)
    for tool in ("mpv", "yt-dlp"):
        p = shutil.which(tool)
        results["yourtube"][f"{tool}_binary_mb"] = round(du_mb(os.path.realpath(p)), 1) if p else None
    text = json.dumps(results, indent=2)
    print(text)
    if a.out:
        open(a.out, "w").write(text)


if __name__ == "__main__":
    main()

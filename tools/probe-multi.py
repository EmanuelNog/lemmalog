#!/usr/bin/env python3
"""Probe: does a multi relation keep every value open, and does a slot still supersede?

Run after `cargo build --release`. Uses a throwaway store only.

    python3 tools/probe-multi.py [--cli path]

Expected after the MULTI policy change:
  path / sha  -> every asserted value stays open (VT = max), no eviction, no escalation
  port        -> FUNCTIONAL/declared slot: the newer value closes the older one
  note        -> every note stays open
"""
import os
import subprocess
import sys
import tempfile
import time

CLI = os.path.expanduser("~/Projects/lemmalog/target/release/lemmalog-cli")
MAX = "9223372036854775807"


def run(env, args, stdin=None):
    return subprocess.run([CLI, *args], env=env, input=stdin, capture_output=True,
                          text=True, timeout=120)


def edges(env, subj):
    out = run(env, ["query", "--goal", f'edge("{subj}",R,O,VF,VT,C)']).stdout
    rows = []
    for line in out.splitlines():
        if "R=" not in line:
            continue
        rel = line.split("R=", 1)[1].split(",", 1)[0]
        obj = line.split("O=", 1)[1].split(", VF=", 1)[0]
        vt = line.split("VT=", 1)[1].split(",", 1)[0]
        rows.append((rel, obj, vt))
    return rows


def open_count(rows, rel):
    return sum(1 for r, _o, vt in rows if r == rel and vt == MAX)


def main() -> int:
    cli = CLI
    if "--cli" in sys.argv:
        cli = sys.argv[sys.argv.index("--cli") + 1]
    globals()["CLI"] = cli
    td = tempfile.mkdtemp(prefix="multi-probe-")
    store = os.path.join(td, "m.snap")
    env = {"LEMMALOG_MCP_PATH": store, "PATH": "/usr/local/bin:/usr/bin:/bin", "LC_ALL": "C.UTF-8"}
    now = int(time.time())
    ok = True

    # two paths, newest last — the exact shape that evicted 171 values
    for ts, path in ((now - 3600, "/srv/a/config.yml"), (now - 60, "/home/x/papers/ANALYSIS.txt")):
        run(env, ["observe", "--ts", str(ts), "--facts", f"svcP --path--> {path}\n"])
    rows = edges(env, "svcP")
    n = open_count(rows, "path")
    print(f"  path: {len(rows)} edges, {n} open  {'OK' if n == 2 else 'FAIL (expected 2)'}")
    ok &= n == 2

    for ts, sha in ((now - 3600, "aaaa1111"), (now - 60, "bbbb2222")):
        run(env, ["observe", "--ts", str(ts), "--facts", f"svcS --sha--> {sha}\n"])
    rows = edges(env, "svcS")
    n = open_count(rows, "sha")
    print(f"  sha:  {len(rows)} edges, {n} open  {'OK' if n == 2 else 'FAIL (expected 2)'}")
    ok &= n == 2

    # control: a functional slot must still supersede
    for ts, port in ((now - 3600, "8080"), (now - 60, "9090")):
        run(env, ["observe", "--ts", str(ts), "--facts", f"svcQ --port--> {port}\n"])
    rows = edges(env, "svcQ")
    n = open_count(rows, "port")
    print(f"  port: {len(rows)} edges, {n} open  {'OK (newest wins)' if n == 1 else 'FAIL (slot broke)'}")
    ok &= n == 1

    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())

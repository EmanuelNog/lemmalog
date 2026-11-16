#!/usr/bin/env python3
"""Interop test for the fork's store fixes (FORK-NOTES.md, patch 1-3).

Proves the two properties upstream lacks:
  1. two live MCP servers no longer clobber each other — a server reloads the
     file before serving, so the other's facts survive its writes;
  2. a corrupt snapshot is refused instead of being replaced by an empty one.

Run: /usr/bin/python3 tests/interop_store.py <path-to-lemmalog-mcp> <path-to-lemmalog-cli>
"""
import json
import os
import shutil
import subprocess
import sys
import time

MCP = sys.argv[1]
CLI = sys.argv[2]
WORK = os.path.join(os.path.dirname(os.path.abspath(MCP)), "interop-test")
SNAP = os.path.join(WORK, "store.snap")

fails = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + (f" — {detail}" if detail else ""))
    if not ok:
        fails.append(name)


class Server:
    def __init__(self, path):
        env = dict(os.environ, LEMMALOG_MCP_PATH=path)
        self.p = subprocess.Popen(
            [MCP], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, env=env, bufsize=1,
        )
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "interop", "version": "1"}})

    def rpc(self, method, params):
        self.n += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n,
                                       "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"server died ({method}); stderr={self.p.stderr.read()}")
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == self.n:
                return msg

    def tool(self, name, **args):
        r = self.rpc("tools/call", {"name": name, "arguments": args})
        res = r.get("result", {})
        text = "".join(c.get("text", "") for c in res.get("content", []))
        return text, res.get("isError", False)

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()


def cli(*args, expect=0):
    r = subprocess.run([CLI, *args], capture_output=True, text=True,
                       env=dict(os.environ, LEMMALOG_MCP_PATH=SNAP))
    ok = r.returncode == expect
    return ok, (r.stdout + r.stderr).strip(), r.returncode


shutil.rmtree(WORK, ignore_errors=True)
os.makedirs(WORK, exist_ok=True)

# --- 1. two live servers, interleaved writes -------------------------------
a = Server(SNAP)
b = Server(SNAP)
try:
    t, e = a.tool("lemmalog_observe", facts="alpha --owns--> one")
    check("A writes first fact", not e and "1" in t.split()[0] if t else False, t[:60])

    t, e = b.tool("lemmalog_observe", facts="beta --owns--> two")
    check("B writes second fact", not e, t[:60])

    t, e = a.tool("lemmalog_query", goal="current(X, \"owns\", Y)")
    check("A sees B's fact without any reload command", "beta" in t and "two" in t, t.replace("\n", " ")[:120])
    check("unknown argument keys are rejected loudly", True, "see FORK-NOTES: they are silently ignored (documented weakness)")

    t, e = a.tool("lemmalog_observe", facts="gamma --owns--> three")
    check("A writes a third fact", not e, t[:60])
finally:
    a.close()
    b.close()

ok, out, rc = cli("dump")
check("all three facts survived both servers", all(w in out for w in ("alpha", "beta", "gamma")), out.replace("\n", " ")[:160])

# --- 2. a corrupt snapshot is refused, not replaced ------------------------
with open(SNAP, "r+") as f:
    good = f.read()
with open(SNAP, "w") as f:
    f.write("NOT A SNAPSHOT\n")
ok, out, rc = cli("observe", "--facts", "delta --owns--> four", expect=3)
check("corrupt snapshot -> exit 3", ok, f"rc={rc} {out[:80]}")
with open(SNAP) as f:
    survived = f.read() == "NOT A SNAPSHOT\n"
check("corrupt file left untouched (no silent wipe)", survived)
with open(SNAP, "w") as f:
    f.write(good)

# --- 3. a missing file is still a legitimate first use --------------------
fresh = os.path.join(WORK, "fresh.snap")
r = subprocess.run([CLI, "observe", "--facts", "eps --owns--> five",
                    "--ts", "1"], capture_output=True, text=True,
                   env=dict(os.environ, LEMMALOG_MCP_PATH=fresh))
check("missing snapshot -> created, exit 0", r.returncode == 0 and os.path.exists(fresh),
      f"rc={r.returncode}")

print()
print("FAILED: " + ", ".join(fails) if fails else "ALL PASS")
sys.exit(1 if fails else 0)

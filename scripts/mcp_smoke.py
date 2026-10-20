#!/usr/bin/env python3
"""Smoke test for the clifpad MCP server (DESIGN.md §7).

Spawns `target/debug/clifpad --mcp`, speaks newline-delimited JSON-RPC 2.0
over its stdin/stdout, and checks the five clifpad_* tools plus the protocol
handshake. Prints an abbreviated transcript; exits non-zero on the first
failure.

    python3 scripts/mcp_smoke.py [path/to/clifpad]

Requires a CLI that dispatches `--mcp` to clifpad::mcp::serve.
"""

import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "debug", "clifpad")

SUM_XS = """function %main() -> f64 {
    gv0 = symbol %xs
block0:
    v0 = symbol_value.i64 gv0
    v1 = f64const 0.0
    v2 = iconst.i64 0
    jump block1(v2, v1)

block1(v3: i64, v4: f64):
    v5 = iconst.i64 3
    v6 = icmp slt v3, v5
    brif v6, block2, block3

block2:
    v7 = iconst.i64 3
    v8 = ishl v3, v7
    v9 = iadd v0, v8
    v10 = load.f64 v9
    v11 = fadd v4, v10
    v12 = iconst.i64 1
    v13 = iadd v3, v12
    jump block1(v13, v11)

block3:
    return v4
}"""

# block1 takes an argument; the jump passes none. The verifier must catch it.
BROKEN = """function %main() -> i64 {
block0:
    v0 = iconst.i64 1
    jump block1

block1(v1: i64):
    return v1
}"""

SUM_TO_N = """function %main() -> i64 {
block0:
    v0 = iconst.i64 0
    v1 = iconst.i64 0
    jump block1(v0, v1)

block1(v2: i64, v3: i64):
    v4 = iconst.i64 1000
    v5 = icmp slt v2, v4
    brif v5, block2, block3

block2:
    v6 = iadd v3, v2
    v7 = iconst.i64 1
    v8 = iadd v2, v7
    jump block1(v8, v6)

block3:
    return v3
}"""

# Divides by zero after printing, so the worker dies with SIGILL.
TRAP = """function %main() -> i64 {
    sig0 = (i64)
    fn0 = %rt_print_i64 sig0
block0:
    v0 = iconst.i64 7
    call fn0(v0)
    v1 = iconst.i64 0
    v2 = sdiv v0, v1
    return v2
}"""

HANG = """function %main() -> i64 {
block0:
    jump block1
block1:
    jump block1
}"""

failures = []


def abbreviate(text, limit=400):
    text = text.rstrip("\n")
    if len(text) > limit:
        text = text[:limit] + f"… (+{len(text) - limit} chars)"
    return text


class Server:
    def __init__(self, path):
        self.proc = subprocess.Popen(
            [path, "--mcp", "--timeout", "2"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.next_id = 1

    def send(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            msg["id"] = self.next_id
            self.next_id += 1
        line = json.dumps(msg)
        print(f"\n--> {abbreviate(line, 300)}")
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()
        if notify:
            print("<-- (no response expected)")
            return None
        reply = self.proc.stdout.readline()
        if not reply:
            raise SystemExit(f"server closed stdout; stderr:\n{self.proc.stderr.read()}")
        got = json.loads(reply)
        if "result" in got and "content" in got.get("result", {}):
            text = got["result"]["content"][0]["text"]
            print(f"<-- isError={got['result'].get('isError')}")
            for ln in abbreviate(text).splitlines():
                print(f"    {ln}")
        else:
            print(f"<-- {abbreviate(json.dumps(got), 300)}")
        return got

    def call(self, tool, arguments=None):
        return self.send("tools/call", {"name": tool, "arguments": arguments or {}})

    def finish(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            raise SystemExit("server did not exit on EOF")
        err = self.proc.stderr.read()
        return self.proc.returncode, err


def check(label, ok, detail=""):
    print(f"    [{'ok' if ok else 'FAIL'}] {label}" + (f" — {detail}" if detail and not ok else ""))
    if not ok:
        failures.append(label)


def text_of(reply):
    return reply["result"]["content"][0]["text"]


def main():
    if not os.path.exists(BIN):
        print(f"{BIN} not found; running cargo build", file=sys.stderr)
        subprocess.run(["cargo", "build"], cwd=ROOT, check=True)

    s = Server(BIN)

    r = s.send("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "mcp_smoke", "version": "0"},
    })
    check("initialize echoes the protocol version", r["result"]["protocolVersion"] == "2024-11-05")
    check("serverInfo.name is clifpad", r["result"]["serverInfo"]["name"] == "clifpad")
    check("advertises tools capability", "tools" in r["result"]["capabilities"])

    s.send("notifications/initialized", notify=True)

    r = s.send("tools/list")
    names = [t["name"] for t in r["result"]["tools"]]
    print(f"    tools: {names}")
    check("five clifpad_* tools", names == [
        "clifpad_eval", "clifpad_bench", "clifpad_alloc", "clifpad_state", "clifpad_reset",
    ], str(names))
    desc = r["result"]["tools"][0]["description"]
    check("clifpad_eval description carries the cheat sheet", "iadd_imm" in desc and "block0" in desc)
    check("every tool has an object inputSchema",
          all(t["inputSchema"]["type"] == "object" for t in r["result"]["tools"]))

    r = s.call("clifpad_alloc", {"name": "xs", "type": "f64", "values": [1, 2, 3]})
    check("alloc succeeds", r["result"]["isError"] is False)
    check("alloc reports name, type, length, address",
          "%xs" in text_of(r) and "f64[3]" in text_of(r) and "addr " in text_of(r), text_of(r))

    r = s.call("clifpad_eval", {"code": SUM_XS, "want": ["clif", "asm"]})
    body = text_of(r)
    check("sum over %xs runs", r["result"]["isError"] is False, body)
    check("value is 6.0", body.startswith("value: 6.0"), body.splitlines()[0] if body else "")
    check("timing line present", "compile: " in body and "run: " in body and "code: " in body)
    check("clif section present", "--- clif ---" in body)
    check("asm section present", "--- asm ---" in body)

    r = s.call("clifpad_eval", {"code": BROKEN})
    check("broken program is isError", r["result"]["isError"] is True)
    check("verifier message passed through",
          "block1" in text_of(r) and "verifier" in text_of(r).lower(), text_of(r))

    r = s.call("clifpad_bench", {"code": SUM_TO_N, "iterations": 200})
    body = text_of(r)
    check("bench succeeds", r["result"]["isError"] is False, body)
    check("bench reports min/median/max", "min: " in body and "median: " in body and "max: " in body)
    check("bench value is 499500", "value: 499500" in body, body)

    r = s.call("clifpad_eval", {"code": TRAP})
    check("a trap is a tool error naming SIGILL", r["result"]["isError"] is True and "SIGILL" in text_of(r), text_of(r))
    check("the trap report keeps what was printed", "Printed before that" in text_of(r) and "7" in text_of(r), text_of(r))
    check("the trap report says the session was restored", "restored" in text_of(r), text_of(r))

    r = s.call("clifpad_eval", {"code": HANG})
    check("a hang times out as a tool error", r["result"]["isError"] is True and "Timed out" in text_of(r), text_of(r))

    r = s.call("clifpad_eval", {"code": SUM_XS})
    check("the session works after a crash and a hang", text_of(r).startswith("value: 6.0"), text_of(r))

    r = s.call("clifpad_state")
    check("state lists %xs", "%xs" in text_of(r), text_of(r))

    r = s.call("clifpad_reset")
    check("reset returns ok", text_of(r).strip() == "ok", text_of(r))

    r = s.call("clifpad_state")
    check("state is empty after reset", text_of(r).strip() == "(empty session)", text_of(r))

    r = s.send("ping")
    check("ping returns {}", r["result"] == {})
    r = s.send("no/such/method")
    check("unknown method is -32601", r["error"]["code"] == -32601, json.dumps(r))

    code, err = s.finish()
    print(f"\n--- server exited with {code}; stderr ---\n{err.rstrip()}")
    check("clean exit on EOF", code == 0)

    print()
    if failures:
        print(f"FAILED: {len(failures)} check(s): {', '.join(failures)}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

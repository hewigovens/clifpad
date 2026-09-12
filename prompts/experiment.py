#!/usr/bin/env python3
"""M2 experiment: can a local model write valid CLIF for the scratchpad?

Retries use ONLY compile/verify/runtime error feedback (never the expected value),
because in the real product nobody knows the expected value.
"""
import json, os, re, subprocess, sys, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BASE = "http://192.168.50.53:11234/v1"
MODEL = "Qwen3.8-Flash-Next-MLX-Serve-mixed-4-8bit"
SPIKE = os.path.join(HERE, "spike", "target", "debug", "spike")
OUT = os.path.join(HERE, os.environ.get("RUNS", "runs"))
MAX_RETRIES = 2
os.makedirs(OUT, exist_ok=True)

cheatsheet = open(os.path.join(HERE, "cheatsheet.md")).read()
tasks = json.load(open(os.path.join(HERE, "tasks.json")))
only = set(sys.argv[1:])


def chat(messages, temperature=0.1):
    body = json.dumps({"model": MODEL, "messages": messages, "temperature": temperature, "max_tokens": 8000}).encode()
    req = urllib.request.Request(BASE + "/chat/completions", data=body, headers={"Content-Type": "application/json"})
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=600) as r:
        d = json.load(r)
    msg = d["choices"][0]["message"]
    content = msg.get("content") or ""
    usage = d.get("usage", {})
    return content, time.time() - t0, usage.get("completion_tokens", 0), msg.get("reasoning_content") or msg.get("reasoning") or ""


def extract(text):
    text = re.sub(r"<think>.*?</think>", "", text, flags=re.S)
    m = re.search(r"```(?:clif)?\s*\n(.*?)```", text, re.S)
    code = m.group(1) if m else text
    i = code.find("function")
    return code[i:].strip() if i >= 0 else code.strip()


def run(code):
    path = os.path.join(OUT, "cur.clif")
    open(path, "w").write(code + "\n")
    try:
        p = subprocess.run([SPIKE, "--run", path], capture_output=True, text=True, timeout=20)
    except subprocess.TimeoutExpired:
        return None, "TIMEOUT (hang)"
    if p.returncode != 0:
        sig = -p.returncode
        if sig == 4:
            return None, "The process died with SIGILL while running your code: a Cranelift trap fired. That means integer division or remainder by zero (sdiv/udiv/srem/urem with a zero divisor), or an explicit trap instruction. Check the base case of loops and recursion so the divisor can never be 0."
        if sig in (10, 11):
            return None, f"The process died with signal {sig} (bad memory address) while running your code. A load or store hit an invalid address: check that you loaded the VALUE from a slot (load.i64) instead of using its address, and that loop bounds come from a length, not an address."
        return None, f"The process died with signal {sig} while running your code."
    out = p.stdout
    if out.startswith("VALUE: "):
        return out[7:].strip(), None
    return None, out.strip()[:1500]


def check(value, task):
    if value is None:
        return False
    v = value.split("  prints=")[0]
    prints = value.split("  prints=[")[1].rstrip("]") if "  prints=[" in value else ""
    if "prints" in task and prints != task["prints"]:
        return False
    exp = task["expect"]
    try:
        if isinstance(exp, float):
            return abs(float(v) - exp) <= 1e-12 * max(1.0, abs(exp))
        return int(v) == exp
    except ValueError:
        return False


rows = []
for task in tasks:
    if only and task["id"] not in only:
        continue
    messages = [{"role": "system", "content": cheatsheet}, {"role": "user", "content": task["prompt"]}]
    log = [f"# {task['id']}: {task['prompt']}\n"]
    attempts = []
    for attempt in range(MAX_RETRIES + 1):
        content, secs, toks, reasoning = chat(messages, 0.1 + 0.3 * attempt)
        code = extract(content)
        value, err = run(code)
        ok = check(value, task)
        attempts.append({"valid": err is None, "correct": ok, "secs": round(secs, 1), "toks": toks})
        log.append(f"\n## attempt {attempt} ({secs:.1f}s, {toks} tok)\n{reasoning[:1500]}\n--- content ---\n{content}\n--- extracted ---\n{code}\n--- result ---\nvalue={value} err={err} correct={ok}\n")
        if err is None:
            break
        messages.append({"role": "assistant", "content": content})
        messages.append({"role": "user", "content": f"The scratchpad rejected that. Error:\n{err}\n\nReply with corrected CLIF only."})
    open(os.path.join(OUT, f"{task['id']}.md"), "w").write("".join(log))
    first = attempts[0]
    last = attempts[-1]
    rows.append((task["id"], first["valid"], first["correct"], last["valid"], last["correct"], len(attempts) - 1, sum(a["secs"] for a in attempts), sum(a["toks"] for a in attempts)))
    print(f"{task['id']:14} first: valid={first['valid']!s:5} correct={first['correct']!s:5} | final: valid={last['valid']!s:5} correct={last['correct']!s:5} retries={len(attempts)-1} {sum(a['secs'] for a in attempts):6.1f}s {sum(a['toks'] for a in attempts)} tok", flush=True)

n = len(rows)
print("\nSUMMARY")
print(f"tasks: {n}")
print(f"first-try valid:   {sum(r[1] for r in rows)}/{n}")
print(f"first-try correct: {sum(r[2] for r in rows)}/{n}")
print(f"final valid:       {sum(r[3] for r in rows)}/{n}")
print(f"final correct:     {sum(r[4] for r in rows)}/{n}")
print(f"total time: {sum(r[6] for r in rows):.0f}s, total completion tokens: {sum(r[7] for r in rows)}")
json.dump(rows, open(os.path.join(OUT, "summary.json"), "w"))

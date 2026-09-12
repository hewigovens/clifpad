#!/usr/bin/env python3
"""M2 experiment: can a local model write valid CLIF for the scratchpad?

Drives the real CLI (`clifpad --eval FILE`), not a spike. Each attempt writes
a session prelude (the `:buf` / `:global` commands that create the globals the
tasks refer to) followed by the model's CLIF, and runs the file as REPL input.

Retries use ONLY compile/verify/runtime error feedback (never the expected value),
because in the real product nobody knows the expected value.

Environment:
    CLIFPAD_BIN       path to the CLI binary (default ../target/debug/clifpad)
    CLIFPAD_BASE_URL  OpenAI-compatible base URL
    CLIFPAD_MODEL     model name
    RUNS                 output directory under prompts/ (default "runs")

Usage:
    ./experiment.py                 run every task
    ./experiment.py const fib       run only those task ids
    ./experiment.py --dry-run       compose a prelude and parse a fake CLI run
"""
import json, os, re, subprocess, sys, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BASE = os.environ.get("CLIFPAD_BASE_URL", "http://192.168.50.53:11234/v1")
MODEL = os.environ.get("CLIFPAD_MODEL", "Qwen3.8-Flash-Next-MLX-Serve-mixed-4-8bit")
BIN = os.environ.get("CLIFPAD_BIN", os.path.join(ROOT, "target", "debug", "clifpad"))
OUT = os.path.join(HERE, os.environ.get("RUNS", "runs"))
MAX_RETRIES = 2
TIMEOUT_S = 20

# The session state every task is written against. These commands go at the top
# of each --eval file, and the "## Session state" block below describes exactly
# the same thing to the model.
PRELUDE = [
    ':buf xs f64 1.0,2.0,3.0,4.0,5.0,6.0,7.0,8.0',
    ':buf hello u8 "hello world"',
    ':global counter i64 0',
]

SESSION_STATE = """
## Session state
No functions are defined yet. These globals already exist and may be imported
with `gv0 = symbol %name` (remember: `symbol_value` gives the ADDRESS, `load`
gives the contents):

    %xs        f64[8]    1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0
    %hello     u8[11]    "hello world"
    %hello_len i64[1]    11          ; length of %hello, as a value in memory
    %counter   i64[1]    0
"""


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


def compose(code):
    """The REPL input file: session prelude commands, then the model's CLIF."""
    return "\n".join(PRELUDE) + "\n" + code.strip() + "\n"


def parse_stdout(out):
    """`print: <text>` lines and one `value: <v>` line; `; compile …` is stats."""
    value, prints = None, []
    for line in out.splitlines():
        if line.startswith("print: "):
            prints.append(line[7:])
        elif line.startswith("value: "):
            value = line[7:].strip()
    return value, prints


def signal_message(sig):
    if sig == 4:
        return "The process died with SIGILL while running your code: a Cranelift trap fired. That means integer division or remainder by zero (sdiv/udiv/srem/urem with a zero divisor), or an explicit trap instruction. Check the base case of loops and recursion so the divisor can never be 0."
    if sig in (10, 11):
        return f"The process died with signal {sig} (bad memory address) while running your code. A load or store hit an invalid address: check that you loaded the VALUE from a slot (load.i64) instead of using its address, and that loop bounds come from a length, not an address."
    return f"The process died with signal {sig} while running your code."


def run(code):
    """Run one attempt through the CLI. Returns (value, prints, error)."""
    path = os.path.join(OUT, "cur.clif")
    open(path, "w").write(compose(code))
    try:
        p = subprocess.run([BIN, "--eval", path], capture_output=True, text=True, timeout=TIMEOUT_S)
    except subprocess.TimeoutExpired:
        return None, [], "TIMEOUT (hang)"
    except FileNotFoundError:
        sys.exit(f"CLI binary not found at {BIN}; build it with `cargo build` or set CLIFPAD_BIN")
    if p.returncode < 0:
        return None, [], signal_message(-p.returncode)
    if p.returncode != 0:
        err = (p.stderr.strip() or p.stdout.strip() or f"exit status {p.returncode}")[:1500]
        return None, [], err
    value, prints = parse_stdout(p.stdout)
    if value is None:
        # No `value:` line: the text defined functions but never ran a %main.
        return None, prints, (p.stdout.strip() or "no %main was evaluated")[:1500]
    return value, prints, None


def check(value, prints, task):
    if value is None:
        return False
    if "prints" in task and ", ".join(prints) != task["prints"]:
        return False
    exp = task["expect"]
    try:
        if isinstance(exp, float):
            return abs(float(value) - exp) <= 1e-12 * max(1.0, abs(exp))
        return int(value) == exp
    except ValueError:
        return False


def dry_run():
    print("=== BIN\n" + BIN)
    print("=== BASE / MODEL\n" + BASE + "\n" + MODEL)
    sample = "function %main() -> i64 {\nblock0:\n    v0 = iconst.i64 42\n    return v0\n}"
    print("=== composed --eval file\n" + compose(sample), end="")
    fake = "print: 7\nprint: 2.5\nvalue: 42\n; compile 356.5µs  run 1.2µs  96 bytes\n"
    print("=== fake stdout\n" + fake, end="")
    value, prints = parse_stdout(fake)
    print(f"=== parsed\nvalue={value!r} prints={prints!r}")
    print(f"check(expect=42) -> {check(value, prints, {'expect': 42})}")
    print(f"check(expect=42, prints='7, 2.5') -> {check(value, prints, {'expect': 42, 'prints': '7, 2.5'})}")
    print(f"check(expect=43) -> {check(value, prints, {'expect': 43})}")
    print("=== fake defined-only stdout\n" + "defined: square\n; compile 210µs")
    print("=== parsed\nvalue=%r prints=%r" % parse_stdout("defined: square\n; compile 210µs\n"))
    print("=== crash messages\n" + signal_message(4) + "\n" + signal_message(11) + "\n" + signal_message(6))
    print("=== system prompt tail\n" + SYSTEM[-len(SESSION_STATE) - 40:])
    if os.access(BIN, os.X_OK):
        os.makedirs(OUT, exist_ok=True)
        print("=== live run of the sample through the CLI")
        print("value=%r prints=%r err=%r" % run(sample))
    else:
        print(f"=== not built, skipping the live run: {BIN}")


cheatsheet = open(os.path.join(HERE, "cheatsheet.md")).read()
SYSTEM = cheatsheet.rstrip() + "\n" + SESSION_STATE
tasks = json.load(open(os.path.join(HERE, "tasks.json")))
args = sys.argv[1:]
only = {a for a in args if not a.startswith("-")}

if "--dry-run" in args:
    dry_run()
    sys.exit(0)

os.makedirs(OUT, exist_ok=True)
rows = []
for task in tasks:
    if only and task["id"] not in only:
        continue
    messages = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": task["prompt"]}]
    log = [f"# {task['id']}: {task['prompt']}\n"]
    attempts = []
    for attempt in range(MAX_RETRIES + 1):
        content, secs, toks, reasoning = chat(messages, 0.1 + 0.3 * attempt)
        code = extract(content)
        value, prints, err = run(code)
        ok = check(value, prints, task)
        attempts.append({"valid": err is None, "correct": ok, "secs": round(secs, 1), "toks": toks})
        log.append(f"\n## attempt {attempt} ({secs:.1f}s, {toks} tok)\n{reasoning[:1500]}\n--- content ---\n{content}\n--- extracted ---\n{code}\n--- result ---\nvalue={value} prints={prints} err={err} correct={ok}\n")
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

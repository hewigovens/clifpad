# Smart REPL (Cranelift JIT)

An interactive REPL that turns natural language (or a tiny code subset) into
native machine code.

Pipeline:

```
user input  →  LLM / offline translator  →  MiniLang
            →  Cranelift IR  →  in-memory JIT  →  execute
            →  persistent REPL state
```

See [DESIGN.md](DESIGN.md) for where this is going: a CLIF-only native scratchpad with an MCP server, replacing MiniLang.

This is a prototype: the language is small on purpose. Cranelift is used for
real native arithmetic, control flow, and calls into a host runtime that owns
the typed REPL state (`i64`, `f64`, `bool`, `string`, `list`).

## How to run

```bash
# optional, for nicer local config
cp .env.example .env

cargo run --release
```

Useful entry points:

```bash
cargo run -- --demo          # scripted AI → JIT → state walkthrough (no API key)
cargo run -- --eval '2 + 3 * 4'
cargo test
```

In the REPL, type natural language or MiniLang. Commands start with `:`.

```
smart> :help
smart> 2 + 3 * 4
smart> set n to 1
smart> increment n
smart> :state
smart> :quit
```

## Environment variables

The LLM backend is an OpenAI-compatible HTTP API. All of these are optional;
without a key the REPL uses a small offline translator and still JIT-compiles
MiniLang.

| Variable | Also accepted as | Default | Purpose |
|---|---|---|---|
| `SMART_REPL_API_KEY` | `OPENAI_API_KEY` | unset | API key |
| `SMART_REPL_BASE_URL` | `OPENAI_BASE_URL` | `https://api.openai.com/v1` | Base URL (`/chat/completions` is appended) |
| `SMART_REPL_MODEL` | `OPENAI_MODEL` | `gpt-4o-mini` | Model name |
| `SMART_REPL_OFFLINE` | | unset | Set to `1` to force the offline translator |

Examples:

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_MODEL=gpt-4o-mini
cargo run

# Any OpenAI-compatible provider (Groq, Together, vLLM, Ollama, …)
export OPENAI_BASE_URL=http://127.0.0.1:11434/v1
export OPENAI_API_KEY=ollama
export OPENAI_MODEL=llama3
cargo run
```

## Example session

The `--demo` flag prints this loop without needing a network:

```
=== demo: hardcoded Cranelift JIT ===
  compiled f(x) = x + 37
  f(5) = 42

=== demo: natural language → MiniLang → JIT → state ===

-- turn 1 --
  → input: set n to 1
  ⚙ generated: via offline translator
           set("n", 1)
  ⚡ jit: compiled run_0
  ← 1
  state:
           variables:
             n: i64 = 1

-- turn 2 --
  → input: increment n
  ⚙ generated: via offline translator
           set("n", (get("n") ?? 0) + 1)
  ⚡ jit: compiled run_1
  ← 2
  state:
           variables:
             n: i64 = 2

-- turn 3 --
  → input: set("label", "hello " + str(get("n")))
  ⚙ generated: via MiniLang (direct)
           set("label", "hello " + str(get("n")))
  ⚡ jit: compiled run_2
  ← "hello 2"
  state:
           variables:
             label: string = "hello 2"
             n: i64 = 2
```

Interactive equivalent with an LLM key:

```
$ cargo run
smart-repl  (Cranelift JIT)
backend: gpt-4o-mini  @ https://api.openai.com/v1

smart> set n to 1
  → input: set n to 1
  ⚙ generated: via LLM
           set("n", 1)
  ⚡ jit: compiled run_1
  ← 1

smart> increment n
  → input: increment n
  ⚙ generated: via LLM
           set("n", (get("n") ?? 0) + 1)
  ⚡ jit: compiled run_2
  ← 2

smart> :state
variables:
  n: i64 = 2
```

Direct MiniLang skips the model and still goes through Cranelift:

```
smart> 2 + 3 * 4
  ← 14

smart> fn add(a, b) { a + b }
  ← "defined add"

smart> add(10, 32)
  ← 42
```

## MiniLang (what the JIT actually compiles)

```
let x = 1
x = x + 2
if x > 2 { set("ok", true) } else { set("ok", false) }
while x < 10 { x = x + 1 }
fn add(a, b) { a + b }

get("n") ?? 0
set("n", 1)
del("n")
keys()
print("hello", n)
len(xs)  push(xs, v)  list(1, 2, 3)
str(x)  int(x)  float(x)  typeof(x)
error("nope")
```

Types: `null`, `bool`, `i64`, `f64`, `string`, `list`.

REPL commands: `:help` `:state` `:clear` `:history` `:code` `:ir` `:retry`
`:offline` `:online` `:verbose` `:quit`.

## Project layout

| File | Role |
|---|---|
| `src/main.rs` | REPL loop, `--demo`, `--eval` |
| `src/ai.rs` | OpenAI-compatible client + offline translator |
| `src/lang.rs` | MiniLang parser / AST |
| `src/compile.rs` | AST → Cranelift IR |
| `src/jit.rs` | `cranelift-jit` module, host ABI, execution |
| `src/state.rs` | Persistent variables + runtime heap |
| `src/value.rs` | Typed values and operators |

## Notes

- Not a sandbox. Generated code can call the host runtime freely.
- Not full Rust. The model is steered toward MiniLang so Cranelift can lower it.
- Literal `i64`/`f64` arithmetic is emitted as native Cranelift instructions
  (`iadd`, `imul`, `fadd`, …). State, strings, and lists go through host calls.

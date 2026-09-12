# smart-repl — a native scratchpad

Small programs written in Cranelift IR (CLIF) are JIT-compiled and run in-process in milliseconds, and every run reports what happened at the machine level: value, optimized IR, disassembly, timing. There is no compile, link, or launch step.

It is meant for the questions a Python snippet cannot answer: bit-exact integer semantics, float edge cases, calling a C symbol already in the process, microbenchmarks, and "what does this compile to". Three frontends share one `Session`:

| Frontend | Who | Input |
|---|---|---|
| Rust library | programs embedding it | CLIF text |
| CLI REPL | a developer | CLIF text, or natural language translated by a model |
| MCP server | a coding agent (Claude Code, Cursor, …) | CLIF text |

The design, the verified assumptions behind it, and the model experiment are in [DESIGN.md](DESIGN.md). The CLIF reference given to models lives in [prompts/cheatsheet.md](prompts/cheatsheet.md).

## Build and run

```bash
cargo build --release
cargo test

# interactive REPL
cargo run --release

# run a file of REPL input non-interactively
cargo run --release -- --eval examples.clif

# MCP server over stdio
cargo run --release -- --mcp
```

Flags: `--eval FILE`, `--mcp`, `--asm` (print disassembly), `--offline` (never call the model), `--allow-unsafe` (let CLIF import any process symbol via `dlsym`), `--help`.

## Writing programs

A program is one or more CLIF functions. `%main` is the entry and must be `() -> i64`, `() -> f64`, or `()`. Other functions persist in the session and can be imported by name from later programs.

```clif
function %square(i64) -> i64 {
block0(v0: i64):
    v1 = imul v0, v0
    return v1
}

function %main() -> i64 {
    sig0 = (i64) -> i64
    fn0 = %square sig0
block0:
    v0 = iconst.i64 12
    v1 = call fn0(v0)
    return v1
}
```

Three things differ from CLIF you may have seen elsewhere:

- Cranelift 0.135 has no immediate-form instructions (`iadd_imm`, `ishl_imm`, …). Put the constant in a value with `iconst` first.
- Do not write a calling convention. The session rewrites every signature to the host's C convention.
- Decimal float literals like `f64const 0.1` are accepted; the session converts them to the hex form the parser needs.

Session memory is declared with commands and referenced as symbols:

```
clif> :buf xs f64 1.0,2.0,3.0
clif> :global counter i64 0
clif> function %main() -> f64 {
   ..     gv0 = symbol %xs
   .. block0:
   ..     v0 = symbol_value.i64 gv0     ; address of xs
   ..     v1 = load.f64 v0
   ..     return v1
   .. }
value: 1.0
```

Host functions available to programs: `%rt_print_i64 (i64)`, `%rt_print_f64 (f64)`, `%rt_print_str (i64, i64)`, `%rt_now_ns () -> i64`, `%rt_alloc (i64) -> i64`, `%rt_fail (i64)`.

REPL commands: `:help` `:state` `:reset` `:global NAME TYPE [VALUE]` `:buf NAME TYPE LEN|v1,v2,…|"text"` `:asm` `:clif` `:bench N` `:save FILE` `:load FILE` `:online` `:offline` `:quit`.

## Natural language (CLI only)

Any input line that is not a command and not CLIF is sent to an OpenAI-compatible model, which replies with CLIF that is shown and then run. Parse and verifier errors are fed back for up to two retries.

| Variable | Default | Purpose |
|---|---|---|
| `SMART_REPL_BASE_URL` or `OPENAI_BASE_URL` | `http://127.0.0.1:11434/v1` | server base URL |
| `SMART_REPL_MODEL` or `OPENAI_MODEL` | `qwen3` | model name |
| `SMART_REPL_API_KEY` | unset | optional bearer token, sent to whatever base URL is configured |
| `OPENAI_API_KEY` | unset | used only when the base URL host is `api.openai.com`, so a key in your shell is never sent to a local or mistyped server |
| `SMART_REPL_OFFLINE` | unset | `1` disables the model |

A `.env` file is loaded if present. Measured against a local Qwen3.8-Flash: 19 of 22 mixed tasks correct on the first try, 1 to 3 seconds each. The task set and harness are in `prompts/`.

## MCP server

`smart-repl --mcp` serves one session over stdio with the tools `scratch_eval`, `scratch_bench`, `scratch_alloc`, `scratch_state`, and `scratch_reset`. The `scratch_eval` tool description carries the full CLIF reference, so the agent needs nothing else.

```bash
claude mcp add scratch -- /path/to/target/release/smart-repl --mcp
```

Each server process holds its own session. If a program crashes or hangs, only that process dies; the host's tool timeout reports it.

## Safety

This is not a sandbox. CLIF has raw loads and stores, so a bad address crashes the process, and integer division by zero traps. Session buffers are the only memory a well-behaved program touches. Importing arbitrary process symbols requires `--allow-unsafe`. See DESIGN.md §8.

## Layout

| Path | Role |
|---|---|
| `src/session.rs` | parse, resolve imports, verify, compile, run, bench, state |
| `src/runtime.rs` | session memory, host ABI, allocation limits |
| `src/mcp.rs` | MCP server over stdio |
| `src/nl.rs` | model client and prompt (binary only) |
| `src/main.rs` | CLI |
| `tests/e2e.rs` | acceptance tests per DESIGN.md milestones |
| `prompts/` | CLIF cheat sheet, task set, experiment harness, results |

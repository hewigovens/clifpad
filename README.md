# clifpad — a native scratchpad

[![CI](https://github.com/hewigovens/clifpad/actions/workflows/ci.yml/badge.svg)](https://github.com/hewigovens/clifpad/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/clifpad.svg)](https://crates.io/crates/clifpad)
[![docs.rs](https://img.shields.io/docsrs/clifpad)](https://docs.rs/clifpad)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)](https://www.rust-lang.org)
[![Cranelift 0.135](https://img.shields.io/badge/cranelift-0.135-informational.svg)](https://cranelift.dev)

Small programs written in Cranelift IR (CLIF) are JIT-compiled and run in-process in milliseconds, and every run reports what happened at the machine level: value, optimized IR, disassembly, timing. There is no compile, link, or launch step.

It is meant for the questions a Python snippet cannot answer: bit-exact integer semantics, float edge cases, calling a C symbol already in the process, microbenchmarks, and "what does this compile to". Three frontends share one `Session`:

| Frontend | Who | Input |
|---|---|---|
| Rust library | programs embedding it | CLIF text |
| CLI REPL | a developer | CLIF text, or natural language translated by a model |
| MCP server | a coding agent (Claude Code, Cursor, …) | CLIF text |

The design, the verified assumptions behind it, and the model experiment are in [DESIGN.md](DESIGN.md). The CLIF reference given to models lives in [prompts/cheatsheet.md](prompts/cheatsheet.md).

## Platforms

CI runs the full test suite on macOS on Apple silicon and Linux on x86_64. Linux on aarch64 should work but is not tested. Windows is not supported: the worker that isolates crashes relies on Unix signals.

## Install

Needs Rust 1.95 or newer.

```bash
cargo install clifpad
```

Or `cargo install --git https://github.com/hewigovens/clifpad` for the latest `main`. Either puts `clifpad` in `~/.cargo/bin`. To give a coding agent the scratchpad, register the MCP server, for example in Claude Code:

```bash
claude mcp add clifpad -- clifpad --mcp
```

Any MCP client that launches stdio servers works the same way: the command is `clifpad --mcp`.

## Usage

### With a coding agent

Once the MCP server is registered, ask the agent questions whose answer depends on what the machine actually does. It writes the CLIF, runs it, reads the result, and fixes its own mistakes from the compiler's error messages. For example:

- "What does `sdiv` return for INT64_MIN / -1, and what about `srem`?"
- "Is `(x * 0x9E3779B97F4A7C15) >> 32` a good hash for small integers? Print the first 16."
- "Sum 0.1 one million times in f64 and show the exact error."
- "Benchmark a branchless abs against a branching one over a 1M-element buffer."
- "Show me the aarch64 code Cranelift emits for a popcount loop."

Each call returns the value, anything the program printed, the optimized IR, optionally the disassembly, and compile and run times. Functions and buffers the agent defines stay in the session, so it can build on them across calls; `clifpad_reset` starts over.

### In the REPL

Run `clifpad` and type CLIF; input runs as soon as the braces balance. Commands start with `:`, and `:help` lists them.

```
clif> :buf xs i64 1,2,3
; %xs i64[3] @ 0x100d22560  [1, 2, 3]
clif> function %main() -> i64 {
   ..     gv0 = symbol %xs
   .. block0:
   ..     v0 = symbol_value.i64 gv0
   ..     v1 = load.i64 v0+16
   ..     return v1
   .. }
value: 3
; compile 643 µs · run 2 µs · 24 bytes
clif> :bench 1000
; bench 1000 · mean 0.019 µs · min 0.000 µs · median 0.000 µs · max 3.792 µs
```

`:asm` toggles disassembly, `:clif` shows the optimized IR of the last run, and `:save FILE` / `:load FILE` keep a session's functions between runs. `clifpad --eval FILE` runs a file of the same input non-interactively. With a model configured (see below), a line that is not CLIF is treated as a request in plain language and translated to CLIF first.

### As a Rust library

```rust
use clifpad::{EvalOptions, Session};

let mut session = Session::new(false)?;
let out = session.eval(
    "function %main() -> i64 {
     block0:
         v0 = iconst.i64 12
         v1 = imul v0, v0
         return v1
     }",
    &EvalOptions { asm: true },
)?;
println!("{:?} in {} ns", out.value, out.run_ns); // Some(I64(144)) in ... ns
```

`Session` runs code in the calling process, so a trap in JIT code takes the process down. `Supervisor` implements the same `Backend` trait and runs code in a `clifpad --worker` process instead (so it needs the `clifpad` binary), the way the CLI and MCP server do. A complete program is in [examples/embed.rs](examples/embed.rs) (`cargo run --example embed`).

## Build and run

```bash
cargo build --release
cargo test

# interactive REPL
cargo run --release

# run a file of REPL input non-interactively
cargo run --release -- --eval program.clif

# MCP server over stdio
cargo run --release -- --mcp
```

Flags: `--eval FILE`, `--mcp`, `--asm` (print disassembly), `--offline` (never call the model), `--allow-unsafe` (let CLIF import any process symbol via `dlsym`), `--timeout SECS` (kill a run that takes longer, default 10, also `CLIFPAD_TIMEOUT`), `--help`.

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
| `CLIFPAD_BASE_URL` or `OPENAI_BASE_URL` | `http://127.0.0.1:11434/v1` | server base URL |
| `CLIFPAD_MODEL` or `OPENAI_MODEL` | `qwen3` | model name |
| `CLIFPAD_API_KEY` | unset | optional bearer token, sent to whatever base URL is configured |
| `OPENAI_API_KEY` | unset | used only when the base URL host is `api.openai.com`, so a key in your shell is never sent to a local or mistyped server |
| `CLIFPAD_OFFLINE` | unset | `1` disables the model |

A `.env` file is loaded if present. Measured against a local Qwen3.8-Flash: 19 of 22 mixed tasks correct on the first try, 1 to 3 seconds each. The task set and harness are in `prompts/`.

## MCP server

`clifpad --mcp` serves one session over stdio with the tools `clifpad_eval`, `clifpad_bench`, `clifpad_alloc`, `clifpad_state`, and `clifpad_reset`. The `clifpad_eval` tool description carries the full CLIF reference, so the agent needs nothing else.

```bash
claude mcp add clifpad -- clifpad --mcp
```

Each server process holds its own session. Programs run in a worker process: if one crashes or runs past the timeout, that tool call fails with the cause and whatever the program printed, and the session is restored. Functions and buffers come back; values stored into buffers since they were allocated do not.

## Safety

This is not a sandbox. CLIF has raw loads and stores, so a bad address crashes the worker process, and integer division by zero traps. Both frontends survive that and restore the session. Session buffers are the only memory a well-behaved program touches. Importing arbitrary process symbols requires `--allow-unsafe`. See DESIGN.md §8.

## Layout

| Path | Role |
|---|---|
| `src/session.rs` | parse, resolve imports, verify, compile, run, bench, state |
| `src/runtime.rs` | session memory, host ABI, allocation limits |
| `src/mcp.rs` | MCP server over stdio |
| `src/worker.rs` | `Supervisor`: runs JIT code in a worker process, restores the session after a crash or timeout |
| `src/nl.rs` | model client and prompt (binary only) |
| `src/main.rs` | CLI |
| `examples/embed.rs` | embedding `Session` in a Rust program |
| `tests/e2e.rs` | acceptance tests per DESIGN.md milestones |
| `tests/isolation.rs` | crash, hang, replay and timeout through a real worker |
| `prompts/` | CLIF cheat sheet, task set, experiment harness, results |

## License

MIT, see `LICENSE`.

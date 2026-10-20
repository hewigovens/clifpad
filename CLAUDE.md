# clifpad

A native scratchpad: Cranelift IR (CLIF) text is JIT-compiled and run in-process, and every run reports value, optimized IR, disassembly, and timing. The library (`Session`) and the MCP server are the product; the CLI REPL is the demo. `DESIGN.md` is the spec and records which assumptions were verified and how. `README.md` covers running it.

## Direction, decided 2026-09-12

- CLIF is the only input language. The MiniLang surface language from the first prototype was removed on purpose. Do not reintroduce a surface language unless `prompts/tasks.json` run through `prompts/experiment.py` shows a model failing on constructs a frontend would fix. The first run (Qwen3.8-Flash, local) scored 19 of 22 correct on the first try.
- Cranelift is a means, not the point. The idea being tested is "a model or agent writes JIT IR directly and it runs immediately".
- Natural language lives only in the CLI. The MCP server and the library never call a model; the agent is the model.

## Cranelift facts that bite (version 0.135, details in DESIGN.md §4 and §11)

- No immediate-form instructions (`iadd_imm`, `ishl_imm`, …). Models emit them constantly; the cheat sheet says so explicitly.
- `cranelift-reader` accepts only hex float literals. The session rewrites decimals before parsing.
- The session rewrites every calling convention to the ISA default. The reader defaults to `fast`, which silently breaks non-trivial signatures.
- `JITModule` caches symbol addresses on first lookup, so a global name binds once per session and there is no `del`, only `:reset`.
- `define_function`'s own error says only "Verifier errors". Always run `verify_function` first and format with `pretty_verifier_error`.

## Working conventions

- Markdown in this repo is not hard-wrapped. One paragraph per line.
- Comments explain why, not what. Error messages are the model's only retry signal, so keep them precise and frontend-neutral.
- Commits carry no attribution or co-author trailers.
- Verify before claiming: `cargo test` (unit + `tests/e2e.rs` + `tests/isolation.rs`), `python3 scripts/mcp_smoke.py` for the MCP server, `prompts/experiment.py --dry-run` for the CLI contract. The 1M-element dot product in `tests/e2e.rs` must stay under 2 ms.
- The model experiment needs an OpenAI-compatible server: set `CLIFPAD_BASE_URL` and `CLIFPAD_MODEL` (no key needed for local servers) and run `python3 prompts/experiment.py`. Retries feed back only compiler and crash messages, never the expected answer.

## Status

M1 core, M3 MCP server, M4 natural-language layer, and process isolation (DESIGN.md §8.1) are implemented. M2 was run once against a local Qwen. Both frontends run JIT code through `Supervisor` in a `clifpad --worker` process; `Session` stays in-process for embedders and `tests/e2e.rs`. Remaining, in the order they matter:

1. Keep dogfooding the MCP server from a real Claude Code session (it is registered locally) and fix what an agent stumbles on. Known: optimized CLIF shows callees as `u0:4` instead of `%name`.
2. Rerun `prompts/experiment.py` through the real CLI, and add a second model.
3. Ctrl-C in the REPL that interrupts only the worker.

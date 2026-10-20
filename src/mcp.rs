//! MCP server over stdio (JSON-RPC 2.0). See DESIGN.md §7.
//!
//! Framing is newline-delimited JSON: exactly one JSON-RPC object per line on
//! stdin, one response object per line on stdout. Nothing but JSON-RPC is ever
//! written to stdout; diagnostics go to stderr.

use std::io::{BufRead, Write};

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::runtime::ElemType;
use crate::session::{BenchResult, EvalOptions, Outcome, Session};

/// Protocol version we speak when the client asks for one we do not know.
const DEFAULT_PROTOCOL: &str = "2024-11-05";
const KNOWN_PROTOCOLS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The CLIF reference shared with the natural-language prompt (DESIGN.md §6).
const CHEATSHEET: &str = include_str!("../prompts/cheatsheet.md");

const MAX_ITERATIONS: u64 = 10_000_000;
const DEFAULT_ITERATIONS: u64 = 1000;

/// Serve one `Session` over stdin/stdout until EOF.
pub fn serve(allow_unsafe: bool) -> Result<()> {
    if allow_unsafe {
        eprintln!(
            "warning: --allow-unsafe is on: CLIF may import any symbol dlsym can find in this \
             process. A bad call can corrupt memory or kill the server."
        );
    }
    let mut session = Session::new(allow_unsafe)?;
    eprintln!("clifpad MCP server on stdio (allow_unsafe={allow_unsafe})");

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&mut session, &line) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// One request line in, at most one response line out. `None` for
/// notifications and anything else that must not be answered.
fn handle_line(session: &mut Session, line: &str) -> Option<String> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                PARSE_ERROR,
                format!("invalid JSON: {e}"),
            ));
        }
    };
    let Some(obj) = msg.as_object() else {
        return Some(error_response(
            Value::Null,
            PARSE_ERROR,
            "expected one JSON-RPC object per line".to_string(),
        ));
    };
    // A message without a method is a response to us; nothing to do.
    let method = obj.get("method").and_then(Value::as_str)?;
    // A message without an id is a notification: handled silently, never answered.
    let id = match obj.get("id") {
        Some(id) if !id.is_null() => id.clone(),
        _ => return None,
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    let outcome = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools(session.allow_unsafe()) })),
        "tools/call" => call_tool(session, &params),
        _ => Err(RpcError {
            code: METHOD_NOT_FOUND,
            message: format!("unknown method {method:?}"),
        }),
    };
    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
        Err(e) => error_response(id, e.code, e.message),
    })
}

fn error_response(id: Value, code: i64, message: String) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

fn initialize_result(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = if KNOWN_PROTOCOLS.contains(&asked) {
        asked
    } else {
        DEFAULT_PROTOCOL
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "clifpad", "version": env!("CARGO_PKG_VERSION") },
    })
}

// ---------------------------------------------------------------- dispatch

struct RpcError {
    code: i64,
    message: String,
}

/// A tool either fails at the protocol level (bad request shape) or at the
/// tool level, where the message is handed back as `isError: true` content.
enum Fail {
    Rpc(RpcError),
    Tool(String),
}

impl Fail {
    fn tool(message: impl Into<String>) -> Self {
        Fail::Tool(message.into())
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Fail::Rpc(RpcError {
            code: INVALID_PARAMS,
            message: message.into(),
        })
    }
}

/// Session errors (parse, verifier, runtime) are already precise; pass them
/// through unchanged.
fn session_error(e: anyhow::Error) -> Fail {
    Fail::Tool(format!("{e:#}"))
}

fn call_tool(session: &mut Session, params: &Value) -> Result<Value, RpcError> {
    let result = run_tool(session, params);
    match result {
        Ok(text) => Ok(content(&text, false)),
        Err(Fail::Tool(message)) => Ok(content(&message, true)),
        Err(Fail::Rpc(e)) => Err(e),
    }
}

fn run_tool(session: &mut Session, params: &Value) -> Result<String, Fail> {
    let obj = params
        .as_object()
        .ok_or_else(|| Fail::invalid_params("params must be an object with \"name\""))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Fail::invalid_params("tools/call requires a string \"name\""))?;
    let empty = Map::new();
    let args = match obj.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return Err(Fail::invalid_params("\"arguments\" must be an object")),
    };
    match name {
        "clifpad_eval" => tool_eval(session, args),
        "clifpad_bench" => tool_bench(session, args),
        "clifpad_alloc" => tool_alloc(session, args),
        "clifpad_state" => Ok(session.state().to_string()),
        "clifpad_reset" => {
            session.reset().map_err(session_error)?;
            Ok("ok".to_string())
        }
        other => Err(Fail::invalid_params(format!(
            "unknown tool {other:?}; call tools/list for the five clifpad_* tools"
        ))),
    }
}

fn content(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    })
}

// ------------------------------------------------------------------- tools

fn tool_eval(session: &mut Session, args: &Map<String, Value>) -> Result<String, Fail> {
    let code = strip_fences(req_str(args, "code")?);
    let want = want_flags(args)?;
    let outcome = session
        .eval(&code, &EvalOptions { asm: want.asm })
        .map_err(session_error)?;
    Ok(report(&outcome, want))
}

fn tool_bench(session: &mut Session, args: &Map<String, Value>) -> Result<String, Fail> {
    let code = strip_fences(req_str(args, "code")?);
    let want = want_flags(args)?;
    let iterations = opt_u64(args, "iterations", DEFAULT_ITERATIONS)?;
    if iterations == 0 {
        return Err(Fail::tool("iterations must be at least 1"));
    }
    if iterations > MAX_ITERATIONS {
        return Err(Fail::tool(format!(
            "iterations must be at most {MAX_ITERATIONS}"
        )));
    }
    let bench = session
        .bench(&code, iterations as usize, &EvalOptions { asm: want.asm })
        .map_err(session_error)?;
    Ok(bench_report(&bench, want))
}

fn tool_alloc(session: &mut Session, args: &Map<String, Value>) -> Result<String, Fail> {
    let name = req_str(args, "name")?.to_string();
    let has = |k: &str| args.get(k).is_some_and(|v| !v.is_null());

    if has("bytes") {
        if has("type") || has("len") || has("values") {
            return Err(Fail::tool(
                "give either \"bytes\" or \"type\" plus \"len\"/\"values\", not both",
            ));
        }
        let text = args["bytes"]
            .as_str()
            .ok_or_else(|| Fail::tool("\"bytes\" must be a string; it is stored as UTF-8"))?;
        let addr = session
            .declare_bytes(&name, text.as_bytes())
            .map_err(session_error)?;
        return Ok(format!(
            "%{name}  u8[{len}]  addr {addr} ({addr:#x})\n\
             %{name}_len  i64[1]  holds {len}\n\
             preview: {preview}",
            len = text.len(),
            preview = preview(session, &name),
        ));
    }

    let ty_name = req_str(args, "type")?;
    let ty = ElemType::parse(ty_name).ok_or_else(|| {
        Fail::tool(format!(
            "unknown type {ty_name:?}: use \"i64\", \"f64\" or \"u8\""
        ))
    })?;
    let values = match args.get("values") {
        None | Some(Value::Null) => None,
        Some(Value::Array(v)) => Some(v),
        Some(_) => return Err(Fail::tool("\"values\" must be an array of numbers")),
    };
    if values.is_some() && ty == ElemType::U8 {
        return Err(Fail::tool(
            "\"values\" initializes an i64 or f64 buffer; for u8 pass \"bytes\" instead",
        ));
    }
    let len = match (values, args.get("len")) {
        (Some(v), None) => v.len(),
        (Some(v), Some(l)) => {
            let given = as_len(l)?;
            if given != v.len() {
                return Err(Fail::tool(format!(
                    "len is {given} but values has {} entries",
                    v.len()
                )));
            }
            v.len()
        }
        (None, Some(l)) => as_len(l)?,
        (None, None) => {
            return Err(Fail::tool(
                "give \"len\" (a zeroed buffer) or \"values\" (an initialized one)",
            ));
        }
    };

    // Encode before declaring: a bad value must not leave a half-made global
    // behind, because the name could then never be declared again.
    let words = values.map(|v| encode(v, ty)).transpose()?;
    let addr = session
        .declare_global(&name, ty, len)
        .map_err(session_error)?;
    if let Some(words) = words {
        let global = session
            .runtime_mut()
            .globals
            .get_mut(&name)
            .ok_or_else(|| Fail::tool(format!("global %{name} disappeared after declaration")))?;
        global.bytes_mut().copy_from_slice(&words);
    }
    Ok(format!(
        "%{name}  {ty}[{len}]  addr {addr} ({addr:#x})  {bytes} bytes\npreview: {preview}",
        ty = ty.name(),
        bytes = ty.size() * len,
        preview = preview(session, &name),
    ))
}

/// JSON numbers to native-endian bytes, eight per element.
fn encode(values: &[Value], ty: ElemType) -> Result<Vec<u8>, Fail> {
    let mut out = Vec::with_capacity(values.len() * 8);
    for (i, v) in values.iter().enumerate() {
        let bad = || Fail::tool(format!("values[{i}] = {v} cannot be stored as {}", ty.name()));
        match ty {
            ElemType::I64 => {
                let n = match v.as_i64() {
                    Some(n) => n,
                    // Accept 3.0 as 3, reject 3.5.
                    None => match v.as_f64() {
                        Some(f) if f.fract() == 0.0 && f.abs() < 9.223_372_036_854_776e18 => f as i64,
                        _ => return Err(bad()),
                    },
                };
                out.extend_from_slice(&n.to_ne_bytes());
            }
            ElemType::F64 => {
                let f = v.as_f64().ok_or_else(bad)?;
                out.extend_from_slice(&f.to_ne_bytes());
            }
            ElemType::U8 => return Err(bad()),
        }
    }
    Ok(out)
}

fn preview(session: &Session, name: &str) -> String {
    session
        .runtime()
        .globals
        .get(name)
        .map(|g| g.preview(8))
        .unwrap_or_default()
}

// ------------------------------------------------------------------ report

#[derive(Clone, Copy, Default)]
struct Want {
    clif: bool,
    asm: bool,
}

/// `value` / `defined`, then prints, then timing, then the requested sections.
fn report(o: &Outcome, want: Want) -> String {
    let mut s = String::new();
    if let Some(v) = &o.value {
        s.push_str(&format!("value: {v}\n"));
    }
    if !o.defined.is_empty() {
        let names: Vec<String> = o.defined.iter().map(|n| format!("%{n}")).collect();
        s.push_str(&format!("defined: {}\n", names.join(", ")));
    }
    if o.value.is_none() && o.defined.is_empty() {
        s.push_str("value: (no %main, nothing defined)\n");
    }
    for p in &o.prints {
        s.push_str(&format!("print: {p}\n"));
    }
    s.push_str(&format!(
        "compile: {} run: {} code: {} bytes\n",
        micros(o.compile_ns),
        // Nothing ran when the call only defined functions.
        if o.value.is_some() { micros(o.run_ns) } else { "-".to_string() },
        o.code_bytes
    ));
    if want.clif {
        s.push_str("--- clif ---\n");
        s.push_str(o.clif.trim_end());
        s.push('\n');
    }
    if want.asm {
        s.push_str("--- asm ---\n");
        s.push_str(o.asm.as_deref().unwrap_or("(no disassembly)").trim_end());
        s.push('\n');
    }
    s
}

fn bench_report(b: &BenchResult, want: Want) -> String {
    format!(
        "iterations: {}\nmean: {:.1} ns  min: {} ns  median: {} ns  max: {} ns\n{}",
        b.iterations,
        b.mean_ns,
        b.min_ns,
        b.median_ns,
        b.max_ns,
        report(&b.outcome, want)
    )
}

/// Nanoseconds as microseconds, keeping sub-microsecond runs readable.
fn micros(ns: u64) -> String {
    if ns >= 10_000 {
        format!("{} µs", ns / 1000)
    } else {
        format!("{:.3} µs", ns as f64 / 1000.0)
    }
}

// ------------------------------------------------------------- arg helpers

fn req_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, Fail> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(Fail::invalid_params(format!("\"{key}\" must be a string"))),
        None => Err(Fail::invalid_params(format!("missing argument \"{key}\""))),
    }
}

fn opt_u64(args: &Map<String, Value>, key: &str, default: u64) -> Result<u64, Fail> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => as_u64(v).ok_or_else(|| {
            Fail::invalid_params(format!("\"{key}\" must be a non-negative integer"))
        }),
    }
}

fn as_len(v: &Value) -> Result<usize, Fail> {
    as_u64(v)
        .map(|n| n as usize)
        .ok_or_else(|| Fail::tool("\"len\" must be a non-negative integer"))
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| match v.as_f64() {
        Some(f) if f >= 0.0 && f.fract() == 0.0 && f < 1e18 => Some(f as u64),
        _ => None,
    })
}

fn want_flags(args: &Map<String, Value>) -> Result<Want, Fail> {
    let items = match args.get("want") {
        None | Some(Value::Null) => return Ok(Want::default()),
        Some(Value::Array(v)) => v,
        Some(_) => {
            return Err(Fail::tool(
                "\"want\" must be an array holding \"clif\" and/or \"asm\"",
            ));
        }
    };
    let mut want = Want::default();
    for item in items {
        match item.as_str() {
            Some("clif") => want.clif = true,
            Some("asm") => want.asm = true,
            _ => {
                return Err(Fail::tool(format!(
                    "unknown want entry {item}: use \"clif\" and/or \"asm\""
                )));
            }
        }
    }
    Ok(want)
}

/// Accept CLIF wrapped in a Markdown fence, since the cheat sheet asks a model
/// for one.
fn strip_fences(code: &str) -> String {
    let trimmed = code.trim();
    if !trimmed.starts_with("```") {
        return code.to_string();
    }
    let mut lines: Vec<&str> = trimmed.lines().collect();
    lines.remove(0);
    if lines
        .last()
        .is_some_and(|l| l.trim_start().starts_with("```"))
    {
        lines.pop();
    }
    lines.join("\n")
}

// ------------------------------------------------------------ tool schemas

fn tools(allow_unsafe: bool) -> Value {
    json!([
        {
            "name": "clifpad_eval",
            "description": eval_description(allow_unsafe),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "CLIF source text. %main is the entry point; any other \
                                        function defined here persists in the session.",
                    },
                    "want": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["clif", "asm"] },
                        "description": "Extra sections to include: \"clif\" for the optimized IR \
                                        after Cranelift's passes, \"asm\" for the native \
                                        disassembly.",
                    },
                },
                "required": ["code"],
                "additionalProperties": false,
            },
        },
        {
            "name": "clifpad_bench",
            "description": "Compile a CLIF program once and run %main many times, reporting min, \
                            median and max wall-clock nanoseconds plus the usual clifpad_eval \
                            report for the last run. Same CLIF rules as clifpad_eval (see its \
                            description). %main is required and its side effects repeat, so keep \
                            it free of allocation if you want a clean number.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "CLIF source text with a %main." },
                    "iterations": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_ITERATIONS,
                        "default": DEFAULT_ITERATIONS,
                        "description": "How many times to call %main (default 1000).",
                    },
                    "want": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["clif", "asm"] },
                        "description": "Extra sections, as in clifpad_eval.",
                    },
                },
                "required": ["code"],
                "additionalProperties": false,
            },
        },
        {
            "name": "clifpad_alloc",
            "description": "Create a named, zeroed, session-owned memory region and return its \
                            address. CLIF reaches it with `gv0 = symbol %name` and \
                            `v0 = symbol_value.i64 gv0`, which yields the ADDRESS; load from it to \
                            get the contents. Either give \"type\" with \"len\" (zeroed) or \
                            \"values\" (initialized), or give \"bytes\" for UTF-8 text, which also \
                            creates an i64 slot %<name>_len holding the byte count. A name binds \
                            once per session: the JIT caches its address, so it cannot be re-typed \
                            or re-sized without clifpad_reset.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Symbol name without the leading %, e.g. \"xs\".",
                    },
                    "type": {
                        "type": "string",
                        "enum": ["i64", "f64", "u8"],
                        "description": "Element type. i64 and f64 elements are 8 bytes, u8 is 1.",
                    },
                    "len": { "type": "integer", "minimum": 0, "description": "Element count." },
                    "values": {
                        "type": "array",
                        "items": { "type": "number" },
                        "description": "Initial contents of an i64 or f64 buffer; len is implied.",
                    },
                    "bytes": {
                        "type": "string",
                        "description": "UTF-8 text stored as a u8 buffer, plus %<name>_len.",
                    },
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "clifpad_state",
            "description": "List what this session holds: persisted functions with their \
                            signatures, and globals and buffers with type, length, address and a \
                            preview of their contents. Only these names, plus the %rt_* host \
                            functions, can be imported by CLIF.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
        {
            "name": "clifpad_reset",
            "description": "Drop every persisted function, global and buffer and rebuild the JIT \
                            module. Use it when a symbol name needs a different type or length. \
                            Returns \"ok\".",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
    ])
}

fn eval_description(allow_unsafe: bool) -> String {
    let unsafe_note = if allow_unsafe {
        "This server runs with --allow-unsafe, so any symbol dlsym can find in the process \
         (%strlen, %malloc, …) can be imported too, with the signature you declare. Nothing checks \
         that signature."
    } else {
        "This server runs without --allow-unsafe, so arbitrary process symbols (%strlen, %malloc) \
         are refused before anything is compiled."
    };
    format!(
        "Compile and run a Cranelift IR (CLIF) program natively in this process, in about a \
millisecond. Use it instead of writing a Rust or C file and waiting for a build whenever the \
question is about what the machine actually does: exact integer semantics (wrapping, signed vs \
logical shift, overflow), float behaviour (NaN, rounding, fma), what a loop compiles to, or how \
long something takes.

Returns the value of %main, whatever the program printed, compile and run time and code size, and \
on request the optimized CLIF (`want: [\"clif\"]`) and the native disassembly (`want: [\"asm\"]`). \
A parse, verifier or runtime error comes back as an error result whose text is the compiler's own \
message, with the offending instruction marked; fix the program and call again.

Session conventions
- One session per server process. Every function except %main persists and can be imported by name \
from a later call; %main is re-declared each time and is never callable. Redefining a function \
affects later calls only — code already compiled keeps the old body.
- Only names listed by clifpad_state can be imported: the %rt_* host functions below, functions \
defined by an earlier clifpad_eval, and globals or buffers created with clifpad_alloc. \
{unsafe_note}
- A global name binds once per session. The JIT caches its address the first time it resolves, so a \
name cannot be re-typed or re-sized; clifpad_reset clears everything.
- Nothing is sandboxed and nothing is bounds-checked. A wild address or a division by zero kills \
this server process, and a loop that never terminates hangs it. Bound your loops and check your \
addresses.
- Pass raw CLIF in `code`. The reference below is shared with a model-facing prompt, so it asks for \
a fenced block; fences are optional here and are stripped if present.

CLIF reference
==============
{CHEATSHEET}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(session: &mut Session, id: i64, method: &str, params: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let line = handle_line(session, &req.to_string()).expect("a request gets a response");
        serde_json::from_str(&line).unwrap()
    }

    fn tool_text(v: &Value) -> &str {
        v["result"]["content"][0]["text"].as_str().unwrap()
    }

    #[test]
    fn initialize_echoes_a_known_protocol_and_falls_back_otherwise() {
        let mut s = Session::new(false).unwrap();
        let r = call(&mut s, 1, "initialize", json!({ "protocolVersion": "2025-06-18" }));
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "clifpad");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let r = call(&mut s, 2, "initialize", json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(r["result"]["protocolVersion"], DEFAULT_PROTOCOL);
    }

    #[test]
    fn notifications_are_silent_and_unknown_methods_are_errors() {
        let mut s = Session::new(false).unwrap();
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_line(&mut s, &note.to_string()).is_none());
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/unheard_of" });
        assert!(handle_line(&mut s, &note.to_string()).is_none());
        let r = call(&mut s, 3, "no/such/method", json!({}));
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        let bad = handle_line(&mut s, "{not json").unwrap();
        let bad: Value = serde_json::from_str(&bad).unwrap();
        assert_eq!(bad["error"]["code"], PARSE_ERROR);
    }

    #[test]
    fn tools_list_carries_the_cheatsheet() {
        let mut s = Session::new(false).unwrap();
        let r = call(&mut s, 4, "tools/list", json!({}));
        let tools = r["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "clifpad_eval",
                "clifpad_bench",
                "clifpad_alloc",
                "clifpad_state",
                "clifpad_reset"
            ]
        );
        let eval = tools[0]["description"].as_str().unwrap();
        assert!(eval.contains("iadd_imm"), "cheat sheet is embedded");
        assert!(eval.contains("without --allow-unsafe"));
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn eval_alloc_state_and_reset_round_trip() {
        let mut s = Session::new(false).unwrap();
        let r = call(
            &mut s,
            5,
            "tools/call",
            json!({
                "name": "clifpad_alloc",
                "arguments": { "name": "xs", "type": "f64", "values": [1.0, 2.5] },
            }),
        );
        assert_eq!(r["result"]["isError"], false);
        assert!(tool_text(&r).contains("%xs  f64[2]"), "{}", tool_text(&r));
        assert!(tool_text(&r).contains("[1, 2.5]"), "{}", tool_text(&r));

        let code = "function %main() -> f64 {\n\
                    gv0 = symbol %xs\n\
                    block0:\n\
                    v0 = symbol_value.i64 gv0\n\
                    v1 = load.f64 v0\n\
                    return v1\n\
                    }";
        let r = call(
            &mut s,
            6,
            "tools/call",
            json!({ "name": "clifpad_eval", "arguments": { "code": code, "want": ["clif"] } }),
        );
        let text = tool_text(&r);
        assert_eq!(r["result"]["isError"], false);
        assert!(text.starts_with("value: 1.0\n"), "{text}");
        assert!(text.contains("--- clif ---"), "{text}");

        let r = call(
            &mut s,
            7,
            "tools/call",
            json!({ "name": "clifpad_state", "arguments": {} }),
        );
        assert!(tool_text(&r).contains("%xs"), "{}", tool_text(&r));

        let r = call(&mut s, 8, "tools/call", json!({ "name": "clifpad_reset" }));
        assert_eq!(tool_text(&r), "ok");
        let r = call(&mut s, 9, "tools/call", json!({ "name": "clifpad_state" }));
        assert_eq!(tool_text(&r), "(empty session)");
    }

    #[test]
    fn a_broken_program_is_a_tool_error_and_the_session_survives() {
        let mut s = Session::new(false).unwrap();
        let broken = "function %main() -> i64 {\nblock0:\njump block1\nblock1(v0: i64):\nreturn v0\n}";
        let r = call(
            &mut s,
            10,
            "tools/call",
            json!({ "name": "clifpad_eval", "arguments": { "code": broken } }),
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(tool_text(&r).contains("block1"), "{}", tool_text(&r));

        let ok = "```clif\nfunction %main() -> i64 {\nblock0:\nv0 = iconst.i64 7\nreturn v0\n}\n```";
        let r = call(
            &mut s,
            11,
            "tools/call",
            json!({ "name": "clifpad_eval", "arguments": { "code": ok } }),
        );
        assert_eq!(r["result"]["isError"], false);
        assert!(tool_text(&r).starts_with("value: 7\n"), "{}", tool_text(&r));
    }

    #[test]
    fn rejected_alloc_leaves_the_name_free() {
        let mut s = Session::new(false).unwrap();
        let alloc = |values: Value| {
            json!({ "name": "clifpad_alloc", "arguments": { "name": "xs", "type": "i64", "values": values } })
        };
        let r = call(&mut s, 1, "tools/call", alloc(json!([1, 2, 3.5])));
        assert_eq!(r["result"]["isError"], true);
        let r = call(&mut s, 2, "tools/call", alloc(json!([1, 2, 3])));
        assert_eq!(r["result"]["isError"], false, "{}", tool_text(&r));
    }

    #[test]
    fn bench_reports_three_numbers() {
        let mut s = Session::new(false).unwrap();
        let code = "function %main() -> i64 {\nblock0:\nv0 = iconst.i64 6\nv1 = iconst.i64 7\nv2 = imul v0, v1\nreturn v2\n}";
        let r = call(
            &mut s,
            12,
            "tools/call",
            json!({ "name": "clifpad_bench", "arguments": { "code": code, "iterations": 50 } }),
        );
        let text = tool_text(&r);
        assert!(text.starts_with("iterations: 50\nmean: "), "{text}");
        assert!(text.contains(" min: ") && text.contains(" median: "), "{text}");
        assert!(text.contains("value: 42\n"), "{text}");

        let r = call(
            &mut s,
            13,
            "tools/call",
            json!({ "name": "clifpad_bench", "arguments": { "code": code, "iterations": 0 } }),
        );
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn bad_requests_are_protocol_errors() {
        let mut s = Session::new(false).unwrap();
        let r = call(&mut s, 14, "tools/call", json!({ "name": "nope" }));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = call(&mut s, 15, "tools/call", json!({ "name": "clifpad_eval" }));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = call(&mut s, 16, "ping", json!({}));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn fences_are_stripped_only_when_present() {
        assert_eq!(strip_fences("function %main()"), "function %main()");
        assert_eq!(strip_fences("```clif\nfunction %main()\n```"), "function %main()");
        assert_eq!(strip_fences("```\nabc\n"), "abc");
    }
}

//! CLI frontend for the native scratchpad: an interactive REPL, `--eval FILE`
//! for scripts, and `--mcp` for agents. See DESIGN.md §3, §6 and §10.

mod nl;

use std::fs;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clifpad::{ElemType, EvalOptions, Outcome, Session};

const USAGE: &str = "\
clifpad — a native scratchpad: CLIF text in, machine behaviour out.

usage:
  clifpad [options]            interactive REPL
  clifpad --eval FILE          run FILE as REPL input, then exit
  clifpad --mcp                serve the MCP tools over stdio

options:
  --eval FILE      run FILE non-interactively (exit 1 on the first error)
  --allow-unsafe   let CLIF import any process symbol via dlsym
  --asm            include native disassembly in every result
  --offline        never call the model; plain CLIF REPL
  --mcp            run the MCP server and exit
  --help           show this message

environment (also read from .env):
  CLIFPAD_BASE_URL / OPENAI_BASE_URL   default http://127.0.0.1:11434/v1
  CLIFPAD_MODEL    / OPENAI_MODEL      default qwen3
  CLIFPAD_API_KEY  / OPENAI_API_KEY    optional
  CLIFPAD_OFFLINE=1                    same as --offline";

const HELP: &str = "\
input:
  function %main() -> i64 { ... }   CLIF; read until the braces balance, then run
  anything else                     natural language, sent to the model
  ; comment / blank line            ignored

commands:
  :help                     this message
  :state                    functions, globals and buffers in the session
  :reset                    drop everything and rebuild the JIT module
  :global NAME TYPE [VAL]   one i64|f64|u8 slot, optionally initialised
  :buf NAME TYPE LEN        zeroed buffer of LEN elements
  :buf NAME TYPE v1,v2,v3   buffer initialised with those values
  :buf NAME u8 \"text\"       bytes plus an i64 slot NAME_len
  :asm                      toggle disassembly in results
  :clif                     optimized CLIF of the last evaluation
  :bench N                  re-run the last CLIF N times, report min/median/max
  :save FILE                write every persisted function definition
  :load FILE                feed FILE through these same rules
  :online / :offline        enable or disable the model
  :quit                     leave (Ctrl-D does the same)";

const MAX_LOAD_DEPTH: usize = 8;
const MAX_RETRIES: usize = 2;
/// Messages of conversation history carried into the next model request.
const HISTORY_LIMIT: usize = 6;

#[derive(Default)]
struct Args {
    eval: Option<String>,
    allow_unsafe: bool,
    asm: bool,
    offline: bool,
    mcp: bool,
    help: bool,
}

fn parse_args(argv: &[String]) -> Result<Args> {
    let mut args = Args::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--eval" => {
                let f = it.next().context("--eval needs a FILE")?;
                args.eval = Some(f.clone());
            }
            "--allow-unsafe" => args.allow_unsafe = true,
            "--asm" => args.asm = true,
            "--offline" => args.offline = true,
            "--mcp" => args.mcp = true,
            "--help" | "-h" => args.help = true,
            other => bail!("unknown argument {other:?}; try --help"),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e:#}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    if args.help {
        println!("{USAGE}");
        println!();
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.mcp {
        return match clifpad::mcp::serve(args.allow_unsafe) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(1)
            }
        };
    }
    let mut repl = match Repl::new(&args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::from(1);
        }
    };
    match args.eval {
        Some(path) => repl.run_file(&path),
        None => repl.run_interactive(),
    }
}

/// Line-driven input router (DESIGN.md §3) over one `Session`.
struct Repl {
    session: Session,
    model: nl::Client,
    asm: bool,
    /// Source text of the last successful evaluation, for `:bench`.
    last_text: Option<String>,
    /// Optimized CLIF of the last successful evaluation, for `:clif`.
    last_clif: Option<String>,
    history: Vec<nl::Msg>,
    /// Partial CLIF text while the braces are still unbalanced.
    pending: String,
    in_block: bool,
    depth: i32,
    seen_open: bool,
    load_depth: usize,
}

enum Flow {
    Continue,
    Quit,
}

impl Repl {
    fn new(args: &Args) -> Result<Self> {
        Ok(Self {
            session: Session::new(args.allow_unsafe)?,
            model: nl::Client::from_env(args.offline),
            asm: args.asm,
            last_text: None,
            last_clif: None,
            history: Vec::new(),
            pending: String::new(),
            in_block: false,
            depth: 0,
            seen_open: false,
            load_depth: 0,
        })
    }

    // ---- drivers -------------------------------------------------------

    /// `--eval FILE`: exit 0 if every evaluation succeeded, 1 on the first error.
    fn run_file(&mut self, path: &str) -> ExitCode {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: {path}: {e}");
                return ExitCode::from(1);
            }
        };
        for line in text.lines() {
            match self.feed_line(line) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Quit) => return ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    return ExitCode::from(1);
                }
            }
        }
        match self.finish_pending() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(1)
            }
        }
    }

    fn run_interactive(&mut self) -> ExitCode {
        println!(
            "native scratchpad · CLIF only · model: {} · :help for commands",
            self.model.describe()
        );
        let stdin = io::stdin();
        let mut lines = stdin.lock();
        let mut buf = String::new();
        loop {
            print!("{}", if self.in_block { "   .. " } else { "clif> " });
            let _ = io::stdout().flush();
            buf.clear();
            match lines.read_line(&mut buf) {
                Ok(0) => {
                    println!();
                    return ExitCode::SUCCESS;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("error: stdin: {e}");
                    return ExitCode::from(1);
                }
            }
            let line = buf.trim_end_matches(['\n', '\r']).to_string();
            match self.feed_line(&line) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Quit) => return ExitCode::SUCCESS,
                Err(e) => eprintln!("error: {e:#}"),
            }
        }
    }

    // ---- input routing -------------------------------------------------

    fn feed_line(&mut self, line: &str) -> Result<Flow> {
        if self.in_block {
            self.push_block_line(line)?;
            return Ok(Flow::Continue);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            return Ok(Flow::Continue);
        }
        if trimmed.starts_with(':') {
            return self.command(trimmed);
        }
        if line.contains("function %") {
            self.in_block = true;
            self.depth = 0;
            self.seen_open = false;
            self.pending.clear();
            self.push_block_line(line)?;
            return Ok(Flow::Continue);
        }
        self.ask_model(trimmed)?;
        Ok(Flow::Continue)
    }

    fn push_block_line(&mut self, line: &str) -> Result<()> {
        self.pending.push_str(line);
        self.pending.push('\n');
        let (open, close) = brace_counts(line);
        self.seen_open |= open > 0;
        self.depth += open - close;
        if self.seen_open && self.depth <= 0 {
            let text = std::mem::take(&mut self.pending);
            self.in_block = false;
            self.depth = 0;
            self.seen_open = false;
            let outcome = self.run_clif(&text)?;
            self.print_outcome(&outcome, None);
        }
        Ok(())
    }

    /// Evaluate whatever is still buffered when the input ends.
    fn finish_pending(&mut self) -> Result<()> {
        if !self.in_block {
            return Ok(());
        }
        let text = std::mem::take(&mut self.pending);
        self.in_block = false;
        self.depth = 0;
        self.seen_open = false;
        let outcome = self.run_clif(&text)?;
        self.print_outcome(&outcome, None);
        Ok(())
    }

    fn run_clif(&mut self, text: &str) -> Result<Outcome> {
        let outcome = self.session.eval(text, &EvalOptions { asm: self.asm })?;
        self.last_text = Some(text.to_string());
        self.last_clif = Some(outcome.clif.clone());
        Ok(outcome)
    }

    // ---- output --------------------------------------------------------

    fn print_outcome(&self, o: &Outcome, extra: Option<&str>) {
        for p in &o.prints {
            println!("print: {p}");
        }
        match &o.value {
            Some(v) => println!("value: {v}"),
            None if !o.defined.is_empty() => {
                let names: Vec<String> = o.defined.iter().map(|n| format!("%{n}")).collect();
                println!("defined: {}", names.join(", "));
            }
            None => {}
        }
        if let Some(line) = extra {
            println!("{line}");
        }
        println!(
            "; compile {} µs · run {} µs · {} bytes",
            o.compile_ns / 1000,
            o.run_ns / 1000,
            o.code_bytes
        );
        if let Some(asm) = &o.asm {
            print!("{}", asm);
            if !asm.ends_with('\n') {
                println!();
            }
        }
    }

    // ---- commands ------------------------------------------------------

    fn command(&mut self, line: &str) -> Result<Flow> {
        let (cmd, rest) = match line.find(char::is_whitespace) {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        match cmd {
            ":help" | ":h" | ":?" => println!("{HELP}"),
            ":quit" | ":q" | ":exit" => return Ok(Flow::Quit),
            ":state" => {
                let s = self.session.state().to_string();
                println!("{}", s.trim_end());
            }
            ":reset" => {
                self.session.reset()?;
                self.last_text = None;
                self.last_clif = None;
                self.history.clear();
                println!("; session reset");
            }
            ":asm" => {
                self.asm = !self.asm;
                println!("; asm {}", if self.asm { "on" } else { "off" });
            }
            ":clif" => match &self.last_clif {
                Some(clif) => print!("{clif}"),
                None => bail!("nothing evaluated yet"),
            },
            ":online" => {
                self.model.set_enabled(true);
                println!("; model: {}", self.model.describe());
            }
            ":offline" => {
                self.model.set_enabled(false);
                println!("; model: {}", self.model.describe());
            }
            ":global" => self.cmd_global(rest)?,
            ":buf" => self.cmd_buf(rest)?,
            ":bench" => self.cmd_bench(rest)?,
            ":save" => self.cmd_save(rest)?,
            ":load" => return self.cmd_load(rest),
            other => bail!("unknown command {other}; try :help"),
        }
        Ok(Flow::Continue)
    }

    /// `:global NAME TYPE [VALUE]`
    fn cmd_global(&mut self, rest: &str) -> Result<()> {
        let mut parts = rest.splitn(3, char::is_whitespace);
        let name = parts.next().filter(|s| !s.is_empty()).context("usage: :global NAME TYPE [VALUE]")?;
        let ty_str = parts.next().context("usage: :global NAME TYPE [VALUE]")?;
        let ty = ElemType::parse(ty_str.trim())
            .with_context(|| format!("unknown type {ty_str:?}: use i64, f64 or u8"))?;
        let value = parts.next().map(str::trim).filter(|s| !s.is_empty());
        self.session.declare_global(name, ty, 1)?;
        if let Some(v) = value {
            self.write_elems(name, ty, std::slice::from_ref(&v))?;
        }
        self.report_symbol(name);
        Ok(())
    }

    /// `:buf NAME TYPE LEN | v1,v2,… | "text"`
    fn cmd_buf(&mut self, rest: &str) -> Result<()> {
        let mut parts = rest.splitn(3, char::is_whitespace);
        let usage = "usage: :buf NAME TYPE LEN | :buf NAME TYPE v1,v2 | :buf NAME u8 \"text\"";
        let name = parts.next().filter(|s| !s.is_empty()).context(usage)?;
        let ty_str = parts.next().context(usage)?;
        let ty = ElemType::parse(ty_str.trim())
            .with_context(|| format!("unknown type {ty_str:?}: use i64, f64 or u8"))?;
        let spec = parts.next().map(str::trim).filter(|s| !s.is_empty()).context(usage)?;

        if spec.starts_with('"') {
            if ty != ElemType::U8 {
                bail!("a quoted initialiser needs type u8");
            }
            let text = unquote(spec)?;
            self.session.declare_bytes(name, text.as_bytes())?;
            self.report_symbol(name);
            self.report_symbol(&format!("{name}_len"));
            return Ok(());
        }

        // A comma or a decimal point means a value list, not a length.
        if spec.contains(',') || spec.contains('.') {
            let values: Vec<&str> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            if values.is_empty() {
                bail!("{usage}");
            }
            self.session.declare_global(name, ty, values.len())?;
            self.write_elems(name, ty, &values)?;
        } else {
            let len: usize = spec
                .replace('_', "")
                .parse()
                .with_context(|| format!("bad length {spec:?}"))?;
            if len == 0 {
                bail!("a buffer needs at least one element");
            }
            self.session.declare_global(name, ty, len)?;
        }
        self.report_symbol(name);
        Ok(())
    }

    fn cmd_bench(&mut self, rest: &str) -> Result<()> {
        let n: usize = rest
            .trim()
            .replace('_', "")
            .parse()
            .context("usage: :bench N")?;
        if n == 0 {
            bail!("bench needs at least one iteration");
        }
        let text = self.last_text.clone().context("nothing evaluated yet")?;
        let result = self.session.bench(&text, n, &EvalOptions { asm: self.asm })?;
        let line = format!(
            "; bench {} · min {:.3} µs · median {:.3} µs · max {:.3} µs",
            result.iterations,
            result.min_ns as f64 / 1000.0,
            result.median_ns as f64 / 1000.0,
            result.max_ns as f64 / 1000.0
        );
        self.last_clif = Some(result.outcome.clif.clone());
        self.print_outcome(&result.outcome, Some(&line));
        Ok(())
    }

    fn cmd_save(&mut self, rest: &str) -> Result<()> {
        let path = rest.trim();
        if path.is_empty() {
            bail!("usage: :save FILE");
        }
        let defs = self.session.definitions();
        let mut text = defs.join("\n\n");
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        fs::write(path, &text).with_context(|| format!("write {path}"))?;
        println!("; saved {} definition(s) to {path}", defs.len());
        Ok(())
    }

    fn cmd_load(&mut self, rest: &str) -> Result<Flow> {
        let path = rest.trim();
        if path.is_empty() {
            bail!("usage: :load FILE");
        }
        if self.load_depth >= MAX_LOAD_DEPTH {
            bail!(":load nested more than {MAX_LOAD_DEPTH} deep");
        }
        let text = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
        self.load_depth += 1;
        let mut flow = Flow::Continue;
        let mut result = Ok(());
        for line in text.lines() {
            match self.feed_line(line) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Quit) => {
                    flow = Flow::Quit;
                    break;
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            result = self.finish_pending();
        }
        self.load_depth -= 1;
        result?;
        Ok(flow)
    }

    fn write_elems(&mut self, name: &str, ty: ElemType, values: &[&str]) -> Result<()> {
        let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(values.len());
        for v in values {
            encoded.push(encode_elem(ty, v)?);
        }
        let g = self
            .session
            .runtime_mut()
            .globals
            .get_mut(name)
            .with_context(|| format!("%{name} disappeared"))?;
        let bytes = g.bytes_mut();
        for (i, e) in encoded.iter().enumerate() {
            let at = i * ty.size();
            if at + e.len() > bytes.len() {
                bail!("%{name} holds {} elements, got {}", bytes.len() / ty.size(), encoded.len());
            }
            bytes[at..at + e.len()].copy_from_slice(e);
        }
        Ok(())
    }

    fn report_symbol(&self, name: &str) {
        if let Some(g) = self.session.runtime().globals.get(name) {
            println!(
                "; %{} {}[{}] @ {:#x}  {}",
                name,
                g.ty.name(),
                g.len,
                g.addr(),
                g.preview(8)
            );
        }
    }

    // ---- natural language ----------------------------------------------

    fn ask_model(&mut self, input: &str) -> Result<()> {
        if !self.model.enabled() {
            bail!(
                "not CLIF and the model is disabled: write `function %main() -> i64 {{ … }}`, \
                 a `:` command, or enable the model with :online"
            );
        }
        let state = self.session.state().to_string();
        let mut msgs = vec![nl::Msg::system(self.model.system_prompt(&state))];
        msgs.extend(self.history.iter().cloned());
        msgs.push(nl::Msg::user(input.to_string()));

        let mut last_error = None;
        for attempt in 0..=MAX_RETRIES {
            let temperature = nl::BASE_TEMPERATURE + nl::TEMPERATURE_STEP * attempt as f32;
            if attempt > 0 {
                println!("; retry {attempt}/{MAX_RETRIES} at temperature {temperature:.1}");
            }
            let reply = match self.model.chat(&msgs, temperature) {
                Ok(r) => r,
                Err(e) => {
                    // Network trouble is not a program error: report and carry on.
                    eprintln!("model unavailable: {e:#}");
                    return Ok(());
                }
            };
            let code = nl::extract_clif(&reply);
            if code.trim().is_empty() || !code.contains("function") {
                let err = format!("the model replied without any CLIF: {}", nl::truncate(reply.trim(), 300));
                println!("; {err}");
                msgs.push(nl::Msg::assistant(reply));
                msgs.push(nl::retry_message(&err));
                last_error = Some(err);
                continue;
            }
            for line in code.lines() {
                println!("⚙ {line}");
            }
            match self.run_clif(&code) {
                Ok(outcome) => {
                    self.print_outcome(&outcome, None);
                    let summary = summarize(&outcome);
                    msgs.push(nl::Msg::assistant(code));
                    msgs.push(nl::Msg::user(summary));
                    self.remember(msgs);
                    return Ok(());
                }
                Err(e) => {
                    let err = format!("{e:#}");
                    for line in err.lines() {
                        println!("; {line}");
                    }
                    msgs.push(nl::Msg::assistant(code));
                    msgs.push(nl::retry_message(&err));
                    last_error = Some(err);
                }
            }
        }
        self.remember(msgs);
        let err = last_error.unwrap_or_else(|| "the model produced no usable CLIF".to_string());
        bail!("{MAX_RETRIES} retries exhausted; last failure: {err}");
    }

    /// Keep the tail of the conversation (never the system prompt) as history.
    fn remember(&mut self, mut msgs: Vec<nl::Msg>) {
        if !msgs.is_empty() {
            msgs.remove(0);
        }
        if msgs.len() > HISTORY_LIMIT {
            msgs.drain(..msgs.len() - HISTORY_LIMIT);
        }
        self.history = msgs;
    }
}

fn summarize(o: &Outcome) -> String {
    let mut s = String::new();
    for p in &o.prints {
        s.push_str("print: ");
        s.push_str(p);
        s.push('\n');
    }
    match &o.value {
        Some(v) => s.push_str(&format!("The scratchpad ran it. value: {v}")),
        None => s.push_str(&format!(
            "The scratchpad accepted it and defined: {}",
            o.defined.join(", ")
        )),
    }
    s
}

/// `{` and `}` counts on one line, ignoring `;` comments.
fn brace_counts(line: &str) -> (i32, i32) {
    let code = match line.find(';') {
        Some(i) => &line[..i],
        None => line,
    };
    let open = code.matches('{').count() as i32;
    let close = code.matches('}').count() as i32;
    (open, close)
}

fn encode_elem(ty: ElemType, text: &str) -> Result<Vec<u8>> {
    let t = text.trim();
    match ty {
        ElemType::I64 => Ok(parse_i64(t)?.to_ne_bytes().to_vec()),
        ElemType::F64 => Ok(t
            .parse::<f64>()
            .with_context(|| format!("bad f64 {t:?}"))?
            .to_ne_bytes()
            .to_vec()),
        ElemType::U8 => {
            let v = parse_i64(t)?;
            let byte = u8::try_from(v).map_err(|_| anyhow!("bad u8 {t:?}: expected 0..=255"))?;
            Ok(vec![byte])
        }
    }
}

/// Decimal, hex (`0x…`), binary (`0b…`) with optional `_` separators. Hex
/// accepts the full unsigned range so `0x8000_0000_0000_0000` works.
fn parse_i64(text: &str) -> Result<i64> {
    let t = text.replace('_', "");
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, t.trim_start_matches('+').to_string()),
    };
    let value = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).with_context(|| format!("bad hex integer {text:?}"))? as i64
    } else if let Some(bin) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        u64::from_str_radix(bin, 2).with_context(|| format!("bad binary integer {text:?}"))? as i64
    } else {
        body.parse::<i64>()
            .with_context(|| format!("bad integer {text:?}"))?
    };
    Ok(if neg { value.wrapping_neg() } else { value })
}

/// `"text"` with the usual escapes.
fn unquote(spec: &str) -> Result<String> {
    let body = spec
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .context("unterminated string: write :buf NAME u8 \"text\"")?;
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => bail!("unknown escape \\{other}"),
            None => bail!("trailing backslash"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braces_ignore_comments() {
        assert_eq!(brace_counts("function %main() -> i64 {"), (1, 0));
        assert_eq!(brace_counts("} ; closes { the function"), (0, 1));
        assert_eq!(brace_counts("    v0 = iconst.i64 1"), (0, 0));
    }

    #[test]
    fn integers_accept_hex_and_underscores() {
        assert_eq!(parse_i64("42").unwrap(), 42);
        assert_eq!(parse_i64("-7").unwrap(), -7);
        assert_eq!(parse_i64("1_000").unwrap(), 1000);
        assert_eq!(parse_i64("0x8000_0000_0000_0000").unwrap(), i64::MIN);
        assert_eq!(parse_i64("0b1011").unwrap(), 11);
        assert!(parse_i64("nope").is_err());
    }

    #[test]
    fn elements_encode_by_type() {
        assert_eq!(encode_elem(ElemType::I64, "3").unwrap(), 3i64.to_ne_bytes());
        assert_eq!(encode_elem(ElemType::F64, "0.5").unwrap(), 0.5f64.to_ne_bytes());
        assert_eq!(encode_elem(ElemType::U8, "255").unwrap(), vec![255]);
        assert!(encode_elem(ElemType::U8, "256").is_err());
    }

    #[test]
    fn strings_unescape() {
        assert_eq!(unquote("\"hi\"").unwrap(), "hi");
        assert_eq!(unquote("\"a\\nb\"").unwrap(), "a\nb");
        assert!(unquote("\"oops").is_err());
    }

    #[test]
    fn args_parse() {
        let argv: Vec<String> = ["--eval", "f.clif", "--asm", "--allow-unsafe"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse_args(&argv).unwrap();
        assert_eq!(a.eval.as_deref(), Some("f.clif"));
        assert!(a.asm && a.allow_unsafe && !a.offline);
        assert!(parse_args(&["--nope".to_string()]).is_err());
    }
}

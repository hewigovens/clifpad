//! The scratchpad session: CLIF text in, native execution and introspection out.
//! See DESIGN.md §4 and §5.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use cranelift::codegen::ir::{self, ExternalName, Function, GlobalValueData, UserExternalName};
use cranelift::codegen::isa::CallConv;
use cranelift::codegen::{self, settings};
use cranelift::prelude::{Configurable, Signature};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataId, FuncId, Linkage, Module, default_libcall_names};

use crate::runtime::{ElemType, HostFn, Runtime, host_table};

/// Result of `%main`, typed by its signature.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scalar {
    I64(i64),
    F64(f64),
    Unit,
}

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scalar::I64(v) => write!(f, "{v}"),
            Scalar::F64(v) => write!(f, "{v:?}"),
            Scalar::Unit => write!(f, "()"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct EvalOptions {
    /// Capture native disassembly into `Outcome::asm`.
    pub asm: bool,
}

/// What one evaluation produced. See DESIGN.md §2.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// `None` when the text defined functions but had no `%main`.
    pub value: Option<Scalar>,
    pub prints: Vec<String>,
    /// Optimized CLIF of every function compiled in this evaluation.
    pub clif: String,
    pub asm: Option<String>,
    pub compile_ns: u64,
    pub run_ns: u64,
    pub code_bytes: usize,
    /// Names of persisted functions this evaluation defined or replaced.
    pub defined: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BenchResult {
    pub iterations: usize,
    pub min_ns: u64,
    pub median_ns: u64,
    pub max_ns: u64,
    pub outcome: Outcome,
}

#[derive(Clone, Debug)]
pub struct FunctionInfo {
    pub name: String,
    pub signature: String,
}

#[derive(Clone, Debug)]
pub struct GlobalInfo {
    pub name: String,
    pub ty: ElemType,
    pub len: usize,
    pub addr: usize,
    pub preview: String,
}

#[derive(Clone, Debug, Default)]
pub struct StateReport {
    pub functions: Vec<FunctionInfo>,
    pub globals: Vec<GlobalInfo>,
    pub alloc_total: usize,
}

impl fmt::Display for StateReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.functions.is_empty() && self.globals.is_empty() {
            return write!(f, "(empty session)");
        }
        if !self.functions.is_empty() {
            writeln!(f, "functions:")?;
            for func in &self.functions {
                writeln!(f, "  %{}{}", func.name, func.signature)?;
            }
        }
        if !self.globals.is_empty() {
            writeln!(f, "globals:")?;
            for g in &self.globals {
                writeln!(
                    f,
                    "  %{:<16} {}[{}] @ {:#x}  {}",
                    g.name,
                    g.ty.name(),
                    g.len,
                    g.addr,
                    g.preview
                )?;
            }
        }
        Ok(())
    }
}

struct FnEntry {
    id: FuncId,
    sig: Signature,
}

struct Compiled {
    entry: Option<(*const u8, Signature)>,
    clif: String,
    asm: Option<String>,
    compile_ns: u64,
    code_bytes: usize,
    defined: Vec<String>,
}

type SymbolTable = Arc<Mutex<HashMap<String, usize>>>;

/// A scratchpad session: a JIT module, persisted functions, and session memory.
pub struct Session {
    module: JITModule,
    call_conv: CallConv,
    fns: HashMap<String, FnEntry>,
    datas: HashMap<String, DataId>,
    host: HashMap<&'static str, HostFn>,
    symbols: SymbolTable,
    runtime: Box<Runtime>,
    allow_unsafe: bool,
    counter: usize,
    /// Source of every evaluation that defined a persisted function, for `:save`.
    definitions: Vec<String>,
}

impl Session {
    /// `allow_unsafe` lets CLIF import any symbol `dlsym` can find (DESIGN.md §8).
    pub fn new(allow_unsafe: bool) -> Result<Self> {
        let symbols: SymbolTable = Arc::new(Mutex::new(HashMap::new()));
        let (module, call_conv) = build_module(&symbols)?;
        Ok(Self {
            module,
            call_conv,
            fns: HashMap::new(),
            datas: HashMap::new(),
            host: host_table().into_iter().map(|h| (h.name, h)).collect(),
            symbols,
            runtime: Runtime::new(),
            allow_unsafe,
            counter: 0,
            definitions: Vec::new(),
        })
    }

    pub fn allow_unsafe(&self) -> bool {
        self.allow_unsafe
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Mutable access to session memory, e.g. to fill a buffer from Rust.
    pub fn runtime_mut(&mut self) -> &mut Runtime {
        &mut self.runtime
    }

    /// Heuristic used by the input router: does this look like CLIF text?
    pub fn looks_like_clif(text: &str) -> bool {
        text.contains("function %")
    }

    /// Compile and run `text`. Functions other than `%main` persist.
    pub fn eval(&mut self, text: &str, opts: &EvalOptions) -> Result<Outcome> {
        let compiled = self.compile(text, opts)?;
        let (value, prints, run_ns) = match &compiled.entry {
            Some((ptr, sig)) => {
                let (v, p, ns) = self.run_entry(*ptr, sig)?;
                (Some(v), p, ns)
            }
            None => (None, Vec::new(), 0),
        };
        Ok(Outcome {
            value,
            prints,
            clif: compiled.clif,
            asm: compiled.asm,
            compile_ns: compiled.compile_ns,
            run_ns,
            code_bytes: compiled.code_bytes,
            defined: compiled.defined,
        })
    }

    /// Compile once, run `%main` `iterations` times, report timing of the runs.
    pub fn bench(&mut self, text: &str, iterations: usize, opts: &EvalOptions) -> Result<BenchResult> {
        if iterations == 0 {
            bail!("bench needs at least one iteration");
        }
        let compiled = self.compile(text, opts)?;
        let Some((ptr, sig)) = compiled.entry.clone() else {
            bail!("bench needs a %main function");
        };
        let mut times = Vec::with_capacity(iterations);
        let mut last = None;
        for _ in 0..iterations {
            let (v, p, ns) = self.run_entry(ptr, &sig)?;
            times.push(ns);
            last = Some((v, p));
        }
        times.sort_unstable();
        let (value, prints) = last.unwrap();
        Ok(BenchResult {
            iterations,
            min_ns: times[0],
            median_ns: times[times.len() / 2],
            max_ns: times[times.len() - 1],
            outcome: Outcome {
                value: Some(value),
                prints,
                clif: compiled.clif,
                asm: compiled.asm,
                compile_ns: compiled.compile_ns,
                run_ns: times[times.len() / 2],
                code_bytes: compiled.code_bytes,
                defined: compiled.defined,
            },
        })
    }

    /// `:global name i64` / `:buf name f64 1024`. Returns the address.
    pub fn declare_global(&mut self, name: &str, ty: ElemType, len: usize) -> Result<usize> {
        let addr = self.runtime.declare(name, ty, len)?;
        self.symbols.lock().unwrap().insert(name.to_string(), addr);
        Ok(addr)
    }

    /// `:buf name u8 "text"`: bytes plus an `i64` slot `<name>_len`.
    pub fn declare_bytes(&mut self, name: &str, data: &[u8]) -> Result<usize> {
        let addr = self.runtime.declare_bytes(name, data)?;
        let len_addr = self.runtime.global_addr(&format!("{name}_len")).unwrap();
        let mut syms = self.symbols.lock().unwrap();
        syms.insert(name.to_string(), addr);
        syms.insert(format!("{name}_len"), len_addr);
        Ok(addr)
    }

    /// NUL-terminated variant of `declare_bytes` for C-string FFI calls.
    pub fn declare_cstr(&mut self, name: &str, text: &str) -> Result<usize> {
        let addr = self.runtime.declare_cstr(name, text)?;
        let len_addr = self.runtime.global_addr(&format!("{name}_len")).unwrap();
        let mut syms = self.symbols.lock().unwrap();
        syms.insert(name.to_string(), addr);
        syms.insert(format!("{name}_len"), len_addr);
        Ok(addr)
    }

    pub fn state(&self) -> StateReport {
        let mut functions: Vec<FunctionInfo> = self
            .fns
            .iter()
            .map(|(name, e)| FunctionInfo {
                name: name.clone(),
                signature: display_signature(&e.sig),
            })
            .collect();
        functions.sort_by(|a, b| a.name.cmp(&b.name));
        let globals = self
            .runtime
            .globals
            .iter()
            .map(|(name, g)| GlobalInfo {
                name: name.clone(),
                ty: g.ty,
                len: g.len,
                addr: g.addr(),
                preview: g.preview(8),
            })
            .collect();
        StateReport {
            functions,
            globals,
            alloc_total: self.runtime.alloc_total,
        }
    }

    /// Source text of every evaluation that defined a persisted function, in order.
    pub fn definitions(&self) -> &[String] {
        &self.definitions
    }

    /// Drop all functions, globals and allocations. Rebuilds the JIT module.
    pub fn reset(&mut self) -> Result<()> {
        self.symbols.lock().unwrap().clear();
        let (module, call_conv) = build_module(&self.symbols)?;
        // The old module's code is leaked deliberately: function pointers may
        // still be referenced by the old runtime until it is dropped below.
        self.module = module;
        self.call_conv = call_conv;
        self.fns.clear();
        self.datas.clear();
        self.runtime = Runtime::new();
        self.counter = 0;
        self.definitions.clear();
        Ok(())
    }

    fn compile(&mut self, text: &str, opts: &EvalOptions) -> Result<Compiled> {
        let t0 = Instant::now();
        let text = hexify_floats(text);
        let mut funcs =
            cranelift_reader::parse_functions(&text).map_err(|e| anyhow!("parse error: {e}"))?;
        if funcs.is_empty() {
            bail!("no functions found; expected `function %main() -> i64 {{ ... }}`");
        }

        // Pass 1: declare every function under a fresh internal name.
        let mut entry: Option<(FuncId, Signature)> = None;
        let mut order: Vec<(String, FuncId)> = Vec::new();
        let mut defined = Vec::new();
        for f in &mut funcs {
            f.signature.call_conv = self.call_conv;
            for (_, s) in f.dfg.signatures.iter_mut() {
                s.call_conv = self.call_conv;
            }
            let name = func_name(f);
            self.counter += 1;
            let internal = format!("{name}@{}", self.counter);
            let id = self
                .module
                .declare_function(&internal, Linkage::Export, &f.signature)
                .with_context(|| format!("declare %{name}"))?;
            if name == "main" {
                if !f.signature.params.is_empty() {
                    bail!("%main must take no parameters");
                }
                if f.signature.returns.len() > 1 {
                    bail!("%main may return at most one value");
                }
                if let Some(r) = f.signature.returns.first()
                    && r.value_type != ir::types::I64
                    && r.value_type != ir::types::F64
                {
                    bail!("%main must return i64, f64 or nothing, not {}", r.value_type);
                }
                entry = Some((id, f.signature.clone()));
            } else {
                self.fns.insert(
                    name.clone(),
                    FnEntry {
                        id,
                        sig: f.signature.clone(),
                    },
                );
                defined.push(name.clone());
            }
            order.push((name, id));
        }

        // Pass 2: resolve imports (functions and global symbols).
        for f in &mut funcs {
            self.resolve_imports(f)?;
        }

        // Pass 3: verify, compile, and collect IR / disassembly.
        let mut clif = String::new();
        let mut asm = String::new();
        let mut code_bytes = 0;
        let mut ctx = self.module.make_context();
        for (f, (name, id)) in funcs.into_iter().zip(order) {
            if let Err(errors) = codegen::verify_function(&f, self.module.isa()) {
                bail!(
                    "verifier rejected %{name}:\n{}",
                    codegen::print_errors::pretty_verifier_error(&f, None, errors)
                );
            }
            ctx.func = f;
            ctx.want_disasm = opts.asm;
            self.module
                .define_function(id, &mut ctx)
                .map_err(|e| anyhow!("compile error in %{name}: {e}"))?;
            clif.push_str(&ctx.func.display().to_string());
            clif.push('\n');
            if let Some(code) = ctx.compiled_code() {
                code_bytes += code.code_buffer().len();
                if opts.asm && let Some(v) = &code.vcode {
                    asm.push_str(&format!("; %{name}\n{v}\n"));
                }
            }
            self.module.clear_context(&mut ctx);
        }
        self.module
            .finalize_definitions()
            .context("finalize JIT code")?;
        if !defined.is_empty() {
            self.definitions.push(text.trim().to_string());
        }
        let compile_ns = t0.elapsed().as_nanos() as u64;
        let entry = entry.map(|(id, sig)| (self.module.get_finalized_function(id), sig));
        Ok(Compiled {
            entry,
            clif,
            asm: opts.asm.then_some(asm),
            compile_ns,
            code_bytes,
            defined,
        })
    }

    fn resolve_imports(&mut self, f: &mut Function) -> Result<()> {
        let refs: Vec<_> = f.dfg.ext_funcs.keys().collect();
        for fref in refs {
            let name = ext_name(&f.dfg.ext_funcs[fref].name);
            let sig = f.dfg.signatures[f.dfg.ext_funcs[fref].signature].clone();
            let id = if let Some(e) = self.fns.get(&name) {
                if !same_signature(&e.sig, &sig) {
                    bail!(
                        "import %{name} declared as {} but the session function is {}",
                        display_signature(&sig),
                        display_signature(&e.sig)
                    );
                }
                e.id
            } else if let Some(h) = self.host.get(name.as_str()) {
                check_host_signature(h, &sig)?;
                self.module
                    .declare_function(&name, Linkage::Import, &sig)
                    .with_context(|| format!("declare host import %{name}"))?
            } else if self.allow_unsafe {
                self.module
                    .declare_function(&name, Linkage::Import, &sig)
                    .with_context(|| format!("declare dlsym import %{name}"))?
            } else {
                bail!(
                    "unknown function %{name}: not a host function (rt_*) or a session function; \
                     process symbols need --allow-unsafe"
                );
            };
            let r = f.declare_imported_user_function(UserExternalName {
                namespace: 0,
                index: id.as_u32(),
            });
            f.dfg.ext_funcs[fref].name = ExternalName::user(r);
        }

        let gvs: Vec<_> = f.global_values.keys().collect();
        for gv in gvs {
            let GlobalValueData::Symbol { name, .. } = &f.global_values[gv] else {
                continue;
            };
            let sym = ext_name(name);
            if !self.symbols.lock().unwrap().contains_key(&sym) {
                bail!(
                    "unknown symbol %{sym}: declare it first with :global or :buf (see :state)"
                );
            }
            let id = match self.datas.get(&sym) {
                Some(&id) => id,
                None => {
                    let id = self
                        .module
                        .declare_data(&sym, Linkage::Import, true, false)
                        .with_context(|| format!("declare data %{sym}"))?;
                    self.datas.insert(sym.clone(), id);
                    id
                }
            };
            let r = f.declare_imported_user_function(UserExternalName {
                namespace: 1,
                index: id.as_u32(),
            });
            if let GlobalValueData::Symbol { name, .. } = &mut f.global_values[gv] {
                *name = ExternalName::user(r);
            }
        }
        Ok(())
    }

    fn run_entry(&mut self, ptr: *const u8, sig: &Signature) -> Result<(Scalar, Vec<String>, u64)> {
        let guard = self.runtime.begin_eval();
        let t0 = Instant::now();
        // SAFETY: `ptr` is finalized JIT code with exactly this signature,
        // rewritten to the host calling convention in `compile`.
        let value = unsafe {
            match sig.returns.first().map(|r| r.value_type) {
                None => {
                    let f: extern "C" fn() = std::mem::transmute(ptr);
                    f();
                    Scalar::Unit
                }
                Some(t) if t == ir::types::I64 => {
                    let f: extern "C" fn() -> i64 = std::mem::transmute(ptr);
                    Scalar::I64(f())
                }
                Some(t) if t == ir::types::F64 => {
                    let f: extern "C" fn() -> f64 = std::mem::transmute(ptr);
                    Scalar::F64(f())
                }
                Some(t) => bail!("unsupported %main return type {t}"),
            }
        };
        let run_ns = t0.elapsed().as_nanos() as u64;
        drop(guard);
        let prints = std::mem::take(&mut self.runtime.prints);
        if let Some(err) = self.runtime.error.take() {
            bail!("runtime error: {err}");
        }
        Ok((value, prints, run_ns))
    }
}

fn build_module(symbols: &SymbolTable) -> Result<(JITModule, CallConv)> {
    let mut flags = settings::builder();
    flags.set("opt_level", "speed")?;
    flags.set("is_pic", "false")?;
    let isa = cranelift_native::builder()
        .map_err(|e| anyhow!("host machine not supported by Cranelift: {e}"))?
        .finish(settings::Flags::new(flags))?;
    let call_conv = isa.default_call_conv();
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    for h in host_table() {
        builder.symbol(h.name, h.ptr);
    }
    let table = symbols.clone();
    builder.symbol_lookup_fn(Box::new(move |name| {
        table.lock().unwrap().get(name).map(|&a| a as *const u8)
    }));
    Ok((JITModule::new(builder), call_conv))
}

fn func_name(f: &Function) -> String {
    f.name.to_string().trim_start_matches('%').to_string()
}

fn ext_name(n: &ExternalName) -> String {
    match n {
        ExternalName::TestCase(t) => t.to_string().trim_start_matches('%').to_string(),
        other => format!("{other:?}"),
    }
}

fn same_signature(a: &Signature, b: &Signature) -> bool {
    a.params == b.params && a.returns == b.returns
}

fn check_host_signature(h: &HostFn, sig: &Signature) -> Result<()> {
    let params: Vec<_> = sig.params.iter().map(|p| p.value_type).collect();
    let ret: Vec<_> = sig.returns.iter().map(|r| r.value_type).collect();
    let expected_ret: Vec<_> = h.ret.into_iter().collect();
    if params != h.params || ret != expected_ret {
        bail!(
            "host function %{} has signature ({}){} but the import declares {}",
            h.name,
            h.params
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            h.ret.map(|t| format!(" -> {t}")).unwrap_or_default(),
            display_signature(sig)
        );
    }
    Ok(())
}

/// `(i64, f64) -> i64`, without the calling convention.
pub fn display_signature(sig: &Signature) -> String {
    let params: Vec<String> = sig.params.iter().map(|p| p.value_type.to_string()).collect();
    let mut s = format!("({})", params.join(", "));
    if !sig.returns.is_empty() {
        let rets: Vec<String> = sig.returns.iter().map(|r| r.value_type.to_string()).collect();
        s.push_str(" -> ");
        s.push_str(&rets.join(", "));
    }
    s
}

/// cranelift-reader accepts only hexadecimal float literals. Rewrite decimal
/// literals after `f64const` / `f32const` so a model can write `0.1`.
pub fn hexify_floats(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.lines() {
        out.push_str(&hexify_line(line));
        out.push('\n');
    }
    out
}

fn hexify_line(line: &str) -> String {
    for kw in ["f64const", "f32const"] {
        let Some(i) = line.find(kw) else { continue };
        let (head, rest) = line.split_at(i + kw.len());
        let trimmed = rest.trim_start();
        let lead = rest.len() - trimmed.len();
        let end = trimmed
            .find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(trimmed.len());
        let lit = &trimmed[..end];
        let tail = &trimmed[end..];
        let is_hex = lit.trim_start_matches(['+', '-']).starts_with("0x");
        let is_decimal = lit
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
        if is_hex || lit.is_empty() || !is_decimal {
            return line.to_string();
        }
        let Ok(v) = lit.parse::<f64>() else {
            return line.to_string();
        };
        let hex = if kw == "f64const" {
            ir::immediates::Ieee64::with_float(v).to_string()
        } else {
            ir::immediates::Ieee32::with_float(v as f32).to_string()
        };
        return format!("{head}{}{hex}{tail}", &rest[..lead]);
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hexify_only_touches_decimal_literals() {
        assert_eq!(hexify_line("    v0 = f64const 0x1.8p1"), "    v0 = f64const 0x1.8p1");
        assert_eq!(hexify_line("    v0 = f64const NaN"), "    v0 = f64const NaN");
        assert_eq!(
            hexify_line("    v0 = f64const 3.0 ; x"),
            "    v0 = f64const 0x1.8000000000000p1 ; x"
        );
        assert_eq!(hexify_line("    v0 = f64const 1e3"), "    v0 = f64const 0x1.f400000000000p9");
        assert_eq!(hexify_line("block0:"), "block0:");
    }
}

//! Session-owned memory (globals, buffers, allocations) and the host ABI that
//! JIT code can call. See DESIGN.md §4.3 and §4.4.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ptr;
use std::time::Instant;

use anyhow::{Result, bail};
use cranelift::prelude::{Type, types};

/// Element type of a global slot or buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElemType {
    I64,
    F64,
    U8,
}

impl ElemType {
    pub fn size(self) -> usize {
        match self {
            ElemType::I64 | ElemType::F64 => 8,
            ElemType::U8 => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ElemType::I64 => "i64",
            ElemType::F64 => "f64",
            ElemType::U8 => "u8",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "i64" => Some(ElemType::I64),
            "f64" => Some(ElemType::F64),
            "u8" => Some(ElemType::U8),
            _ => None,
        }
    }
}

/// A named, session-owned, 8-byte-aligned memory region with a stable address.
pub struct Global {
    pub ty: ElemType,
    pub len: usize,
    mem: Vec<u64>,
}

impl Global {
    fn new(ty: ElemType, len: usize) -> Self {
        let words = (ty.size() * len).div_ceil(8).max(1);
        Self {
            ty,
            len,
            mem: vec![0u64; words],
        }
    }

    pub fn addr(&self) -> usize {
        self.mem.as_ptr() as usize
    }

    pub fn byte_len(&self) -> usize {
        self.ty.size() * self.len
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: mem holds at least byte_len() bytes of initialized memory.
        unsafe { std::slice::from_raw_parts(self.mem.as_ptr() as *const u8, self.byte_len()) }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        let n = self.byte_len();
        // SAFETY: as above, and we hold &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.mem.as_mut_ptr() as *mut u8, n) }
    }

    /// Human-readable preview of the first few elements.
    pub fn preview(&self, max: usize) -> String {
        let b = self.bytes();
        let mut items: Vec<String> = Vec::new();
        match self.ty {
            ElemType::I64 => {
                for c in b.chunks_exact(8).take(max) {
                    items.push(i64::from_ne_bytes(c.try_into().unwrap()).to_string());
                }
            }
            ElemType::F64 => {
                for c in b.chunks_exact(8).take(max) {
                    items.push(f64::from_ne_bytes(c.try_into().unwrap()).to_string());
                }
            }
            ElemType::U8 => {
                let shown = &b[..b.len().min(max.max(16))];
                if shown.iter().all(|&c| c.is_ascii_graphic() || c == b' ') {
                    let s = String::from_utf8_lossy(shown);
                    return if shown.len() < b.len() {
                        format!("{s:?}…")
                    } else {
                        format!("{s:?}")
                    };
                }
                for &c in shown {
                    items.push(format!("{c:#04x}"));
                }
            }
        }
        let more = if self.len > items.len() { ", …" } else { "" };
        format!("[{}{more}]", items.join(", "))
    }
}

/// Everything JIT code can touch through the host ABI. One per `Session`.
pub struct Runtime {
    pub globals: BTreeMap<String, Global>,
    pub prints: Vec<String>,
    pub error: Option<String>,
    allocs: Vec<Vec<u64>>,
    pub alloc_total: usize,
    pub max_alloc: usize,
    pub max_total: usize,
    start: Instant,
}

thread_local! {
    static CURRENT: Cell<*mut Runtime> = const { Cell::new(ptr::null_mut()) };
}

/// Clears the thread-local runtime pointer when an evaluation ends.
pub struct EvalGuard(());

impl Drop for EvalGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(ptr::null_mut()));
    }
}

impl Runtime {
    pub fn new() -> Box<Self> {
        Box::new(Self {
            globals: BTreeMap::new(),
            prints: Vec::new(),
            error: None,
            allocs: Vec::new(),
            alloc_total: 0,
            max_alloc: 256 << 20,
            max_total: 1 << 30,
            start: Instant::now(),
        })
    }

    /// Create a global. Re-declaring with the same type and length is a no-op;
    /// anything else is an error because JIT code may already hold the address.
    pub fn declare(&mut self, name: &str, ty: ElemType, len: usize) -> Result<usize> {
        check_name(name)?;
        if let Some(g) = self.globals.get(name) {
            if g.ty == ty && g.len == len {
                return Ok(g.addr());
            }
            bail!(
                "global %{name} already exists as {}[{}]; names bind once per session, use :reset",
                g.ty.name(),
                g.len
            );
        }
        let bytes = ty.size() * len;
        if bytes > self.max_alloc {
            bail!("global %{name} would be {bytes} bytes, limit is {}", self.max_alloc);
        }
        self.charge(bytes)?;
        let g = Global::new(ty, len);
        let addr = g.addr();
        self.globals.insert(name.to_string(), g);
        Ok(addr)
    }

    /// Create a `u8` global holding `data` plus an `i64` slot `<name>_len`.
    pub fn declare_bytes(&mut self, name: &str, data: &[u8]) -> Result<usize> {
        let len_name = format!("{name}_len");
        if let Some(g) = self.globals.get(name) {
            if g.ty == ElemType::U8 && g.len == data.len() && g.bytes() == data {
                return Ok(g.addr());
            }
            bail!("global %{name} already exists; names bind once per session, use :reset");
        }
        let addr = self.declare(name, ElemType::U8, data.len())?;
        self.globals.get_mut(name).unwrap().bytes_mut().copy_from_slice(data);
        self.declare(&len_name, ElemType::I64, 1)?;
        self.globals
            .get_mut(&len_name)
            .unwrap()
            .bytes_mut()
            .copy_from_slice(&(data.len() as i64).to_ne_bytes());
        Ok(addr)
    }

    pub fn global_addr(&self, name: &str) -> Option<usize> {
        self.globals.get(name).map(Global::addr)
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        if self.alloc_total + bytes > self.max_total {
            bail!(
                "allocation of {bytes} bytes would exceed the session limit of {} bytes",
                self.max_total
            );
        }
        self.alloc_total += bytes;
        Ok(())
    }

    /// Make this runtime the target of host calls for the current thread and
    /// clear per-evaluation state. Keep the guard alive while JIT code runs.
    pub fn begin_eval(&mut self) -> EvalGuard {
        self.prints.clear();
        self.error = None;
        CURRENT.with(|c| c.set(self as *mut Runtime));
        EvalGuard(())
    }

    fn with<R>(f: impl FnOnce(&mut Runtime) -> R) -> Option<R> {
        let p = CURRENT.with(Cell::get);
        if p.is_null() {
            return None;
        }
        // SAFETY: the pointer was set by begin_eval on this thread and the
        // EvalGuard clears it before the Runtime can be dropped or moved.
        Some(f(unsafe { &mut *p }))
    }
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if !ok {
        bail!("invalid symbol name {name:?}: use letters, digits and underscores");
    }
    if name.starts_with("rt_") || name.starts_with("__") {
        bail!("symbol name {name:?} is reserved");
    }
    Ok(())
}

/// One entry of the host ABI table.
pub struct HostFn {
    pub name: &'static str,
    pub ptr: *const u8,
    pub params: &'static [Type],
    pub ret: Option<Type>,
    pub doc: &'static str,
}

pub fn host_table() -> Vec<HostFn> {
    vec![
        HostFn {
            name: "rt_print_i64",
            ptr: rt_print_i64 as *const u8,
            params: &[types::I64],
            ret: None,
            doc: "print an integer",
        },
        HostFn {
            name: "rt_print_f64",
            ptr: rt_print_f64 as *const u8,
            params: &[types::F64],
            ret: None,
            doc: "print a float",
        },
        HostFn {
            name: "rt_print_str",
            ptr: rt_print_str as *const u8,
            params: &[types::I64, types::I64],
            ret: None,
            doc: "print UTF-8 bytes (ptr, len)",
        },
        HostFn {
            name: "rt_now_ns",
            ptr: rt_now_ns as *const u8,
            params: &[],
            ret: Some(types::I64),
            doc: "monotonic clock in nanoseconds",
        },
        HostFn {
            name: "rt_alloc",
            ptr: rt_alloc as *const u8,
            params: &[types::I64],
            ret: Some(types::I64),
            doc: "allocate zeroed bytes owned by the session, returns address (0 on failure)",
        },
        HostFn {
            name: "rt_fail",
            ptr: rt_fail as *const u8,
            params: &[types::I64],
            ret: None,
            doc: "abort the evaluation with an error code",
        },
    ]
}

extern "C" fn rt_print_i64(v: i64) {
    Runtime::with(|r| r.prints.push(v.to_string()));
}

extern "C" fn rt_print_f64(v: f64) {
    Runtime::with(|r| r.prints.push(v.to_string()));
}

extern "C" fn rt_print_str(ptr: i64, len: i64) {
    Runtime::with(|r| {
        if ptr <= 0 || len < 0 || len > (64 << 20) {
            r.prints.push(format!("<invalid string ptr={ptr} len={len}>"));
            return;
        }
        // SAFETY: the caller asserts ptr..ptr+len is readable; this is JIT
        // code and is inherently unchecked (DESIGN.md §8).
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
        r.prints.push(String::from_utf8_lossy(bytes).into_owned());
    });
}

extern "C" fn rt_now_ns() -> i64 {
    Runtime::with(|r| r.start.elapsed().as_nanos() as i64).unwrap_or(0)
}

extern "C" fn rt_alloc(bytes: i64) -> i64 {
    Runtime::with(|r| {
        if bytes < 0 || bytes as usize > r.max_alloc {
            r.error = Some(format!(
                "rt_alloc({bytes}) refused: limit is {} bytes per allocation",
                r.max_alloc
            ));
            return 0;
        }
        if let Err(e) = r.charge(bytes as usize) {
            r.error = Some(e.to_string());
            return 0;
        }
        let words = (bytes as usize).div_ceil(8).max(1);
        let v = vec![0u64; words];
        let addr = v.as_ptr() as i64;
        r.allocs.push(v);
        addr
    })
    .unwrap_or(0)
}

extern "C" fn rt_fail(code: i64) {
    Runtime::with(|r| r.error = Some(format!("rt_fail({code}) called by JIT code")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declare_is_idempotent_for_same_shape() {
        let mut rt = Runtime::new();
        let a = rt.declare("xs", ElemType::F64, 4).unwrap();
        let b = rt.declare("xs", ElemType::F64, 4).unwrap();
        assert_eq!(a, b);
        assert!(rt.declare("xs", ElemType::I64, 4).is_err());
    }

    #[test]
    fn bytes_global_gets_len_slot() {
        let mut rt = Runtime::new();
        rt.declare_bytes("hello", b"hello world").unwrap();
        assert_eq!(rt.globals["hello"].bytes(), b"hello world");
        assert_eq!(rt.globals["hello_len"].preview(8), "[11]");
        assert_eq!(rt.globals["hello"].preview(8), "\"hello world\"");
    }

    #[test]
    fn host_calls_need_a_current_runtime() {
        rt_print_i64(1); // no runtime: silently ignored
        let mut rt = Runtime::new();
        {
            let _g = rt.begin_eval();
            rt_print_i64(7);
            rt_print_f64(0.5);
        }
        assert_eq!(rt.prints, vec!["7", "0.5"]);
        rt_print_i64(9); // guard dropped: ignored
        assert_eq!(rt.prints.len(), 2);
    }
}

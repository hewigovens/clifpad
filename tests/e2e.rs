//! Acceptance tests for the CLIF scratchpad session (DESIGN.md §9, milestone M1).
//!
//! Every test drives the public library surface only: `Session`, `EvalOptions`,
//! `Scalar`, `ElemType`. CLIF follows the conventions of DESIGN.md §4: no
//! calling convention in the text, no `*_imm` instructions (cranelift 0.135
//! removed them), decimal float literals are fine because the session rewrites
//! them to hex before parsing.

use clifpad::{ElemType, EvalOptions, Scalar, Session};

// ---------------------------------------------------------------- helpers

fn plain() -> EvalOptions {
    EvalOptions { asm: false }
}

/// A session with `--allow-unsafe` off, which is the default posture (§8).
fn session() -> Session {
    Session::new(false).expect("build session")
}

/// Evaluate and unwrap, printing the error so a failure is diagnosable.
fn eval_ok(s: &mut Session, text: &str) -> clifpad::Outcome {
    match s.eval(text, &plain()) {
        Ok(o) => o,
        Err(e) => panic!("expected success, got error:\n{e:#}\n--- source ---\n{text}"),
    }
}

/// Evaluate and unwrap the error, printing the outcome if it unexpectedly ran.
fn eval_err(s: &mut Session, text: &str) -> String {
    match s.eval(text, &plain()) {
        Ok(o) => panic!("expected an error, got value {:?}\n--- source ---\n{text}", o.value),
        Err(e) => format!("{e:#}"),
    }
}

fn i64_value(o: &clifpad::Outcome) -> i64 {
    match o.value {
        Some(Scalar::I64(v)) => v,
        other => panic!("expected an i64 result, got {other:?}"),
    }
}

fn f64_value(o: &clifpad::Outcome) -> f64 {
    match o.value {
        Some(Scalar::F64(v)) => v,
        other => panic!("expected an f64 result, got {other:?}"),
    }
}

/// Write `values` into a session `f64` global in native byte order.
fn fill_f64(s: &mut Session, name: &str, values: &[f64]) {
    let g = s
        .runtime_mut()
        .globals
        .get_mut(name)
        .unwrap_or_else(|| panic!("global %{name} was not declared"));
    assert_eq!(g.len, values.len(), "global %{name} has the wrong length");
    let bytes = g.bytes_mut();
    for (i, v) in values.iter().enumerate() {
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&v.to_ne_bytes());
    }
}

// ------------------------------------------------------------ 1. constants

#[test]
fn const_main_returns_i64() {
    let mut s = session();
    let o = eval_ok(
        &mut s,
        r#"
function %main() -> i64 {
block0:
    v0 = iconst.i64 42
    return v0
}
"#,
    );
    assert_eq!(o.value, Some(Scalar::I64(42)));
    assert!(o.prints.is_empty());
    assert!(o.defined.is_empty(), "%main must not persist as a session function");
    assert!(o.code_bytes > 0, "some machine code should have been emitted");
    assert!(o.asm.is_none(), "asm is opt-in");
}

#[test]
fn f64_main_and_decimal_literals() {
    // The reader rejects decimal float literals; the session rewrites them to
    // hex first (DESIGN.md §4.2), so 0.1 and 0.2 are accepted verbatim.
    let mut s = session();
    let o = eval_ok(
        &mut s,
        r#"
function %main() -> f64 {
block0:
    v0 = f64const 0.1
    v1 = f64const 0.2
    v2 = fadd v0, v1
    return v2
}
"#,
    );
    assert_eq!(o.value, Some(Scalar::F64(0.30000000000000004)));
}

// ------------------------------------------------------------- 2. host ABI

#[test]
fn unit_main_and_host_print() {
    let mut s = session();
    let o = eval_ok(
        &mut s,
        r#"
function %main() {
    sig0 = (i64)
    sig1 = (f64)
    fn0 = %rt_print_i64 sig0
    fn1 = %rt_print_f64 sig1
block0:
    v0 = iconst.i64 7
    call fn0(v0)
    v1 = f64const 2.5
    call fn1(v1)
    return
}
"#,
    );
    assert_eq!(o.value, Some(Scalar::Unit));
    assert_eq!(o.prints, vec!["7".to_string(), "2.5".to_string()]);
}

#[test]
fn host_signature_mismatch_is_rejected() {
    let mut s = session();
    let err = eval_err(
        &mut s,
        r#"
function %main() -> i64 {
    sig0 = (f64)
    fn0 = %rt_print_i64 sig0
block0:
    v0 = f64const 1.0
    call fn0(v0)
    v1 = iconst.i64 0
    return v1
}
"#,
    );
    assert!(err.contains("rt_print_i64"), "error should name the host function: {err}");
    assert!(err.contains("(f64)"), "error should quote the bad import: {err}");
}

// --------------------------------------------------- 3. session functions

#[test]
fn functions_persist_across_evals() {
    let mut s = session();
    let def = eval_ok(
        &mut s,
        r#"
function %square(i64) -> i64 {
block0(v0: i64):
    v1 = imul v0, v0
    return v1
}
"#,
    );
    assert_eq!(def.defined, vec!["square".to_string()]);
    assert_eq!(def.value, None, "text with no %main defines only");
    assert_eq!(def.run_ns, 0);

    let use_it = eval_ok(
        &mut s,
        r#"
function %main() -> i64 {
    sig0 = (i64) -> i64
    fn0 = %square sig0
block0:
    v0 = iconst.i64 12
    v1 = call fn0(v0)
    return v1
}
"#,
    );
    assert_eq!(use_it.value, Some(Scalar::I64(144)));

    let state = s.state();
    assert_eq!(state.functions.len(), 1);
    assert_eq!(state.functions[0].name, "square");
    assert_eq!(state.functions[0].signature, "(i64) -> i64");
}

#[test]
fn redefinition_semantics() {
    // DESIGN.md §4.5: redefining a name gives future evaluations the new body,
    // while code compiled earlier keeps the pointer it was linked against.
    let mut s = session();
    eval_ok(
        &mut s,
        r#"
function %f() -> i64 {
block0:
    v0 = iconst.i64 1
    return v0
}
"#,
    );
    eval_ok(
        &mut s,
        r#"
function %caller() -> i64 {
    sig0 = () -> i64
    fn0 = %f sig0
block0:
    v0 = call fn0()
    return v0
}
"#,
    );
    let redef = eval_ok(
        &mut s,
        r#"
function %f() -> i64 {
block0:
    v0 = iconst.i64 2
    return v0
}
"#,
    );
    assert_eq!(redef.defined, vec!["f".to_string()]);

    let via_f = eval_ok(
        &mut s,
        r#"
function %main() -> i64 {
    sig0 = () -> i64
    fn0 = %f sig0
block0:
    v0 = call fn0()
    return v0
}
"#,
    );
    assert_eq!(via_f.value, Some(Scalar::I64(2)), "a new caller gets the new body");

    let via_caller = eval_ok(
        &mut s,
        r#"
function %main() -> i64 {
    sig0 = () -> i64
    fn0 = %caller sig0
block0:
    v0 = call fn0()
    return v0
}
"#,
    );
    assert_eq!(
        via_caller.value,
        Some(Scalar::I64(1)),
        "%caller was linked against the old %f and keeps it"
    );
}

#[test]
fn import_signature_mismatch_is_rejected() {
    let mut s = session();
    eval_ok(
        &mut s,
        r#"
function %square(i64) -> i64 {
block0(v0: i64):
    v1 = imul v0, v0
    return v1
}
"#,
    );
    let err = eval_err(
        &mut s,
        r#"
function %main() -> f64 {
    sig0 = (f64) -> f64
    fn0 = %square sig0
block0:
    v0 = f64const 2.0
    v1 = call fn0(v0)
    return v1
}
"#,
    );
    assert!(err.contains("square"), "error should name the import: {err}");
    assert!(err.contains("(i64) -> i64"), "error should show the real signature: {err}");
}

// ------------------------------------------------------- 4. error quality

#[test]
fn verifier_error_is_precise_and_session_survives() {
    // DESIGN.md §9 M1 acceptance: a missing block argument is reported as text
    // and the session stays usable.
    let mut s = session();
    let err = eval_err(
        &mut s,
        r#"
function %main() -> i64 {
block0:
    jump block1

block1(v1: i64):
    return v1
}
"#,
    );
    assert!(
        err.contains("mismatched argument count"),
        "verifier text should be pretty-printed: {err}"
    );
    assert!(err.contains("block1"), "verifier text should name the block: {err}");

    let after = eval_ok(
        &mut s,
        r#"
function %main() -> i64 {
block0:
    v0 = iconst.i64 5
    return v0
}
"#,
    );
    assert_eq!(after.value, Some(Scalar::I64(5)));
}

#[test]
fn parse_error_has_line_number() {
    // Cranelift 0.135 removed every `*_imm` instruction (DESIGN.md §4.2).
    let mut s = session();
    let err = eval_err(
        &mut s,
        r#"function %main() -> i64 {
block0:
    v0 = iconst.i64 1
    v1 = ishl_imm v0, 3
    return v1
}
"#,
    );
    assert!(err.starts_with("parse error"), "error should be tagged: {err}");
    assert!(err.contains("ishl_imm"), "error should quote the bad opcode: {err}");
    assert!(
        err.contains("4:"),
        "error should carry the line number of the offending line: {err}"
    );
    assert!(err.contains("   4 |     v1 = ishl_imm"), "error should echo the line: {err}");
    assert!(err.contains("hint: immediate forms do not exist"), "{err}");
}

#[test]
fn parse_error_hints_at_a_calling_convention() {
    let mut s = session();
    let err = eval_err(
        &mut s,
        "function %main() -> f64 {\n    sig0 = (f64) -> system_v\nblock0:\n    v0 = f64const 0.0\n    return v0\n}",
    );
    assert!(err.contains("   2 |     sig0 = (f64) -> system_v"), "{err}");
    assert!(err.contains("hint: do not write a calling convention"), "{err}");
}

#[test]
fn ir_and_asm_name_imports_and_f64_prints_look_like_floats() {
    let mut s = session();
    eval_ok(&mut s, "function %sq(i64) -> i64 {\nblock0(v0: i64):\n    v1 = imul v0, v0\n    return v1\n}");
    s.declare_global("xs", ElemType::F64, 1).unwrap();
    let text = "function %main() -> i64 {
    sig0 = (i64) -> i64
    fn0 = %sq sig0
    sig1 = (f64)
    fn1 = %rt_print_f64 sig1
    gv0 = symbol %xs
block0:
    v0 = symbol_value.i64 gv0
    v1 = load.f64 v0
    call fn1(v1)
    v2 = iconst.i64 3
    v3 = call fn0(v2)
    return v3
}";
    let o = s.eval(text, &EvalOptions { asm: true }).unwrap();
    assert_eq!(o.prints, ["0.0"]);
    assert!(o.clif.contains("fn0 = %sq sig0"), "{}", o.clif);
    assert!(o.clif.contains("gv0 = symbol %xs"), "{}", o.clif);
    let asm = o.asm.unwrap();
    assert!(asm.contains("%sq") && asm.contains("%rt_print_f64") && asm.contains("%xs"), "{asm}");
    assert!(!asm.contains("userextname") && !asm.contains("unwind "), "{asm}");
}

#[test]
fn unknown_symbol_is_rejected() {
    let mut s = session();
    let err = eval_err(
        &mut s,
        r#"
function %main() -> i64 {
    gv0 = symbol %nope
block0:
    v0 = symbol_value.i64 gv0
    return v0
}
"#,
    );
    assert!(err.contains("nope"), "error should name the symbol: {err}");
    assert!(err.contains(":global"), "error should point at the fix: {err}");
}

// ------------------------------------------------------------- 5. dlsym gate

#[test]
fn dlsym_is_gated() {
    const CALL_STRLEN: &str = r#"
function %main() -> i64 {
    sig0 = (i64) -> i64
    fn0 = %strlen sig0
    gv0 = symbol %hello
block0:
    v0 = symbol_value.i64 gv0
    v1 = call fn0(v0)
    return v1
}
"#;

    // Without the flag the import is refused before anything is compiled.
    let mut safe = session();
    safe.declare_global("hello", ElemType::U8, 6).unwrap();
    let err = eval_err(&mut safe, CALL_STRLEN);
    assert!(err.contains("strlen"), "error should name the symbol: {err}");
    assert!(err.contains("--allow-unsafe"), "error should name the gate: {err}");

    // With the flag, dlsym resolves the process symbol.
    // NOTE: `declare_bytes` stores exactly the bytes given and no NUL
    // terminator, so a C string has to be built by hand.
    let mut unsafe_session = Session::new(true).unwrap();
    unsafe_session.declare_global("hello", ElemType::U8, 6).unwrap();
    unsafe_session
        .runtime_mut()
        .globals
        .get_mut("hello")
        .unwrap()
        .bytes_mut()
        .copy_from_slice(b"hello\0");
    let o = unsafe_session.eval(CALL_STRLEN, &plain()).expect("strlen should resolve");
    assert_eq!(o.value, Some(Scalar::I64(5)));
}

// -------------------------------------------------- 6. globals and buffers

#[test]
fn globals_and_buffers() {
    let mut s = session();
    s.declare_global("counter", ElemType::I64, 1).unwrap();
    let o = eval_ok(
        &mut s,
        r#"
function %main() -> i64 {
    gv0 = symbol %counter
block0:
    v0 = symbol_value.i64 gv0
    v1 = load.i64 v0
    v2 = iconst.i64 5
    v3 = iadd v1, v2
    store v3, v0
    return v3
}
"#,
    );
    assert_eq!(o.value, Some(Scalar::I64(5)));

    let state = s.state();
    let counter = state
        .globals
        .iter()
        .find(|g| g.name == "counter")
        .expect(":state should list %counter");
    assert_eq!(counter.ty, ElemType::I64);
    assert_eq!(counter.len, 1);
    assert_eq!(counter.preview, "[5]", "the store must be visible from the host");

    // An f64 buffer filled from Rust and summed by a CLIF loop.
    let xs: Vec<f64> = (1..=8).map(|i| i as f64).collect();
    s.declare_global("xs", ElemType::F64, xs.len()).unwrap();
    fill_f64(&mut s, "xs", &xs);
    let sum = eval_ok(
        &mut s,
        r#"
function %main() -> f64 {
    gv0 = symbol %xs
block0:
    v0 = symbol_value.i64 gv0
    v1 = f64const 0.0
    v2 = iconst.i64 0
    jump block1(v2, v1)

block1(v3: i64, v4: f64):
    v5 = iconst.i64 8
    v6 = icmp slt v3, v5
    brif v6, block2, block3

block2:
    v7 = iconst.i64 3
    v8 = ishl v3, v7
    v9 = iadd v0, v8
    v10 = load.f64 v9
    v11 = fadd v4, v10
    v12 = iconst.i64 1
    v13 = iadd v3, v12
    jump block1(v13, v11)

block3:
    return v4
}
"#,
    );
    assert_eq!(sum.value, Some(Scalar::F64(36.0)));
}

#[test]
fn global_redeclare_rules() {
    // DESIGN.md §4.4: a name binds once per session because JITModule caches
    // the address on first resolution. Same shape is a no-op, anything else errors.
    let mut s = session();
    let a = s.declare_global("xs", ElemType::F64, 8).unwrap();
    let b = s.declare_global("xs", ElemType::F64, 8).unwrap();
    assert_eq!(a, b, "re-declaring the same shape must be a no-op");

    let ty_err = s.declare_global("xs", ElemType::I64, 8).unwrap_err().to_string();
    assert!(ty_err.contains("xs"), "{ty_err}");
    assert!(ty_err.contains("reset the session"), "error should point at a reset: {ty_err}");

    let len_err = s.declare_global("xs", ElemType::F64, 16).unwrap_err().to_string();
    assert!(len_err.contains("reset the session"), "error should point at a reset: {len_err}");
}

// ----------------------------------------------- 7. M1 performance bar (§9)

#[test]
fn dot_product_1m_under_2ms() {
    const N: usize = 1_000_000;
    const DOT: &str = r#"
function %dot(i64, i64, i64) -> f64 {
    ; a: ptr, b: ptr, n: len
block0(v0: i64, v1: i64, v2: i64):
    v3 = f64const 0.0
    v4 = iconst.i64 0
    jump block1(v4, v3)

block1(v5: i64, v6: f64):
    v7 = icmp slt v5, v2
    brif v7, block2, block3

block2:
    v8 = iconst.i64 3
    v9 = ishl v5, v8
    v10 = iadd v0, v9
    v11 = iadd v1, v9
    v12 = load.f64 v10
    v13 = load.f64 v11
    v14 = fma v12, v13, v6
    v15 = iconst.i64 1
    v16 = iadd v5, v15
    jump block1(v16, v14)

block3:
    return v6
}
"#;
    const MAIN: &str = r#"
function %main() -> f64 {
    sig0 = (i64, i64, i64) -> f64
    fn0 = %dot sig0
    gv0 = symbol %xs
    gv1 = symbol %ys
block0:
    v0 = symbol_value.i64 gv0
    v1 = symbol_value.i64 gv1
    v2 = iconst.i64 1000000
    v3 = call fn0(v0, v1, v2)
    return v3
}
"#;

    let xs: Vec<f64> = (0..N).map(|i| (i % 7) as f64).collect();
    let ys: Vec<f64> = (0..N).map(|i| (i % 3) as f64).collect();
    // Same operation, same order, same fused multiply-add as the CLIF loop, so
    // the two agree bit for bit.
    let mut expected = 0.0f64;
    for i in 0..N {
        expected = xs[i].mul_add(ys[i], expected);
    }

    let mut s = session();
    s.declare_global("xs", ElemType::F64, N).unwrap();
    s.declare_global("ys", ElemType::F64, N).unwrap();
    fill_f64(&mut s, "xs", &xs);
    fill_f64(&mut s, "ys", &ys);

    let def = eval_ok(&mut s, DOT);
    assert_eq!(def.defined, vec!["dot".to_string()]);

    let outcome = eval_ok(&mut s, MAIN);
    assert_eq!(f64_value(&outcome), expected, "the JIT dot product must match Rust exactly");
    // JIT code is native regardless of the Rust build profile, so the same bar
    // applies in debug and release.
    assert!(
        outcome.run_ns < 2_000_000,
        "M1 acceptance: 1M-element dot product must run in under 2 ms, took {} ns",
        outcome.run_ns
    );

    let bench = s.bench(MAIN, 20, &plain()).expect("bench %main");
    assert_eq!(bench.iterations, 20);
    assert_eq!(f64_value(&bench.outcome), expected);
    assert!(bench.min_ns <= bench.median_ns && bench.median_ns <= bench.max_ns);
    assert!(
        bench.min_ns < 2_000_000,
        "M1 acceptance: best of 20 runs must be under 2 ms, was {} ns",
        bench.min_ns
    );
    eprintln!(
        "dot 1M: first run {} ns, compile {} ns; bench min {} / median {} / max {} ns",
        outcome.run_ns, outcome.compile_ns, bench.min_ns, bench.median_ns, bench.max_ns
    );
}

// -------------------------------------------------------- 8. introspection

#[test]
fn asm_and_clif_are_reported() {
    const MUL: &str = r#"
function %main() -> i64 {
block0:
    v0 = iconst.i64 6
    v1 = iconst.i64 7
    v2 = imul v0, v1
    return v2
}
"#;
    let mut s = session();
    let o = s.eval(MUL, &EvalOptions { asm: true }).expect("eval with asm");
    assert_eq!(o.value, Some(Scalar::I64(42)));
    assert!(
        o.clif.contains("42"),
        "Outcome.clif is the optimized IR, so 6 * 7 is folded: {}",
        o.clif
    );
    let asm = o.asm.as_deref().expect("asm was requested");
    assert!(!asm.trim().is_empty(), "disassembly should not be empty");
    assert!(asm.contains("block0"), "disassembly should be annotated: {asm}");

    let quiet = s.eval(MUL, &plain()).expect("eval without asm");
    assert!(quiet.asm.is_none(), "asm must be opt-in");
}

// ------------------------------------------------- 9. runtime memory/errors

#[test]
fn rt_alloc_and_limits() {
    let mut s = session();
    let o = eval_ok(
        &mut s,
        r#"
function %main() -> i64 {
    sig0 = (i64) -> i64
    sig1 = (i64)
    fn0 = %rt_alloc sig0
    fn1 = %rt_print_i64 sig1
block0:
    v0 = iconst.i64 64
    v1 = call fn0(v0)
    call fn1(v1)
    v2 = iconst.i64 1234
    store v2, v1
    v3 = iconst.i64 56
    v4 = iadd v1, v3
    v5 = iconst.i64 5678
    store v5, v4
    v6 = load.i64 v1
    v7 = load.i64 v4
    v8 = iadd v6, v7
    return v8
}
"#,
    );
    assert_eq!(i64_value(&o), 1234 + 5678, "stores and loads through rt_alloc memory");
    let addr: i64 = o.prints[0].parse().expect("the printed address should be an integer");
    assert_ne!(addr, 0, "rt_alloc(64) must succeed");

    // Over the per-allocation limit (256 MiB by default, DESIGN.md §8): the
    // host returns 0 and sets the error flag, which turns the eval into an Err.
    let err = eval_err(
        &mut s,
        r#"
function %main() -> i64 {
    sig0 = (i64) -> i64
    fn0 = %rt_alloc sig0
block0:
    v0 = iconst.i64 0x7fff_ffff
    v1 = call fn0(v0)
    return v1
}
"#,
    );
    assert!(err.contains("rt_alloc"), "error should name the host call: {err}");
    assert!(err.contains("limit"), "error should explain the cap: {err}");

    // The session survives a refused allocation.
    let after = eval_ok(
        &mut s,
        "function %main() -> i64 {\nblock0:\n    v0 = iconst.i64 1\n    return v0\n}\n",
    );
    assert_eq!(after.value, Some(Scalar::I64(1)));
}

#[test]
fn rt_fail_aborts_with_error() {
    let mut s = session();
    let err = eval_err(
        &mut s,
        r#"
function %main() -> i64 {
    sig0 = (i64)
    fn0 = %rt_fail sig0
block0:
    v0 = iconst.i64 3
    call fn0(v0)
    v1 = iconst.i64 99
    return v1
}
"#,
    );
    assert!(err.contains("rt_fail(3)"), "error should carry the code: {err}");
}

// ------------------------------------------------------------- 10. session

#[test]
fn reset_clears_everything() {
    let mut s = session();
    eval_ok(
        &mut s,
        r#"
function %twice(i64) -> i64 {
block0(v0: i64):
    v1 = iadd v0, v0
    return v1
}
"#,
    );
    s.declare_global("buf", ElemType::I64, 4).unwrap();
    assert!(!s.state().functions.is_empty());
    assert!(!s.state().globals.is_empty());
    assert!(!s.definitions().is_empty());

    s.reset().expect("reset");

    let state = s.state();
    assert!(state.functions.is_empty(), "reset drops functions");
    assert!(state.globals.is_empty(), "reset drops globals");
    assert_eq!(state.alloc_total, 0);
    assert!(s.definitions().is_empty());
    assert_eq!(state.to_string(), "(empty session)");

    let err = eval_err(
        &mut s,
        r#"
function %main() -> i64 {
    sig0 = (i64) -> i64
    fn0 = %twice sig0
block0:
    v0 = iconst.i64 21
    v1 = call fn0(v0)
    return v1
}
"#,
    );
    assert!(err.contains("twice"), "the old function is gone: {err}");

    // The name is free again, so a different shape is now allowed.
    s.declare_global("buf", ElemType::I64, 16)
        .expect("after :reset a name can be re-declared with a new shape");
}

#[test]
fn bench_requires_main() {
    let mut s = session();
    let no_main = s
        .bench(
            "function %helper() -> i64 {\nblock0:\n    v0 = iconst.i64 1\n    return v0\n}\n",
            5,
            &plain(),
        )
        .unwrap_err()
        .to_string();
    assert!(no_main.contains("%main"), "bench should ask for an entry point: {no_main}");

    let zero = s
        .bench(
            "function %main() -> i64 {\nblock0:\n    v0 = iconst.i64 1\n    return v0\n}\n",
            0,
            &plain(),
        )
        .unwrap_err()
        .to_string();
    assert!(zero.contains("iteration"), "bench should reject 0 iterations: {zero}");
}

#[test]
fn looks_like_clif() {
    assert!(Session::looks_like_clif(
        "function %main() -> i64 {\nblock0:\n    v0 = iconst.i64 1\n    return v0\n}"
    ));
    assert!(Session::looks_like_clif("  function %square(i64) -> i64 { }"));
    assert!(!Session::looks_like_clif("sum the integers from 1 to 100"));
    assert!(!Session::looks_like_clif(":state"));
    assert!(!Session::looks_like_clif(""));
}

//! Process isolation acceptance tests (DESIGN.md §8.1): a crash or hang in JIT
//! code fails one call, and the session comes back without it.

use std::time::{Duration, Instant};

use clifpad::{Backend, ElemType, EvalOptions, Init, Scalar, Supervisor};

fn supervisor(timeout_ms: u64) -> Supervisor {
    Supervisor::new(
        env!("CARGO_BIN_EXE_clifpad"),
        false,
        Duration::from_millis(timeout_ms),
    )
    .unwrap()
}

fn eval(s: &mut Supervisor, text: &str) -> anyhow::Result<clifpad::Outcome> {
    s.eval(text, &EvalOptions::default())
}

const SQ: &str = "function %sq(i64) -> i64 {
block0(v0: i64):
    v1 = imul v0, v0
    return v1
}";

const USE_SQ: &str = "function %main() -> i64 {
    sig0 = (i64) -> i64
    fn0 = %sq sig0
block0:
    v0 = iconst.i64 9
    v1 = call fn0(v0)
    return v1
}";

const READ_XS1: &str = "function %main() -> i64 {
    gv0 = symbol %xs
block0:
    v0 = symbol_value.i64 gv0
    v1 = load.i64 v0+8
    return v1
}";

fn crash_with(body: &str) -> String {
    format!("function %main() -> i64 {{\nblock0:\n{body}\n}}")
}

#[test]
fn trap_reports_sigill_with_prints_and_restores_functions_and_buffers() {
    let mut s = supervisor(5000);
    eval(&mut s, SQ).unwrap();
    let bytes = [1i64, 2, 3].iter().flat_map(|v| v.to_ne_bytes()).collect();
    s.alloc("xs", &Init::Data { ty: ElemType::I64, bytes }).unwrap();

    let trap = "function %main() -> i64 {
    sig0 = (i64)
    fn0 = %rt_print_i64 sig0
    gv0 = symbol %xs
block0:
    v0 = symbol_value.i64 gv0
    v1 = iconst.i64 99
    store v1, v0+8
    call fn0(v1)
    v2 = iconst.i64 0
    v3 = sdiv v1, v2
    return v3
}";
    let err = eval(&mut s, trap).unwrap_err().to_string();
    assert!(err.contains("SIGILL") && err.contains("division"), "{err}");
    assert!(err.contains("Printed before that (1 line(s)): 99"), "{err}");
    assert!(err.contains("buffers hold their initial contents again"), "{err}");

    assert_eq!(eval(&mut s, USE_SQ).unwrap().value, Some(Scalar::I64(81)));
    // The store of 99 died with the worker; replay restores the alloc contents.
    assert_eq!(eval(&mut s, READ_XS1).unwrap().value, Some(Scalar::I64(2)));
}

#[test]
fn bad_address_is_sigsegv() {
    let mut s = supervisor(5000);
    let err = eval(&mut s, &crash_with("    v0 = iconst.i64 8\n    v1 = load.i64 v0\n    return v1"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("SIGSEGV") || err.contains("SIGBUS"), "{err}");
    assert!(err.contains("held nothing yet"), "{err}");
}

#[test]
fn unbounded_recursion_is_reported_as_stack_overflow() {
    let mut s = supervisor(5000);
    let text = "function %r(i64) -> i64 {
    sig0 = (i64) -> i64
    fn0 = %r sig0
block0(v0: i64):
    v1 = call fn0(v0)
    v2 = iadd v1, v0
    return v2
}
function %main() -> i64 {
    sig0 = (i64) -> i64
    fn0 = %r sig0
block0:
    v0 = iconst.i64 1
    v1 = call fn0(v0)
    return v1
}";
    let err = eval(&mut s, text).unwrap_err().to_string();
    assert!(err.contains("overflowed the stack"), "{err}");
    // The crashing call is all or nothing: %r was compiled but is not kept.
    assert!(s.state().unwrap().functions.is_empty());
    assert!(s.definitions().is_empty());
}

#[test]
fn hang_times_out_and_session_survives() {
    let mut s = supervisor(300);
    eval(&mut s, SQ).unwrap();
    let t0 = Instant::now();
    let err = eval(&mut s, "function %main() -> i64 {\nblock0:\n    jump block1\nblock1:\n    jump block1\n}")
        .unwrap_err()
        .to_string();
    let took = t0.elapsed();
    assert!(err.contains("Timed out after 0.3 s"), "{err}");
    assert!(err.contains("every function defined before this call is back"), "{err}");
    assert!(took < Duration::from_secs(2), "timeout took {took:?}");
    assert_eq!(eval(&mut s, USE_SQ).unwrap().value, Some(Scalar::I64(81)));
}

#[test]
fn bench_timeout_names_the_iteration_count() {
    let mut s = supervisor(300);
    let slow = "function %main() -> i64 {
block0:
    v0 = iconst.i64 0
    jump block1(v0)
block1(v1: i64):
    v2 = iconst.i64 1
    v3 = iadd v1, v2
    v4 = iconst.i64 1000000
    v5 = icmp slt v3, v4
    brif v5, block1(v3), block2
block2:
    return v3
}";
    let err = s.bench(slow, 10_000_000, &EvalOptions::default()).unwrap_err().to_string();
    assert!(err.contains("10000000 times"), "{err}");
}

#[test]
fn ordinary_errors_pass_through_unchanged_and_keep_the_worker() {
    let mut s = supervisor(5000);
    eval(&mut s, SQ).unwrap();
    let err = eval(&mut s, "function %main() -> i64 {\nblock0:\n    v0 = iadd_imm v0, 1\n    return v0\n}")
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("parse error:"), "{err}");
    assert!(!err.contains("restored"), "{err}");
    assert_eq!(s.definitions().len(), 1);
}

#[test]
fn floats_cross_the_pipe_bit_exact() {
    let mut s = supervisor(5000);
    for (lit, want) in [("NaN", f64::NAN), ("-Inf", f64::NEG_INFINITY), ("-0.0", -0.0)] {
        let o = eval(&mut s, &format!("function %main() -> f64 {{\nblock0:\n    v0 = f64const {lit}\n    return v0\n}}"))
            .unwrap();
        let Some(Scalar::F64(v)) = o.value else { panic!("{:?}", o.value) };
        assert_eq!(v.to_bits(), want.to_bits(), "{lit}");
    }
}

#[test]
fn reset_replaces_the_worker_and_forgets_the_journal() {
    let mut s = supervisor(5000);
    eval(&mut s, SQ).unwrap();
    s.alloc("buf", &Init::Text("hi".into())).unwrap();
    s.reset().unwrap();
    let state = s.state().unwrap();
    assert!(state.functions.is_empty() && state.globals.is_empty());
    assert!(s.definitions().is_empty());
    // A crash after reset must not resurrect anything from before it.
    eval(&mut s, &crash_with("    trap user1")).unwrap_err();
    assert!(s.state().unwrap().globals.is_empty());
}

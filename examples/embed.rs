//! Embedding clifpad: compile and run CLIF text in-process.
//!
//!     cargo run --example embed

use clifpad::{EvalOptions, Session};

fn main() -> anyhow::Result<()> {
    let mut session = Session::new(false)?;

    // Functions without %main persist and can be called by later evaluations.
    session.eval(
        "function %square(i64) -> i64 {
         block0(v0: i64):
             v1 = imul v0, v0
             return v1
         }",
        &EvalOptions::default(),
    )?;

    let out = session.eval(
        "function %main() -> i64 {
             sig0 = (i64) -> i64
             fn0 = %square sig0
         block0:
             v0 = iconst.i64 12
             v1 = call fn0(v0)
             return v1
         }",
        &EvalOptions { asm: true },
    )?;

    println!("value: {:?}", out.value);
    println!("compile {} ns, run {} ns", out.compile_ns, out.run_ns);
    println!("{}", out.asm.unwrap_or_default());
    Ok(())
}

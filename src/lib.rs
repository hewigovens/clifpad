//! Native scratchpad: CLIF text → Cranelift JIT → native execution, with
//! introspection. See DESIGN.md.

pub mod mcp;
pub mod runtime;
pub mod session;
pub mod worker;

pub use runtime::{ElemType, Runtime};
pub use session::{
    Backend, BenchResult, EvalOptions, FunctionInfo, GlobalInfo, Init, Outcome, Scalar, Session,
    StateReport, display_signature, hexify_floats,
};
pub use worker::Supervisor;

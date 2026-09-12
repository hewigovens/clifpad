//! Native scratchpad: CLIF text → Cranelift JIT → native execution, with
//! introspection. See DESIGN.md.

pub mod mcp;
pub mod runtime;
pub mod session;

pub use runtime::{ElemType, Runtime};
pub use session::{
    BenchResult, EvalOptions, FunctionInfo, GlobalInfo, Outcome, Scalar, Session, StateReport,
    display_signature, hexify_floats,
};

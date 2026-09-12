//! MCP server over stdio (JSON-RPC 2.0). See DESIGN.md §7.
//! Stub: to be implemented.

use anyhow::{Result, bail};

/// Serve one `Session` over stdin/stdout until EOF.
pub fn serve(allow_unsafe: bool) -> Result<()> {
    let _ = allow_unsafe;
    bail!("MCP server not implemented yet")
}

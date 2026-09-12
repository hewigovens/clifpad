//! Temporary minimal CLI while the real one is built: `smart-repl --eval FILE`.

use std::process::ExitCode;

use smart_repl::{EvalOptions, Session};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 || args[1] != "--eval" {
        eprintln!("usage: smart-repl --eval FILE.clif");
        return ExitCode::from(2);
    }
    let text = match std::fs::read_to_string(&args[2]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{}: {e}", args[2]);
            return ExitCode::from(2);
        }
    };
    let mut session = match Session::new(false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e:#}");
            return ExitCode::from(1);
        }
    };
    match session.eval(&text, &EvalOptions { asm: true }) {
        Ok(o) => {
            for p in &o.prints {
                println!("{p}");
            }
            if let Some(v) = o.value {
                println!("{v}");
            }
            println!("; compile {} µs, run {} µs, {} bytes", o.compile_ns / 1000, o.run_ns / 1000, o.code_bytes);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

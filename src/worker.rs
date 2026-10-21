//! Process isolation (DESIGN.md §8.1). A `Supervisor` runs a `Session` in a
//! worker process, `clifpad --worker`, so a crash or hang in JIT code costs one
//! call instead of the session. The two sides speak newline-delimited JSON.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::{
    Backend, BenchResult, EvalOptions, GlobalInfo, Init, Outcome, Session, StateReport,
};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a new worker may take to say it is ready. Kept apart from the
/// run timeout: the first exec from a fresh process has been measured at
/// 400 ms on macOS, which a 0.3 s run limit would blame on the program.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Prints kept for a crash report; the tail is what explains a crash.
const PRINTS_KEPT: usize = 10;

// Signal numbers differ between Linux and the BSDs (SIGBUS is 7 vs 10); the
// rest are the same on every Unix this builds for.
const SIGILL: i32 = 4;
const SIGABRT: i32 = 6;
const SIGFPE: i32 = 8;
const SIGSEGV: i32 = 11;
#[cfg(target_os = "linux")]
const SIGBUS: i32 = 7;
#[cfg(not(target_os = "linux"))]
const SIGBUS: i32 = 10;
const STDERR_KEPT: usize = 4096;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Eval { text: String, asm: bool },
    Bench { text: String, iterations: usize, asm: bool },
    Alloc { name: String, init: Init },
    Define { text: String },
    State,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Print(String),
    Ok(Value),
    Err(String),
}

// ------------------------------------------------------------------ worker

/// The `--worker` loop: one request line in, streamed prints, one reply line out.
pub fn serve(allow_unsafe: bool) -> Result<()> {
    exit_with_parent();
    let mut session = Session::new(allow_unsafe)?;
    session.runtime_mut().print_sink = Some(stream_print);
    send(&Reply::Ok(Value::Null))?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(req) => match handle(&mut session, req) {
                Ok(v) => Reply::Ok(v),
                Err(e) => Reply::Err(format!("{e:#}")),
            },
            Err(e) => Reply::Err(format!("bad worker request: {e}")),
        };
        send(&reply)?;
    }
    Ok(())
}

fn handle(session: &mut Session, req: Request) -> Result<Value> {
    Ok(match req {
        Request::Eval { text, asm } => serde_json::to_value(session.eval(&text, &EvalOptions { asm })?)?,
        Request::Bench { text, iterations, asm } => {
            // Streaming every print of every iteration would flood the pipe.
            let sink = session.runtime_mut().print_sink.take();
            let result = session.bench(&text, iterations, &EvalOptions { asm });
            session.runtime_mut().print_sink = sink;
            serde_json::to_value(result?)?
        }
        Request::Alloc { name, init } => serde_json::to_value(session.alloc(&name, &init)?)?,
        Request::Define { text } => {
            session.define(&text)?;
            Value::Null
        }
        Request::State => serde_json::to_value(Session::state(session))?,
    })
}

fn send(reply: &Reply) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, reply)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn stream_print(s: &str) {
    let _ = send(&Reply::Print(s.to_string()));
}

/// A worker stuck in a JIT loop never reads stdin, so it cannot notice EOF
/// when its parent dies. Poll the parent pid instead.
fn exit_with_parent() {
    let parent = std::os::unix::process::parent_id();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(250));
            if std::os::unix::process::parent_id() != parent {
                std::process::exit(1);
            }
        }
    });
}

// -------------------------------------------------------------- supervisor

struct Worker {
    child: Child,
    stdin: ChildStdin,
    /// Replies from the stdout reader thread; `None` means stdout closed.
    replies: Receiver<Option<Reply>>,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(exe: &Path, allow_unsafe: bool) -> Result<Self> {
        let mut cmd = Command::new(exe);
        cmd.arg("--worker");
        if allow_unsafe {
            cmd.arg("--allow-unsafe");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start worker {}", exe.display()))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let mut stderr_pipe = child.stderr.take().expect("piped");

        let (tx, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let reply = serde_json::from_str(&line)
                    .unwrap_or_else(|e| Reply::Err(format!("worker sent an invalid reply: {e}")));
                if tx.send(Some(reply)).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let tail = Arc::clone(&stderr);
        let stderr_thread = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr_pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut t = tail.lock().unwrap();
                t.push_str(&String::from_utf8_lossy(&buf[..n]));
                if t.len() > 2 * STDERR_KEPT {
                    let mut cut = t.len() - STDERR_KEPT;
                    while !t.is_char_boundary(cut) {
                        cut += 1;
                    }
                    t.drain(..cut);
                }
            }
        });

        let mut worker = Self {
            child,
            stdin,
            replies,
            stderr,
            stderr_thread: Some(stderr_thread),
        };
        let why = match worker.replies.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Some(Reply::Ok(_))) => return Ok(worker),
            Ok(Some(Reply::Err(e))) => e,
            Ok(Some(Reply::Print(_))) => "it printed before it was ready".into(),
            Err(RecvTimeoutError::Timeout) => format!("no ready signal within {} s", STARTUP_TIMEOUT.as_secs()),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                let (status, stderr) = worker.reap(false);
                let code = status.map_or("an unknown status".into(), |s| s.to_string());
                return Err(anyhow!(
                    "the worker process {} --worker did not start: it exited with {code}. {}",
                    exe.display(),
                    stderr.trim()
                ));
            }
        };
        worker.reap(true);
        Err(anyhow!("the worker process {} --worker did not start: {why}", exe.display()))
    }

    /// Reap the process and return its status plus the tail of its stderr.
    fn reap(&mut self, kill: bool) -> (Option<ExitStatus>, String) {
        if kill {
            let _ = self.child.kill();
        }
        let status = self.child.wait().ok();
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
        (status, self.stderr.lock().unwrap().clone())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Why the worker stopped answering.
enum Death {
    Exited(Option<ExitStatus>, String),
    TimedOut,
}

/// A `Session` in a worker process. A crash or timeout ends only the call
/// that caused it: the supervisor starts a new worker and replays every
/// earlier call that changed state (DESIGN.md §8.1).
pub struct Supervisor {
    exe: PathBuf,
    allow_unsafe: bool,
    timeout: Duration,
    worker: Worker,
    /// Successful `Define` and `Alloc` requests, in order.
    journal: Vec<Request>,
}

impl Supervisor {
    /// Start a worker from `exe`, which must accept `--worker`.
    pub fn new(exe: impl Into<PathBuf>, allow_unsafe: bool, timeout: Duration) -> Result<Self> {
        let exe = exe.into();
        let worker = Worker::spawn(&exe, allow_unsafe)?;
        Ok(Self {
            exe,
            allow_unsafe,
            timeout,
            worker,
            journal: Vec::new(),
        })
    }

    /// Start a worker from the running binary.
    pub fn from_current_exe(allow_unsafe: bool, timeout: Duration) -> Result<Self> {
        let exe = std::env::current_exe().context("locate the clifpad binary for the worker")?;
        Self::new(exe, allow_unsafe, timeout)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn call<T: DeserializeOwned>(&mut self, req: &Request) -> Result<T> {
        let started = Instant::now();
        let mut prints: VecDeque<String> = VecDeque::new();
        let mut printed = 0usize;
        let death = 'wait: {
            let line = serde_json::to_string(req)?;
            if writeln!(self.worker.stdin, "{line}")
                .and_then(|()| self.worker.stdin.flush())
                .is_err()
            {
                let (status, stderr) = self.worker.reap(false);
                break 'wait Death::Exited(status, stderr);
            }
            loop {
                let left = self.timeout.saturating_sub(started.elapsed());
                match self.worker.replies.recv_timeout(left) {
                    Ok(Some(Reply::Print(p))) => {
                        printed += 1;
                        if prints.len() == PRINTS_KEPT {
                            prints.pop_front();
                        }
                        prints.push_back(p);
                    }
                    Ok(Some(Reply::Ok(v))) => {
                        return serde_json::from_value(v).context("decode worker reply");
                    }
                    Ok(Some(Reply::Err(e))) => return Err(anyhow!(e)),
                    Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                        let (status, stderr) = self.worker.reap(false);
                        break 'wait Death::Exited(status, stderr);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        self.worker.reap(true);
                        break 'wait Death::TimedOut;
                    }
                }
            }
        };

        let mut msg = match death {
            Death::Exited(status, stderr) => crash_cause(status, &stderr),
            Death::TimedOut => timeout_cause(req, self.timeout),
        };
        if printed > 0 {
            let shown: Vec<&str> = prints.iter().map(String::as_str).collect();
            let which = if printed > prints.len() {
                format!("last {} of {printed} lines", prints.len())
            } else {
                format!("{printed} line(s)")
            };
            msg.push_str(&format!("\nPrinted before that ({which}): {}", shown.join(", ")));
        }
        msg.push('\n');
        msg.push_str(&self.restore());
        Err(anyhow!(msg))
    }

    /// Replace the dead worker and replay the journal. Returns the note that
    /// tells the caller what survived.
    fn restore(&mut self) -> String {
        let journal = std::mem::take(&mut self.journal);
        let result = Worker::spawn(&self.exe, self.allow_unsafe).and_then(|w| {
            self.worker = w;
            for req in &journal {
                self.call::<Value>(req)
                    .map_err(|e| anyhow!("replaying an earlier call failed: {e:#}"))?;
            }
            Ok(())
        });
        if let Err(e) = result {
            // Leave a working, empty session rather than a half-restored one.
            let note = match Worker::spawn(&self.exe, self.allow_unsafe) {
                Ok(w) => {
                    self.worker = w;
                    "The session is now empty."
                }
                Err(_) => "No new worker could be started.",
            };
            return format!("Restoring the session failed ({e:#}). {note}");
        }
        let has_fns = journal.iter().any(|r| matches!(r, Request::Define { .. }));
        let has_bufs = journal.iter().any(|r| matches!(r, Request::Alloc { .. }));
        self.journal = journal;
        match (has_fns, has_bufs) {
            (false, false) => "The session held nothing yet and is ready for the next call.".into(),
            (true, false) => "The session was restored: every function defined before this call is back.".into(),
            (_, true) => "The session was restored: functions and buffers defined before this call are back, \
                but buffers hold their initial contents again (values stored into them since, and memory \
                from %rt_alloc, are lost) and buffer addresses changed."
                .into(),
        }
    }
}

fn crash_cause(status: Option<ExitStatus>, stderr: &str) -> String {
    let signal = status.and_then(|s| s.signal());
    match signal {
        Some(sig @ (SIGILL | SIGFPE)) => format!(
            "The program crashed with a Cranelift trap ({}): division or remainder by zero, \
            signed division overflow (INT_MIN / -1), a float-to-int conversion of NaN or an out-of-range \
            value, or an explicit trap.",
            if sig == SIGILL { "SIGILL" } else { "SIGFPE" }
        ),
        Some(sig @ (SIGSEGV | SIGBUS)) => format!(
            "The program crashed on a bad memory access ({}): most often an address used where a \
             loaded value was meant (symbol_value gives an address, load from it), or an index past \
             the end of a buffer.",
            if sig == SIGSEGV { "SIGSEGV" } else { "SIGBUS" }
        ),
        Some(SIGABRT) if stderr.contains("overflowed its stack") => "The program overflowed the stack (SIGABRT): \
            recursion without a reachable base case, or recursion too deep for the stack."
            .into(),
        Some(sig) => {
            let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
            let name = if sig == SIGABRT { "SIGABRT".to_string() } else { format!("signal {sig}") };
            if last.is_empty() {
                format!("The program was killed by {name}.")
            } else {
                format!("The program was killed by {name}: {last}")
            }
        }
        None => {
            let code = status
                .and_then(|s| s.code())
                .map_or("an unknown status".to_string(), |c| format!("status {c}"));
            format!("The worker process exited with {code}. {}", stderr.trim())
        }
    }
}

fn timeout_cause(req: &Request, timeout: Duration) -> String {
    let secs = timeout.as_secs_f64();
    // Do not guess between "never ends" and "too slow": a dogfooding agent
    // got a loop hint for exponential recursion that had no loop at all.
    let causes = "Either it never terminates (a loop exit condition that is never met, or a \
        loop-carried value not passed on the back-edge jump) or it is too slow (for example \
        exponential recursion); try a smaller input to tell which. The limit is set by the \
        --timeout flag or CLIFPAD_TIMEOUT.";
    match req {
        Request::Bench { iterations, .. } => format!(
            "Timed out after {secs} s running %main {iterations} times: lower the iteration \
             count, or {}",
            causes.replacen("Either it", "check whether one run", 1)
        ),
        _ => format!("Timed out after {secs} s: %main did not return. {causes}"),
    }
}

impl Backend for Supervisor {
    fn eval(&mut self, text: &str, opts: &EvalOptions) -> Result<Outcome> {
        let outcome: Outcome = self.call(&Request::Eval { text: text.to_string(), asm: opts.asm })?;
        if !outcome.defined.is_empty() {
            self.journal.push(Request::Define { text: text.trim().to_string() });
        }
        Ok(outcome)
    }

    fn bench(&mut self, text: &str, iterations: usize, opts: &EvalOptions) -> Result<BenchResult> {
        let req = Request::Bench { text: text.to_string(), iterations, asm: opts.asm };
        let bench: BenchResult = self.call(&req)?;
        if !bench.outcome.defined.is_empty() {
            self.journal.push(Request::Define { text: text.trim().to_string() });
        }
        Ok(bench)
    }

    fn alloc(&mut self, name: &str, init: &Init) -> Result<Vec<GlobalInfo>> {
        let req = Request::Alloc { name: name.to_string(), init: init.clone() };
        let infos = self.call(&req)?;
        self.journal.push(req);
        Ok(infos)
    }

    fn state(&mut self) -> Result<StateReport> {
        self.call(&Request::State)
    }

    fn reset(&mut self) -> Result<()> {
        // A fresh worker also hands the old JIT memory back to the OS.
        self.worker = Worker::spawn(&self.exe, self.allow_unsafe)?;
        self.journal.clear();
        Ok(())
    }

    fn definitions(&self) -> Vec<String> {
        self.journal
            .iter()
            .filter_map(|r| match r {
                Request::Define { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn allow_unsafe(&self) -> bool {
        self.allow_unsafe
    }
}

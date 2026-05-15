use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Serialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, Notify};

use crate::ipc::{parse_signal, Request, Response};
use crate::meta::{Meta, SCHEMA_VERSION};
use crate::pty::{
    log_segments_from_env, log_size_from_env, stream_to_log, timestamps_from_env, LogWriter,
};
use crate::state::{
    cleanup_stale_files, cmd_path, get_state_dir, log_path, meta_path, pid_path, recent_log_path,
    sock_path, version_path,
};

/// How long the daemon stays up after the child exits so observers can collect
/// the exit code via `status` before sidecars vanish.
const POST_EXIT_LINGER: Duration = Duration::from_secs(2);

/// Grace period between SIGTERM and SIGKILL when tearing down the child on
/// idle/close/signal exit paths.
const CHILD_KILL_GRACE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug)]
enum ChildState {
    Running,
    Exited { code: Option<i32> },
}

struct SharedState {
    child_pid: u32,
    state: ChildState,
    counters: Arc<DaemonCounters>,
}

type Shared = Arc<Mutex<SharedState>>;

/// Live counters the PTY reader maintains so `summary` doesn't have to scan
/// the entire log every call. AtomicU64 + cross-thread Arc; no async locks.
#[derive(Default)]
pub struct DaemonCounters {
    pub line_count: AtomicU64,
    /// ms-since-epoch of the most recent `\n` write. 0 if the daemon hasn't
    /// emitted any lines yet.
    pub last_line_at_ms: AtomicU64,
    pub bytes_written: AtomicU64,
}

/// Why the daemon's main loop exited. Used to label entries in `recent.jsonl`
/// so doctor's misuse heuristic can tell short-lived but useful runs from
/// noise.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ExitReason {
    ChildExited,
    Close,
    Idle,
    Signal,
    Error,
}

/// RAII guard that runs sidecar cleanup and records the daemon's lifetime to
/// `recent.jsonl` on every exit path — including panics. This is the single
/// "cleanup convergence" point referenced in P6: close/idle/signal/error all
/// drop the same guard.
struct CleanupGuard {
    id: String,
    started_at: u64,
    start: Instant,
    /// If set, the daemon stopped being "useful" at this instant (e.g. the
    /// child exited). The post-exit linger is bookkeeping, not lifetime, and
    /// is excluded from the duration recorded in `recent.jsonl` so doctor's
    /// misuse heuristic measures user-facing runtime.
    useful_until: Option<Instant>,
    reason: ExitReason,
}

impl CleanupGuard {
    fn new(id: &str, started_at: u64) -> Self {
        Self {
            id: id.to_string(),
            started_at,
            start: Instant::now(),
            useful_until: None,
            reason: ExitReason::Error,
        }
    }

    fn set_reason(&mut self, reason: ExitReason) {
        self.reason = reason;
    }

    fn mark_child_exited(&mut self) {
        self.useful_until = Some(Instant::now());
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        cleanup_stale_files(&self.id);
        let exited_at = now_secs();
        let until = self.useful_until.unwrap_or_else(Instant::now);
        let duration_ms = until.duration_since(self.start).as_millis() as u64;
        append_recent_jsonl(&self.id, self.started_at, exited_at, duration_ms, self.reason);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn append_recent_jsonl(
    id: &str,
    started_at: u64,
    exited_at: u64,
    duration_ms: u64,
    reason: ExitReason,
) {
    let entry = json!({
        "id": id,
        "started_at": started_at,
        "exited_at": exited_at,
        "duration_ms": duration_ms,
        "reason": reason,
    });
    let mut line = serde_json::to_string(&entry).unwrap_or_default();
    line.push('\n');
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(recent_log_path())
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Runs the daemon for a given id and child command. Bind happens before any
/// other sidecar is written so a spawn-race loser exits on EADDRINUSE without
/// clobbering the winner's state. After bind, stderr is redirected to
/// /dev/null so the CLI's pipe-drop can't kill the daemon.
pub async fn run_daemon(id: &str, argv: Vec<String>) -> std::io::Result<()> {
    if argv.is_empty() {
        eprintln!("Daemon error: empty argv");
        return Err(std::io::Error::other("empty argv"));
    }

    let dir = get_state_dir();
    fs::create_dir_all(&dir)?;

    let sock = sock_path(id);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind socket: {e}");
            return Err(e);
        }
    };

    let pid_str = process::id().to_string();
    fs::write(pid_path(id), &pid_str)?;
    fs::write(version_path(id), env!("CARGO_PKG_VERSION"))?;
    fs::write(
        cmd_path(id),
        serde_json::to_string(&argv).unwrap_or_default(),
    )?;

    redirect_stderr_to_devnull();

    let started_at = now_secs();
    let mut guard = CleanupGuard::new(id, started_at);

    run_main_loop(id, argv, listener, started_at, &mut guard).await
}

async fn run_main_loop(
    id: &str,
    argv: Vec<String>,
    listener: UnixListener,
    started_at: u64,
    guard: &mut CleanupGuard,
) -> std::io::Result<()> {
    let (program, args) = argv.split_first().expect("argv non-empty (checked above)");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| std::io::Error::other(format!("openpty: {e}")))?;

    let mut cmd = CommandBuilder::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    for (k, v) in std::env::vars_os() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| std::io::Error::other(format!("spawn child: {e}")))?;
    let child_pid = child.process_id().unwrap_or(0);

    // The slave fd lives in the child; closing our copy lets read() on the
    // master return EOF/EIO cleanly once the child exits.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| std::io::Error::other(format!("clone pty reader: {e}")))?;

    let log = log_path(id);
    let log_writer = LogWriter::open_with(
        log.clone(),
        log_size_from_env(),
        log_segments_from_env(),
        timestamps_from_env(),
    )?;
    let counters: Arc<DaemonCounters> = Arc::new(DaemonCounters::default());
    let counters_for_reader = counters.clone();
    thread::spawn(move || stream_to_log(reader, log_writer, Some(counters_for_reader)));

    // Write .meta atomically *after* the child spawn so it includes child_pid.
    // Readers (list, doctor) tolerate a transient missing .meta in the small
    // window between sock bind and this write.
    write_meta(id, &argv, started_at, child_pid, &log)?;

    // Wait for child on a blocking thread; signal main loop via mpsc.
    let (exit_tx, mut exit_rx) = mpsc::channel::<Option<i32>>(1);
    let child_arc = Arc::new(Mutex::new(child));
    {
        let child_arc = child_arc.clone();
        thread::spawn(move || {
            let status = child_arc.lock().unwrap().wait().ok();
            let code = status.map(|s| s.exit_code() as i32);
            let _ = exit_tx.blocking_send(code);
        });
    }

    let shared: Shared = Arc::new(Mutex::new(SharedState {
        child_pid,
        state: ChildState::Running,
        counters: counters.clone(),
    }));
    let close_notify = Arc::new(Notify::new());

    // Idle timeout (optional). Pinned per agent-browser's pattern so the
    // future is stable across select! polls — a re-created future on each
    // poll iteration would never accumulate elapsed time and never fire
    // (the #1101 bug).
    let idle_ms: Option<u64> = std::env::var("AGENT_TERMINAL_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0);

    let (idle_reset_tx, mut idle_reset_rx) = mpsc::channel::<()>(64);
    let idle_reset_tx: Option<Arc<mpsc::Sender<()>>> = idle_ms.map(|_| Arc::new(idle_reset_tx));

    let mut idle_sleep_pin = idle_ms
        .map(|ms| Box::pin(tokio::time::sleep(Duration::from_millis(ms))));

    let mut linger_deadline: Option<Instant> = None;
    let mut exit_reason = ExitReason::Error;

    loop {
        let sleep_until = linger_deadline.map(tokio::time::Instant::from_std);

        tokio::select! {
            accept = listener.accept() => {
                if let Ok((stream, _)) = accept {
                    let sh = shared.clone();
                    let cn = close_notify.clone();
                    let reset = idle_reset_tx.clone();
                    tokio::spawn(async move {
                        handle_connection(stream, sh, cn, reset).await;
                    });
                }
            }
            maybe_code = exit_rx.recv(), if linger_deadline.is_none() => {
                let code = maybe_code.flatten();
                let mut sh = shared.lock().unwrap();
                sh.state = ChildState::Exited { code };
                linger_deadline = Some(Instant::now() + POST_EXIT_LINGER);
                exit_reason = ExitReason::ChildExited;
                guard.mark_child_exited();
            }
            _ = async { tokio::time::sleep_until(sleep_until.unwrap()).await }, if sleep_until.is_some() => {
                break;
            }
            _ = close_notify.notified() => {
                kill_child(child_pid, libc::SIGTERM);
                tokio::time::sleep(CHILD_KILL_GRACE).await;
                kill_child(child_pid, libc::SIGKILL);
                let mut sh = shared.lock().unwrap();
                sh.state = ChildState::Exited { code: None };
                exit_reason = ExitReason::Close;
                break;
            }
            _ = shutdown_signal() => {
                kill_child(child_pid, libc::SIGTERM);
                tokio::time::sleep(CHILD_KILL_GRACE).await;
                kill_child(child_pid, libc::SIGKILL);
                exit_reason = ExitReason::Signal;
                break;
            }
            // Idle branches — only armed when AGENT_TERMINAL_IDLE_TIMEOUT_MS > 0.
            _ = async {
                match idle_sleep_pin {
                    Some(ref mut s) => s.as_mut().await,
                    None => std::future::pending::<()>().await,
                }
            }, if idle_ms.is_some() => {
                kill_child(child_pid, libc::SIGTERM);
                tokio::time::sleep(CHILD_KILL_GRACE).await;
                kill_child(child_pid, libc::SIGKILL);
                exit_reason = ExitReason::Idle;
                break;
            }
            _ = idle_reset_rx.recv(), if idle_ms.is_some() => {
                idle_sleep_pin = idle_ms
                    .map(|ms| Box::pin(tokio::time::sleep(Duration::from_millis(ms))));
                continue;
            }
        }
    }

    guard.set_reason(exit_reason);
    Ok(())
}

fn write_meta(
    id: &str,
    argv: &[String],
    started_at: u64,
    child_pid: u32,
    log: &std::path::Path,
) -> std::io::Result<()> {
    let project = std::env::var("AGENT_TERMINAL_PROJECT").unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = std::env::var("AGENT_TERMINAL_NAME")
        .ok()
        .filter(|s| !s.is_empty());
    let tags: BTreeMap<String, String> = std::env::var("AGENT_TERMINAL_TAGS")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let started_by_pid = std::env::var("AGENT_TERMINAL_STARTED_BY_PID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok());

    let meta = Meta {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        name,
        project,
        tags,
        cmd: argv.to_vec(),
        cwd,
        started_at,
        started_by_pid,
        child_pid: if child_pid > 0 { Some(child_pid) } else { None },
        log_path: log.to_string_lossy().into_owned(),
    };
    meta.write_atomically(&meta_path(id))
}

#[cfg(unix)]
fn kill_child(pid: u32, sig: i32) {
    if pid == 0 {
        return;
    }
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

#[cfg(not(unix))]
fn kill_child(_pid: u32, _sig: i32) {}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    shared: Shared,
    close_notify: Arc<Notify>,
    idle_reset: Option<Arc<mpsc::Sender<()>>>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut buf = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match buf.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Any well-formed command counts as activity for the idle timer.
        if let Some(ref tx) = idle_reset {
            let _ = tx.try_send(());
        }

        let response = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => dispatch(&req, &shared),
            Err(e) => Response::err(format!("Invalid JSON: {e}")),
        };

        let is_close = response.success
            && serde_json::from_str::<Request>(trimmed)
                .map(|r| r.action == "close")
                .unwrap_or(false);

        let mut buf_out = serde_json::to_vec(&response).unwrap_or_default();
        buf_out.push(b'\n');
        if writer.write_all(&buf_out).await.is_err() {
            return;
        }
        let _ = writer.flush().await;

        if is_close {
            tokio::time::sleep(Duration::from_millis(50)).await;
            close_notify.notify_one();
            return;
        }
    }
}

fn dispatch(req: &Request, shared: &Shared) -> Response {
    match req.action.as_str() {
        "status" => {
            let sh = shared.lock().unwrap();
            let data = match sh.state {
                ChildState::Running => json!({
                    "state": "running",
                    "child_pid": sh.child_pid,
                }),
                ChildState::Exited { code } => json!({
                    "state": "exited",
                    "code": code,
                }),
            };
            Response::ok(data)
        }
        "signal" => {
            let pid = shared.lock().unwrap().child_pid;
            match req.sig.as_deref().and_then(parse_signal) {
                Some(sig) => {
                    kill_child(pid, sig);
                    Response::ok(json!({ "sent": req.sig, "pid": pid }))
                }
                None => Response::err(format!(
                    "Invalid or missing signal: {:?}",
                    req.sig.as_deref().unwrap_or("")
                )),
            }
        }
        "close" => Response::ok(json!({ "closing": true })),
        "summary" => {
            let sh = shared.lock().unwrap();
            let state_str = match sh.state {
                ChildState::Running => "running",
                ChildState::Exited { .. } => "exited",
            };
            let exit_code = match sh.state {
                ChildState::Exited { code } => code,
                _ => None,
            };
            let c = &sh.counters;
            Response::ok(json!({
                "state": state_str,
                "child_pid": sh.child_pid,
                "exit_code": exit_code,
                "line_count": c.line_count.load(Ordering::Relaxed),
                "last_line_at_ms": c.last_line_at_ms.load(Ordering::Relaxed),
                "bytes_written": c.bytes_written.load(Ordering::Relaxed),
            }))
        }
        other => Response::err(format!("Unknown action: {other}")),
    }
}

#[cfg(unix)]
fn redirect_stderr_to_devnull() {
    use std::os::unix::io::IntoRawFd;
    if let Ok(devnull) = fs::File::create("/dev/null") {
        let fd = devnull.into_raw_fd();
        unsafe {
            libc::dup2(fd, 2);
            libc::close(fd);
        }
    }
}

#[cfg(not(unix))]
fn redirect_stderr_to_devnull() {}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
        _ = sighup.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// Used as a placeholder so the variable is read even when only some branches
// touch it; without this clippy would complain about the unused write in
// `exit_reason = ExitReason::Error` initialization.
#[allow(dead_code)]
fn _suppress() {
    let _ = PathBuf::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin-future regression. The daemon's idle sleep MUST be created once and
    /// polled in-place; re-creating it inside the select! arm makes it never
    /// fire because each tick gets a fresh "starts now" sleep. This is the
    /// #1101 pattern in agent-browser; the same shape lives in
    /// `run_main_loop` and this test pins it in code.
    #[tokio::test]
    async fn pinned_idle_sleep_fires_even_with_busy_drain() {
        let start = std::time::Instant::now();
        let mut drain = tokio::time::interval(Duration::from_millis(100));
        drain.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut idle = Box::pin(tokio::time::sleep(Duration::from_secs(1)));

        let mut ticks = 0;
        let fired_at = loop {
            tokio::select! {
                _ = drain.tick() => {
                    ticks += 1;
                    if ticks > 50 {
                        panic!("idle should have fired by now (ticks={ticks})");
                    }
                }
                _ = idle.as_mut() => break start.elapsed(),
            }
        };
        assert!(
            fired_at >= Duration::from_millis(900) && fired_at <= Duration::from_millis(1400),
            "fired_at = {fired_at:?}, ticks = {ticks}"
        );
    }

    /// Re-armed sleep behaves like a fresh deadline. Mirrors the
    /// `idle_reset_rx.recv()` branch.
    #[tokio::test]
    async fn rearmed_idle_sleep_resets_deadline() {
        let _initial = Box::pin(tokio::time::sleep(Duration::from_millis(300)));
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Re-arm — deadline should now be 300ms from this point, not 150ms.
        let mut idle = Box::pin(tokio::time::sleep(Duration::from_millis(300)));
        let t0 = std::time::Instant::now();
        idle.as_mut().await;
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed <= Duration::from_millis(450),
            "elapsed = {elapsed:?}"
        );
    }

    /// Guard against re-introducing `waitpid(-1)` in daemon code. Inherited
    /// from agent-browser's #1035 regression — `waitpid(-1, WNOHANG)` races
    /// with Rust's `Child::try_wait()` because it reaps *any* child, stealing
    /// the exit status before Rust can collect it.
    #[test]
    fn no_waitpid_minus_one_in_daemon() {
        let source = include_str!("daemon.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !production.contains("waitpid(-1"),
            "daemon.rs production code must not call waitpid(-1, ...)"
        );
    }

    /// `process::exit` skips destructors — including our `CleanupGuard`. The
    /// daemon must not call it on any hot path; converge on `break` instead.
    #[test]
    fn no_process_exit_in_daemon_hot_paths() {
        let source = include_str!("daemon.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !production.contains("process::exit"),
            "daemon.rs production code must not call process::exit(...)"
        );
    }
}

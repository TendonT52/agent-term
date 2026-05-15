use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::ipc::{Request, Response};
use crate::state::{
    cleanup_stale_files, get_state_dir, pid_path, sock_path, version_path,
};

pub struct DaemonResult {
    pub already_running: bool,
    pub id: String,
}

/// Spawn-time options derived from CLI flags. Forwarded to the daemon via
/// env vars so the daemon can record them in `.meta`.
pub struct SpawnOptions<'a> {
    pub project: &'a str,
    pub name: Option<&'a str>,
    pub tags: &'a BTreeMap<String, String>,
    pub started_by_pid: Option<u32>,
    /// Prepend `[<ms>] ` to each log line. The LogWriter picks this up via
    /// the env var on the daemon side.
    pub timestamps: bool,
}

const STARTUP_POLL_MS: u64 = 25;
const STARTUP_MAX_POLLS: usize = 200; // 5s total

/// Spawn a detached daemon for `id` running `argv`, or attach to an existing
/// one if a daemon for `id` is already serving its socket.
///
/// On the spawn-race path: a second CLI invoking with the same explicit id
/// will see the daemon-child exit with EADDRINUSE on stderr; we then wait
/// briefly and connect to the winner's socket.
pub fn ensure_daemon(
    id: &str,
    argv: &[String],
    opts: &SpawnOptions<'_>,
) -> Result<DaemonResult, String> {
    let dir = get_state_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create state dir: {e}"))?;

    let sock = sock_path(id);

    // Reuse existing daemon iff its socket is accepting connections AND its
    // version matches this CLI. A version mismatch means the user upgraded
    // the binary; silently reusing the older daemon would mask new features
    // or fixed bugs, so we restart it.
    if can_connect(&sock) {
        if daemon_version_matches(id) {
            return Ok(DaemonResult {
                already_running: true,
                id: id.to_string(),
            });
        }
        eprintln!(
            "agent-terminal: daemon version mismatch for id {id}, restarting"
        );
        kill_stale_daemon(id);
        // fall through to a fresh spawn below.
    }

    let exe = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);

    let argv_json = serde_json::to_string(argv).map_err(|e| e.to_string())?;
    let tags_json = serde_json::to_string(opts.tags).map_err(|e| e.to_string())?;

    let mut cmd = Command::new(&exe);
    cmd.env("AGENT_TERMINAL_DAEMON", "1")
        .env("AGENT_TERMINAL_ID", id)
        .env("AGENT_TERMINAL_CMD", &argv_json)
        .env("AGENT_TERMINAL_PROJECT", opts.project)
        .env("AGENT_TERMINAL_TAGS", &tags_json)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(n) = opts.name {
        cmd.env("AGENT_TERMINAL_NAME", n);
    }
    if let Some(p) = opts.started_by_pid {
        cmd.env("AGENT_TERMINAL_STARTED_BY_PID", p.to_string());
    }
    if opts.timestamps {
        cmd.env("AGENT_TERMINAL_TIMESTAMPS", "1");
    }

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // Detach from controlling terminal and create a new session.
            // PPID becomes 1 (or the init-like reaper) after the parent exits.
            libc::setsid();
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn daemon: {e}"))?;

    for _ in 0..STARTUP_MAX_POLLS {
        if can_connect(&sock) {
            return Ok(DaemonResult {
                already_running: false,
                id: id.to_string(),
            });
        }
        if let Ok(Some(_status)) = child.try_wait() {
            let mut buf = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut buf);
            }
            let trimmed = buf.trim();
            if trimmed.contains("Address already in use")
                || trimmed.contains("Failed to bind")
            {
                // Loser of a spawn race — let the winner finish coming up.
                thread::sleep(Duration::from_millis(200));
                if can_connect(&sock) {
                    return Ok(DaemonResult {
                        already_running: true,
                        id: id.to_string(),
                    });
                }
            }
            return Err(if trimmed.is_empty() {
                "daemon exited during startup with no error output".to_string()
            } else {
                format!("daemon failed to start: {trimmed}")
            });
        }
        thread::sleep(Duration::from_millis(STARTUP_POLL_MS));
    }

    Err("daemon failed to bind socket within startup window".to_string())
}

fn can_connect(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}

/// True iff the running daemon for `id` was built from the same CLI version.
/// A missing `.version` file counts as "no match" — most likely a stale
/// leftover from before version tracking was added, and silently reusing it
/// is the bug this check exists to prevent.
fn daemon_version_matches(id: &str) -> bool {
    match fs::read_to_string(version_path(id)) {
        Ok(v) => v.trim() == env!("CARGO_PKG_VERSION"),
        Err(_) => false,
    }
}

/// Tear down an existing daemon for `id`: remove its socket so no new clients
/// reach it, then SIGTERM (1 s grace), SIGKILL fallback, then sweep sidecars.
/// Used on the version-mismatch upgrade path.
fn kill_stale_daemon(id: &str) {
    // Remove the socket first so concurrent CLIs don't try to use the stale daemon.
    let _ = fs::remove_file(sock_path(id));

    if let Ok(s) = fs::read_to_string(pid_path(id)) {
        if let Ok(pid) = s.trim().parse::<u32>() {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            // Up to 1 s grace.
            for _ in 0..10 {
                thread::sleep(Duration::from_millis(100));
                #[cfg(unix)]
                let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
                #[cfg(not(unix))]
                let alive = false;
                if !alive {
                    break;
                }
            }
            #[cfg(unix)]
            unsafe {
                if libc::kill(pid as i32, 0) == 0 {
                    libc::kill(pid as i32, libc::SIGKILL);
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    cleanup_stale_files(id);
}

/// Send a single JSON request over the daemon's socket and read a single
/// newline-terminated JSON response.
pub fn send_request(id: &str, request: &Request) -> Result<Response, String> {
    let sock = sock_path(id);
    let stream = UnixStream::connect(&sock)
        .map_err(|e| format!("connect to {}: {e}", sock.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok();

    let mut writer = &stream;
    let mut body = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    body.push(b'\n');
    writer
        .write_all(&body)
        .map_err(|e| format!("write: {e}"))?;
    writer.flush().ok();

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;

    serde_json::from_str::<Response>(&line)
        .map_err(|e| format!("invalid response: {e} (raw: {line:?})"))
}

/// Convenience wrapper for the no-argument status action.
pub fn status(id: &str) -> Result<Value, String> {
    let resp = send_request(
        id,
        &Request {
            action: "status".into(),
            sig: None,
        },
    )?;
    if !resp.success {
        return Err(resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    resp.data
        .ok_or_else(|| "daemon returned no data".to_string())
}

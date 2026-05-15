use std::fs;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::meta::Meta;
use crate::state::{
    cleanup_stale_files, get_state_dir, is_pid_alive, meta_path, pid_path, recent_log_path,
};

/// Threshold for the misuse heuristic: ≥ this many short-lived daemons in
/// the last hour triggers a warning.
const MISUSE_COUNT: usize = 10;
const MISUSE_DURATION_MS: u64 = 2_000;
const MISUSE_WINDOW_SECS: u64 = 3_600;

pub struct DoctorOptions {
    pub fix: bool,
    pub json: bool,
}

pub fn run(opts: DoctorOptions) -> ExitCode {
    let report = scan(&opts);

    if opts.json {
        let body = serde_json::to_string_pretty(&report)
            .unwrap_or_else(|_| "{}".into());
        println!("{}", body);
    } else {
        print_human(&report);
    }

    if opts.fix {
        apply_fixes(&report);
    }

    // Exit code: 0 if no problems or all fixed; 1 if there are issues and
    // --fix was not specified.
    let has_issues = !report.stale.is_empty()
        || !report.orphans.is_empty()
        || !report.warnings.is_empty();
    if has_issues && !opts.fix {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Snapshot of the daemon state directory and recent activity. Public so the
/// CLI can render it however it likes; also stable enough to JSON-emit.
#[derive(Default, Serialize)]
pub struct DoctorReport {
    pub live: Vec<LiveEntry>,
    /// Sidecar bundles whose daemon process is gone.
    pub stale: Vec<StaleEntry>,
    /// Children whose parent daemon vanished (typically because the daemon
    /// was SIGKILLed and couldn't run its cleanup).
    pub orphans: Vec<OrphanEntry>,
    /// Free-form advisory messages (misuse heuristic, version skew, etc).
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
pub struct LiveEntry {
    pub id: String,
    pub daemon_pid: u32,
    pub child_pid: Option<u32>,
    pub name: Option<String>,
    pub project: String,
}

#[derive(Serialize)]
pub struct StaleEntry {
    pub id: String,
    pub reason: String,
}

#[derive(Serialize)]
pub struct OrphanEntry {
    pub id: String,
    pub child_pid: u32,
}

fn scan(_opts: &DoctorOptions) -> DoctorReport {
    let dir = get_state_dir();
    let mut report = DoctorReport::default();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return report,
    };

    let mut sock_ids: Vec<String> = Vec::new();
    let mut meta_ids: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_suffix(".sock") {
            if !id.is_empty() {
                sock_ids.push(id.to_string());
            }
            continue;
        }
        if let Some(id) = name.strip_suffix(".meta") {
            if !id.is_empty() {
                meta_ids.push(id.to_string());
            }
            continue;
        }
        let Some(id) = name.strip_suffix(".pid") else {
            continue;
        };
        if id.is_empty() {
            continue;
        }

        let pid: Option<u32> = fs::read_to_string(entry.path())
            .ok()
            .and_then(|s| s.trim().parse().ok());

        match pid {
            Some(pid) if is_pid_alive(pid) => {
                let m = Meta::load(&meta_path(id)).ok();
                report.live.push(LiveEntry {
                    id: id.to_string(),
                    daemon_pid: pid,
                    child_pid: m.as_ref().and_then(|m| m.child_pid),
                    name: m.as_ref().and_then(|m| m.name.clone()),
                    project: m.map(|m| m.project).unwrap_or_default(),
                });
            }
            Some(_) => {
                // pid file but process gone
                report.stale.push(StaleEntry {
                    id: id.to_string(),
                    reason: "daemon process gone".into(),
                });
                // Orphan-child detection: meta may still be on disk with the
                // child's pid. Doctor reports living children whose parent
                // (daemon) died.
                if let Ok(m) = Meta::load(&meta_path(id)) {
                    if let Some(child) = m.child_pid {
                        if is_pid_alive(child) {
                            report.orphans.push(OrphanEntry {
                                id: id.to_string(),
                                child_pid: child,
                            });
                        }
                    }
                }
            }
            None => {
                report.stale.push(StaleEntry {
                    id: id.to_string(),
                    reason: "unreadable .pid".into(),
                });
            }
        }
    }

    // Orphan .sock files (no matching .pid).
    for id in &sock_ids {
        if !pid_path(id).exists() {
            report.stale.push(StaleEntry {
                id: id.clone(),
                reason: "orphan .sock".into(),
            });
        }
    }
    // Orphan .meta files (no matching .pid) — daemon died abruptly.
    for id in &meta_ids {
        if !pid_path(id).exists() {
            // Try to surface a child orphan even when .pid is gone but .meta
            // survives (e.g. external rm of .pid).
            if let Ok(m) = Meta::load(&meta_path(id)) {
                if let Some(child) = m.child_pid {
                    if is_pid_alive(child) {
                        report.orphans.push(OrphanEntry {
                            id: id.clone(),
                            child_pid: child,
                        });
                    }
                }
            }
            report.stale.push(StaleEntry {
                id: id.clone(),
                reason: "orphan .meta".into(),
            });
        }
    }

    // Misuse heuristic via recent.jsonl.
    if let Some(msg) = misuse_warning() {
        report.warnings.push(msg);
    }

    report
}

fn apply_fixes(report: &DoctorReport) {
    for stale in &report.stale {
        cleanup_stale_files(&stale.id);
    }
    for o in &report.orphans {
        #[cfg(unix)]
        unsafe {
            // Best-effort: SIGTERM, brief grace, SIGKILL fallback.
            libc::kill(o.child_pid as i32, libc::SIGTERM);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        #[cfg(unix)]
        unsafe {
            if libc::kill(o.child_pid as i32, 0) == 0 {
                libc::kill(o.child_pid as i32, libc::SIGKILL);
            }
        }
        // Also clean up the orphan's sidecars since the daemon is gone.
        cleanup_stale_files(&o.id);
    }
}

fn print_human(report: &DoctorReport) {
    if report.live.is_empty()
        && report.stale.is_empty()
        && report.orphans.is_empty()
        && report.warnings.is_empty()
    {
        println!("agent-terminal: clean (no live, no stale, no orphans)");
        return;
    }

    if !report.live.is_empty() {
        println!("live daemons:");
        for e in &report.live {
            let name = e.name.as_deref().unwrap_or("-");
            let child = e
                .child_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into());
            println!(
                "  {id:<14} pid={pid:<6} child={child:<6} name={name:<14} project={proj}",
                id = e.id,
                pid = e.daemon_pid,
                proj = e.project,
            );
        }
    }
    if !report.stale.is_empty() {
        println!("stale entries:");
        for s in &report.stale {
            println!("  {} ({})", s.id, s.reason);
        }
    }
    if !report.orphans.is_empty() {
        println!("orphan children (daemon vanished):");
        for o in &report.orphans {
            println!("  {} child_pid={}", o.id, o.child_pid);
        }
    }
    if !report.warnings.is_empty() {
        println!("warnings:");
        for w in &report.warnings {
            println!("  ! {w}");
        }
    }
}

#[derive(Debug, Deserialize)]
struct RecentEntry {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    started_at: u64,
    exited_at: u64,
    duration_ms: u64,
}

fn misuse_warning() -> Option<String> {
    let content = fs::read_to_string(recent_log_path()).ok()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut count = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(e) = serde_json::from_str::<RecentEntry>(line) else {
            continue;
        };
        if now.saturating_sub(e.exited_at) > MISUSE_WINDOW_SECS {
            continue;
        }
        if e.duration_ms < MISUSE_DURATION_MS {
            count += 1;
        }
    }

    if count >= MISUSE_COUNT {
        Some(format!(
            "Detected {count} short-lived daemons (<{}s each) in the past hour. \
             For one-shot commands, run them through `bash` directly instead of `agent-terminal spawn`.",
            MISUSE_DURATION_MS / 1000
        ))
    } else {
        None
    }
}

/// Renders the misuse warning so doctor's JSON output is deterministic for
/// tests. Returns the rendered string so callers can inject it into report.warnings.
#[cfg(test)]
pub(crate) fn _misuse_warning_for_test() -> Option<String> {
    misuse_warning()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// PID 1 (init) exists on every unix system, and `kill(1, 0)` from a
    /// non-root user returns EPERM. The agent-browser-inherited semantics
    /// say EPERM counts as alive — verify here so a future refactor doesn't
    /// silently flip that.
    #[test]
    fn is_pid_alive_treats_eperm_as_alive() {
        // If we happen to be root, kill(1,0) returns 0 ("alive") — that's
        // also a pass. Either way, init is "alive" to us.
        assert!(is_pid_alive(1));
    }

    #[test]
    fn misuse_warning_fires_at_threshold() {
        let _lock = lock_env();
        let dir = TempDir::new().unwrap();
        let prev = std::env::var("AGENT_TERMINAL_STATE_DIR").ok();
        std::env::set_var("AGENT_TERMINAL_STATE_DIR", dir.path());

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut content = String::new();
        for i in 0..MISUSE_COUNT {
            content.push_str(&serde_json::to_string(&json!({
                "id": format!("id{i}"),
                "started_at": now - 10,
                "exited_at": now - 9,
                "duration_ms": 1000,
                "reason": "child_exited",
            })).unwrap());
            content.push('\n');
        }
        fs::write(dir.path().join("recent.jsonl"), content).unwrap();

        let w = misuse_warning();
        assert!(w.is_some(), "expected misuse warning");
        assert!(w.unwrap().contains("short-lived"));

        // Below threshold: nothing
        let mut short = String::new();
        for i in 0..(MISUSE_COUNT - 1) {
            short.push_str(&serde_json::to_string(&json!({
                "id": format!("id{i}"),
                "started_at": now - 10,
                "exited_at": now - 9,
                "duration_ms": 1000,
                "reason": "child_exited",
            })).unwrap());
            short.push('\n');
        }
        fs::write(dir.path().join("recent.jsonl"), short).unwrap();
        assert!(misuse_warning().is_none());

        // Stale entries (older than 1h) don't count
        let mut stale = String::new();
        for i in 0..MISUSE_COUNT {
            stale.push_str(&serde_json::to_string(&json!({
                "id": format!("id{i}"),
                "started_at": now - 7200,
                "exited_at": now - 7100,
                "duration_ms": 1000,
                "reason": "child_exited",
            })).unwrap());
            stale.push('\n');
        }
        fs::write(dir.path().join("recent.jsonl"), stale).unwrap();
        assert!(misuse_warning().is_none());

        // Long-lived entries don't count
        let mut long = String::new();
        for i in 0..MISUSE_COUNT {
            long.push_str(&serde_json::to_string(&json!({
                "id": format!("id{i}"),
                "started_at": now - 60,
                "exited_at": now - 10,
                "duration_ms": 50_000,
                "reason": "child_exited",
            })).unwrap());
            long.push('\n');
        }
        fs::write(dir.path().join("recent.jsonl"), long).unwrap();
        assert!(misuse_warning().is_none());

        match prev {
            Some(v) => std::env::set_var("AGENT_TERMINAL_STATE_DIR", v),
            None => std::env::remove_var("AGENT_TERMINAL_STATE_DIR"),
        }
    }
}

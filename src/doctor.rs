use std::fs;
use std::path::Path;
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

/// Default age threshold for `--fix`-driven orphan-log cleanup.
pub const DEFAULT_LOG_AGE_DAYS: u32 = 7;

pub struct DoctorOptions {
    pub fix: bool,
    pub json: bool,
    /// Age threshold in days for `--fix` orphan-log cleanup. 0 deletes all
    /// orphan logs unconditionally. Ignored unless `fix` is set.
    pub log_age_days: u32,
}

pub fn run(opts: DoctorOptions) -> ExitCode {
    let report = scan(&opts);

    if opts.json {
        let body = serde_json::to_string_pretty(&report)
            .unwrap_or_else(|_| "{}".into());
        println!("{}", body);
    } else {
        print_human(&report, opts.log_age_days);
    }

    if opts.fix {
        apply_fixes(&report, opts.log_age_days);
    }

    // Exit code: 0 if no problems or all fixed; 1 if there are issues and
    // --fix was not specified. Orphan logs are advisory only — they're a
    // by-design artifact, not a malfunction — and so do not bump the code.
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
    /// `.log` / `.log.N` files whose owning daemon is gone. Intentionally
    /// retained for post-mortem `grep`/`slice`/`summary`, but surfaced here so
    /// they don't masquerade as a failed spawn and so `--fix` can GC old ones.
    pub orphan_logs: Vec<OrphanLogEntry>,
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

#[derive(Serialize)]
pub struct OrphanLogEntry {
    pub id: String,
    /// Filename, e.g. `abcd1234.log` or `abcd1234.log.2`.
    pub file: String,
    pub age_secs: u64,
    pub size_bytes: u64,
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
    let mut log_files: Vec<(String, String)> = Vec::new(); // (id, filename)
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = parse_log_filename(&name) {
            log_files.push((id, name));
            continue;
        }
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

    // Orphan log files: `.log` / `.log.N` with no live daemon. These are
    // retained on purpose (post-mortem grep/slice), but surfacing them here
    // closes the "did my spawn fail?" gap when a state dir accumulates them.
    for (id, file) in &log_files {
        if pid_path(id).exists() {
            continue;
        }
        let path = dir.join(file);
        let (age_secs, size_bytes) = log_stats(&path, now);
        report.orphan_logs.push(OrphanLogEntry {
            id: id.clone(),
            file: file.clone(),
            age_secs,
            size_bytes,
        });
    }
    report
        .orphan_logs
        .sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.file.cmp(&b.file)));

    // Misuse heuristic via recent.jsonl.
    if let Some(msg) = misuse_warning() {
        report.warnings.push(msg);
    }

    report
}

/// Returns the bare daemon id if `name` matches `{id}.log` or `{id}.log.N`
/// (where N is a non-negative integer). Returns None otherwise.
fn parse_log_filename(name: &str) -> Option<String> {
    if let Some(id) = name.strip_suffix(".log") {
        if !id.is_empty() {
            return Some(id.to_string());
        }
        return None;
    }
    let (head, num) = name.rsplit_once('.')?;
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let id = head.strip_suffix(".log")?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

fn log_stats(path: &Path, now: u64) -> (u64, u64) {
    let Ok(md) = fs::metadata(path) else {
        return (0, 0);
    };
    let size = md.len();
    let mtime_secs = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(now);
    let age = now.saturating_sub(mtime_secs);
    (age, size)
}

fn apply_fixes(report: &DoctorReport, log_age_days: u32) {
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

    // Orphan-log GC. Threshold is age-based so logs from a still-relevant
    // post-mortem window survive a casual `doctor --fix`. `log_age_days == 0`
    // is the user explicitly asking for an unconditional sweep.
    let threshold = (log_age_days as u64).saturating_mul(86_400);
    let dir = get_state_dir();
    for log in &report.orphan_logs {
        if log.age_secs < threshold {
            continue;
        }
        let _ = fs::remove_file(dir.join(&log.file));
    }
}

fn print_human(report: &DoctorReport, log_age_days: u32) {
    if report.live.is_empty()
        && report.stale.is_empty()
        && report.orphans.is_empty()
        && report.orphan_logs.is_empty()
        && report.warnings.is_empty()
    {
        println!("agent-term: clean (no live, no stale, no orphans)");
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
    if !report.orphan_logs.is_empty() {
        let total_size: u64 = report.orphan_logs.iter().map(|l| l.size_bytes).sum();
        let threshold_secs = (log_age_days as u64).saturating_mul(86_400);
        let eligible = report
            .orphan_logs
            .iter()
            .filter(|l| l.age_secs >= threshold_secs)
            .count();
        println!(
            "orphan logs ({} files, {}; retained for post-mortem grep/slice/summary):",
            report.orphan_logs.len(),
            format_bytes(total_size),
        );
        for l in &report.orphan_logs {
            println!(
                "  {file:<22} age={age:<8} {size}",
                file = l.file,
                age = format_age(l.age_secs),
                size = format_bytes(l.size_bytes),
            );
        }
        println!(
            "  hint: `agent-term doctor --fix --log-age-days {}` removes {} log(s) older than {} day(s)",
            log_age_days, eligible, log_age_days,
        );
    }
    if !report.warnings.is_empty() {
        println!("warnings:");
        for w in &report.warnings {
            println!("  ! {w}");
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1}GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
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
             For one-shot commands, run them through `bash` directly instead of `agent-term spawn`.",
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
        let prev = std::env::var("AGENT_TERM_STATE_DIR").ok();
        std::env::set_var("AGENT_TERM_STATE_DIR", dir.path());

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
            Some(v) => std::env::set_var("AGENT_TERM_STATE_DIR", v),
            None => std::env::remove_var("AGENT_TERM_STATE_DIR"),
        }
    }

    #[test]
    fn parse_log_filename_accepts_bare_and_rotated() {
        assert_eq!(
            parse_log_filename("abcd1234.log"),
            Some("abcd1234".to_string())
        );
        assert_eq!(
            parse_log_filename("abcd1234.log.1"),
            Some("abcd1234".to_string())
        );
        assert_eq!(
            parse_log_filename("abcd1234.log.42"),
            Some("abcd1234".to_string())
        );
    }

    #[test]
    fn parse_log_filename_rejects_non_log() {
        assert_eq!(parse_log_filename("abcd1234.sock"), None);
        assert_eq!(parse_log_filename("abcd1234.meta"), None);
        assert_eq!(parse_log_filename("recent.jsonl"), None);
        assert_eq!(parse_log_filename(".log"), None);
        assert_eq!(parse_log_filename("abcd1234.log.tmp"), None);
        // Trailing dot or non-numeric suffix after `.log.` is not a rotation segment.
        assert_eq!(parse_log_filename("abcd1234.log."), None);
        assert_eq!(parse_log_filename("abcd1234.logfoo"), None);
    }

    #[test]
    fn scan_surfaces_orphan_logs_and_fix_respects_age() {
        let _lock = lock_env();
        let dir = TempDir::new().unwrap();
        let prev = std::env::var("AGENT_TERM_STATE_DIR").ok();
        std::env::set_var("AGENT_TERM_STATE_DIR", dir.path());

        // Live daemon (this process): its log should NOT be orphan.
        let live_id = "livelive1234";
        fs::write(
            dir.path().join(format!("{live_id}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();
        fs::write(dir.path().join(format!("{live_id}.log")), b"live").unwrap();

        // Two orphan logs: one fresh, one we'll backdate to old.
        let fresh_id = "freshfresh1";
        let old_id = "oldoldoldol";
        fs::write(dir.path().join(format!("{fresh_id}.log")), b"fresh").unwrap();
        fs::write(dir.path().join(format!("{old_id}.log")), b"old").unwrap();
        // Rotated orphan log: ensure rotation segments are detected too.
        fs::write(dir.path().join(format!("{old_id}.log.1")), b"old1").unwrap();

        // Backdate the "old" entries by 10 days. Uses File::set_modified
        // (stable since Rust 1.75) so we don't add a new dep just for tests.
        let ten_days_ago = SystemTime::now() - std::time::Duration::from_secs(10 * 86_400);
        for name in [format!("{old_id}.log"), format!("{old_id}.log.1")] {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(dir.path().join(&name))
                .unwrap();
            f.set_modified(ten_days_ago).unwrap();
        }

        let report = scan(&DoctorOptions {
            fix: false,
            json: false,
            log_age_days: DEFAULT_LOG_AGE_DAYS,
        });

        let files: Vec<&str> = report.orphan_logs.iter().map(|l| l.file.as_str()).collect();
        assert!(!files.iter().any(|f| f.starts_with(live_id)),
            "live daemon's log should not be an orphan: got {files:?}");
        assert!(files.contains(&format!("{fresh_id}.log").as_str()));
        assert!(files.contains(&format!("{old_id}.log").as_str()));
        assert!(files.contains(&format!("{old_id}.log.1").as_str()));

        // --fix with 7-day threshold removes the old ones but keeps the fresh.
        apply_fixes(&report, DEFAULT_LOG_AGE_DAYS);
        assert!(dir.path().join(format!("{fresh_id}.log")).exists());
        assert!(!dir.path().join(format!("{old_id}.log")).exists());
        assert!(!dir.path().join(format!("{old_id}.log.1")).exists());
        // Live daemon's log is never touched by orphan-log GC.
        assert!(dir.path().join(format!("{live_id}.log")).exists());

        // --fix with log_age_days=0 sweeps the remaining fresh orphan.
        let report = scan(&DoctorOptions {
            fix: true,
            json: false,
            log_age_days: 0,
        });
        apply_fixes(&report, 0);
        assert!(!dir.path().join(format!("{fresh_id}.log")).exists());
        assert!(dir.path().join(format!("{live_id}.log")).exists());

        match prev {
            Some(v) => std::env::set_var("AGENT_TERM_STATE_DIR", v),
            None => std::env::remove_var("AGENT_TERM_STATE_DIR"),
        }
    }
}

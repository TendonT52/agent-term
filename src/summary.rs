use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::RegexBuilder;
use serde::Serialize;

use crate::client;
use crate::ids::is_valid_id;
use crate::meta::Meta;
use crate::pty::parse_ms_prefix;
use crate::state::{get_state_dir, log_path, meta_path};
use crate::tail::parse_time_spec;

pub const DEFAULT_RECENT_WINDOW: &str = "60s";
pub const DEFAULT_ERROR_PATTERN: &str = "(?i)error|fatal";
pub const DEFAULT_WARNING_PATTERN: &str = "(?i)warn";

pub struct SummaryOptions {
    pub id: String,
    pub json: bool,
    pub recent_window: Option<String>,
    pub error_pattern: Option<String>,
    pub warning_pattern: Option<String>,
}

#[derive(Serialize)]
struct SummaryReport {
    schema_version: u32,
    id: String,
    name: Option<String>,
    project: Option<String>,
    state: String,
    child_pid: Option<u32>,
    exit_code: Option<i32>,
    started_at: Option<u64>,
    uptime_ms: u64,
    log_bytes: u64,
    log_lines: u64,
    segments: u32,
    last_line_age_ms: Option<u64>,
    tail_cursor: u64,
    recent: Option<RecentStats>,
}

#[derive(Serialize)]
struct RecentStats {
    since_ms: u64,
    lines_scanned: u64,
    errors: u64,
    warnings: u64,
    mode: &'static str, // "time-window" or "tail-bytes"
}

pub fn run(opts: SummaryOptions) -> ExitCode {
    let SummaryOptions {
        id,
        json,
        recent_window,
        error_pattern,
        warning_pattern,
    } = opts;

    if !is_valid_id(&id) {
        eprintln!("agent-term: summary: invalid id {id:?}");
        return ExitCode::from(1);
    }

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let now_ms = now_secs * 1000
        + (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_millis() as u64)
            .unwrap_or(0));

    // ---- Live state from the daemon (if alive) ----
    let daemon_view = client::status(&id).ok();
    let (state, child_pid, exit_code, daemon_line_count, daemon_last_line_ms) =
        match daemon_view {
            Some(data) => {
                let state = data
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let child_pid = data
                    .get("child_pid")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let exit_code = data
                    .get("code")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);
                // The daemon only embeds counters in the dedicated `summary`
                // IPC action; reach for those if we want them.
                let summary_view = send_summary_request(&id).ok();
                let (line_count, last_line_ms) = match summary_view {
                    Some(s) => (
                        s.get("line_count").and_then(|v| v.as_u64()),
                        s.get("last_line_at_ms").and_then(|v| v.as_u64()),
                    ),
                    None => (None, None),
                };
                (state, child_pid, exit_code, line_count, last_line_ms)
            }
            None => ("unknown".to_string(), None, None, None, None),
        };

    // ---- Meta + file-side facts ----
    let meta = Meta::load(&meta_path(&id)).ok();
    let name = meta.as_ref().and_then(|m| m.name.clone());
    let project = meta.as_ref().map(|m| m.project.clone());
    let started_at = meta.as_ref().map(|m| m.started_at);

    let log = log_path(&id);
    if !log.exists() {
        eprintln!("agent-term: summary: no log for id {id:?}");
        return ExitCode::from(1);
    }

    let log_bytes = fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let segments = count_segments(&log);

    // log_lines: prefer the daemon-maintained counter; fall back to a quick
    // scan of the current segment (rotated segments aren't counted in the
    // fallback path — daemon-side is the right answer).
    let log_lines = match daemon_line_count {
        Some(n) => n,
        None => count_newlines_in_file(&log).unwrap_or(0),
    };

    let last_line_age_ms = match daemon_last_line_ms {
        Some(ms) if ms > 0 => Some(now_ms.saturating_sub(ms)),
        _ => fs::metadata(&log)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| now_ms.saturating_sub(d.as_millis() as u64)),
    };

    let uptime_ms = started_at
        .map(|s| now_ms.saturating_sub(s * 1000))
        .unwrap_or(0);

    // ---- Recent error/warning counts ----
    let recent_window_spec = recent_window.as_deref().unwrap_or(DEFAULT_RECENT_WINDOW);
    let recent_since_ms = match parse_time_spec(recent_window_spec, now_ms) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("agent-term: summary: --recent-window: {e}");
            return ExitCode::from(1);
        }
    };
    let recent_window_dur = now_ms.saturating_sub(recent_since_ms);

    let err_pat = error_pattern.as_deref().unwrap_or(DEFAULT_ERROR_PATTERN);
    let warn_pat = warning_pattern.as_deref().unwrap_or(DEFAULT_WARNING_PATTERN);
    let err_re = match RegexBuilder::new(err_pat).build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agent-term: summary: --error-pattern: {e}");
            return ExitCode::from(1);
        }
    };
    let warn_re = match RegexBuilder::new(warn_pat).build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agent-term: summary: --warning-pattern: {e}");
            return ExitCode::from(1);
        }
    };

    let recent = scan_recent(&log, recent_since_ms, recent_window_dur, &err_re, &warn_re);

    let report = SummaryReport {
        schema_version: 1,
        id: id.clone(),
        name,
        project,
        state,
        child_pid,
        exit_code,
        started_at,
        uptime_ms,
        log_bytes,
        log_lines,
        segments,
        last_line_age_ms,
        tail_cursor: log_bytes,
        recent,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
        );
    } else {
        print_human(&report);
    }
    ExitCode::SUCCESS
}

fn send_summary_request(id: &str) -> Result<serde_json::Value, String> {
    let resp = client::send_request(
        id,
        &crate::ipc::Request {
            action: "summary".into(),
            sig: None,
        },
    )?;
    if !resp.success {
        return Err(resp.error.unwrap_or_else(|| "summary failed".into()));
    }
    resp.data.ok_or_else(|| "no summary data".into())
}

fn count_segments(log: &Path) -> u32 {
    let parent = match log.parent() {
        Some(p) => p,
        None => return 0,
    };
    let base = match log.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => return 0,
    };
    let dir = match fs::read_dir(parent) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let mut count = 0u32;
    for entry in dir.flatten() {
        let n = entry.file_name().to_string_lossy().into_owned();
        if n == base {
            count += 1;
        } else if let Some(rest) = n.strip_prefix(&format!("{}.", base)) {
            if rest.chars().all(|c| c.is_ascii_digit()) {
                count += 1;
            }
        }
    }
    count
}

fn count_newlines_in_file(p: &Path) -> std::io::Result<u64> {
    let mut f = fs::File::open(p)?;
    f.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; 8192];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf)?;
        if r == 0 {
            break;
        }
        n += buf[..r].iter().filter(|&&b| b == b'\n').count() as u64;
    }
    Ok(n)
}

/// Scans the tail of `log` for error/warning matches within the recent
/// window. If the log has parseable `[ms] ` prefixes it uses a real time
/// window; otherwise it falls back to "last 64 KiB" so the function still
/// returns useful counts.
fn scan_recent(
    log: &Path,
    since_ms: u64,
    window_dur_ms: u64,
    err_re: &regex::Regex,
    warn_re: &regex::Regex,
) -> Option<RecentStats> {
    let mut file = fs::File::open(log).ok()?;
    let len = file.seek(SeekFrom::End(0)).ok()?;
    if len == 0 {
        return Some(RecentStats {
            since_ms: window_dur_ms,
            lines_scanned: 0,
            errors: 0,
            warnings: 0,
            mode: "tail-bytes",
        });
    }

    // Always read at most the last 1 MiB. Bounds the work for huge logs.
    let max_scan: u64 = 1 << 20;
    let scan_start = len.saturating_sub(max_scan);
    file.seek(SeekFrom::Start(scan_start)).ok()?;
    let mut buf = Vec::with_capacity((len - scan_start) as usize);
    file.read_to_end(&mut buf).ok()?;

    let mut lines_scanned = 0u64;
    let mut errors = 0u64;
    let mut warnings = 0u64;
    let mut mode_time = false;

    let mut start = 0usize;
    for i in 0..buf.len() {
        if buf[i] != b'\n' {
            continue;
        }
        let line = &buf[start..=i];
        start = i + 1;
        let body = match parse_ms_prefix(line) {
            Some((ts, off)) => {
                mode_time = true;
                if ts < since_ms {
                    continue;
                }
                &line[off..]
            }
            None => line,
        };
        lines_scanned += 1;
        let s = String::from_utf8_lossy(body);
        if err_re.is_match(&s) {
            errors += 1;
        } else if warn_re.is_match(&s) {
            warnings += 1;
        }
    }

    Some(RecentStats {
        since_ms: window_dur_ms,
        lines_scanned,
        errors,
        warnings,
        mode: if mode_time { "time-window" } else { "tail-bytes" },
    })
}

fn print_human(r: &SummaryReport) {
    println!("id              {}", r.id);
    println!("name            {}", r.name.as_deref().unwrap_or("-"));
    println!(
        "project         {}",
        r.project.as_deref().unwrap_or("-")
    );
    let child = r.child_pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
    let exit = match r.exit_code {
        Some(c) => format!(" exit_code={}", c),
        None => String::new(),
    };
    println!("state           {}  child_pid={}{}", r.state, child, exit);
    println!("uptime          {}", format_duration_ms(r.uptime_ms));
    println!(
        "log             {} bytes, {} lines, {} segment(s)",
        r.log_bytes, r.log_lines, r.segments
    );
    if let Some(age) = r.last_line_age_ms {
        println!("last line       {} ago", format_duration_ms(age));
    } else {
        println!("last line       -");
    }
    println!("tail cursor     {}", r.tail_cursor);
    if let Some(rec) = &r.recent {
        println!(
            "recent ({})  errors={}  warnings={}  scanned={}  mode={}",
            format_duration_ms(rec.since_ms),
            rec.errors,
            rec.warnings,
            rec.lines_scanned,
            rec.mode
        );
    }
}

fn format_duration_ms(ms: u64) -> String {
    let s = ms / 1000;
    let sub = ms % 1000;
    if s < 1 {
        return format!("{ms}ms");
    }
    if s < 60 {
        return format!("{s}.{:03}s", sub);
    }
    if s < 3600 {
        return format!("{}m{:02}s", s / 60, s % 60);
    }
    if s < 86400 {
        return format!("{}h{:02}m", s / 3600, (s % 3600) / 60);
    }
    format!("{}d{:02}h", s / 86400, (s % 86400) / 3600)
}

#[allow(dead_code)]
fn unused_state_dir() -> std::path::PathBuf {
    get_state_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn format_duration_humanises_ranges() {
        assert_eq!(format_duration_ms(0), "0ms");
        assert_eq!(format_duration_ms(999), "999ms");
        assert_eq!(format_duration_ms(1_000), "1.000s");
        assert_eq!(format_duration_ms(61_000), "1m01s");
        assert_eq!(format_duration_ms(3_660_000), "1h01m");
        assert_eq!(format_duration_ms(90_000_000), "1d01h");
    }

    #[test]
    fn count_segments_counts_current_and_rotated() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("a.log");
        fs::write(&log, b"hi").unwrap();
        fs::write(dir.path().join("a.log.1"), b"x").unwrap();
        fs::write(dir.path().join("a.log.2"), b"x").unwrap();
        // Distractor: unrelated file shouldn't count.
        fs::write(dir.path().join("a.log.tmp"), b"x").unwrap();
        // Unrelated id.
        fs::write(dir.path().join("b.log"), b"x").unwrap();
        assert_eq!(count_segments(&log), 3);
    }

    #[test]
    fn count_newlines_scans_full_file() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("c.log");
        fs::write(&log, b"a\nb\nc\nd").unwrap();
        assert_eq!(count_newlines_in_file(&log).unwrap(), 3);
    }

    #[test]
    fn scan_recent_uses_time_window_when_prefix_present() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("r.log");
        // Three lines, one old, two recent. Errors and warnings sprinkled.
        let body = "[100] INFO startup\n[2000] WARN slow\n[2500] ERROR boom\n";
        fs::write(&log, body).unwrap();
        let err = RegexBuilder::new("(?i)error").build().unwrap();
        let warn = RegexBuilder::new("(?i)warn").build().unwrap();
        // since=1500, window=1000 (decorative; only since matters for filtering)
        let r = scan_recent(&log, 1500, 1000, &err, &warn).unwrap();
        assert_eq!(r.lines_scanned, 2);
        assert_eq!(r.errors, 1);
        assert_eq!(r.warnings, 1);
        assert_eq!(r.mode, "time-window");
    }

    #[test]
    fn scan_recent_falls_back_to_tail_bytes_without_prefix() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("nb.log");
        let body = "INFO ok\nERROR boom\nWARN slow\n";
        fs::write(&log, body).unwrap();
        let err = RegexBuilder::new("(?i)error").build().unwrap();
        let warn = RegexBuilder::new("(?i)warn").build().unwrap();
        let r = scan_recent(&log, 0, 60_000, &err, &warn).unwrap();
        assert_eq!(r.lines_scanned, 3);
        assert_eq!(r.errors, 1);
        assert_eq!(r.warnings, 1);
        assert_eq!(r.mode, "tail-bytes");
    }
}

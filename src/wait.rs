use std::fs;
use std::io::Read;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use regex::RegexBuilder;
use serde_json::json;

use crate::client;
use crate::ids::is_valid_id;
use crate::state::{log_path, pid_path};

const POLL: Duration = Duration::from_millis(100);

pub struct WaitOptions {
    pub id: String,
    pub pattern: Option<String>,
    pub pattern_file: Option<String>,
    pub timeout: Option<String>,
    pub multiline: bool,
    pub json: bool,
}

pub fn run(opts: WaitOptions) -> ExitCode {
    let WaitOptions {
        id,
        pattern,
        pattern_file,
        timeout,
        multiline,
        json,
    } = opts;

    if !is_valid_id(&id) {
        return fail_json_or_text(json, "invalid id", 1, |_| {
            eprintln!("agent-terminal: invalid id {id:?}")
        });
    }

    let pattern_text = match (pattern, pattern_file) {
        (Some(p), None) => p,
        (None, Some(path)) => match fs::read_to_string(&path) {
            Ok(s) => s.trim_end_matches('\n').to_string(),
            Err(e) => {
                return fail_json_or_text(json, &format!("read pattern-file: {e}"), 1, |_| {
                    eprintln!("agent-terminal: wait: read --pattern-file {path}: {e}")
                });
            }
        },
        (Some(_), Some(_)) => {
            return fail_json_or_text(
                json,
                "--pattern and --pattern-file are mutually exclusive",
                1,
                |m| eprintln!("agent-terminal: wait: {m}"),
            );
        }
        (None, None) => {
            return fail_json_or_text(json, "--pattern or --pattern-file required", 1, |m| {
                eprintln!("agent-terminal: wait: {m}")
            });
        }
    };

    let regex = match RegexBuilder::new(&pattern_text).multi_line(multiline).build() {
        Ok(r) => r,
        Err(e) => {
            return fail_json_or_text(json, &format!("bad regex: {e}"), 1, |m| {
                eprintln!("agent-terminal: wait: {m}")
            });
        }
    };

    let timeout_dur = match timeout.as_deref() {
        Some(s) => match parse_timeout(s) {
            Ok(d) => Some(d),
            Err(e) => {
                return fail_json_or_text(json, &e, 1, |m| {
                    eprintln!("agent-terminal: wait: {m}")
                });
            }
        },
        None => None,
    };

    let log = log_path(&id);
    let start = Instant::now();

    // Open the log lazily: log may not exist yet if the daemon is mid-startup.
    let mut file: Option<fs::File> = None;
    let mut accum: Vec<u8> = Vec::with_capacity(8192);
    let mut next_line_start = 0usize;

    loop {
        if file.is_none() {
            if let Ok(f) = fs::File::open(&log) {
                file = Some(f);
            }
        }
        if let Some(f) = file.as_mut() {
            drain(f, &mut accum);
        }

        if let Some(m) = find_match(&regex, &accum, multiline, &mut next_line_start) {
            return emit_match(&m, start.elapsed(), json);
        }

        if let Some(t) = timeout_dur {
            if start.elapsed() >= t {
                return emit_timeout(start.elapsed(), json);
            }
        }

        if let Some(exit_info) = check_child_exited(&id) {
            // One more drain in case the daemon flushed the final bytes after
            // our last read but before its sidecars vanished.
            if let Some(f) = file.as_mut() {
                drain(f, &mut accum);
            }
            if let Some(m) = find_match(&regex, &accum, multiline, &mut next_line_start) {
                return emit_match(&m, start.elapsed(), json);
            }
            return emit_child_exited(exit_info, start.elapsed(), json);
        }

        thread::sleep(POLL);
    }
}

fn drain(file: &mut fs::File, accum: &mut Vec<u8>) {
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => accum.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
}

/// In line-mode, walks newly-arrived complete lines (from `*line_cursor`
/// onward) and returns the first matching line text. In multiline mode,
/// runs the regex over the whole accumulator and returns the surrounding
/// line of the first match.
fn find_match(
    regex: &regex::Regex,
    accum: &[u8],
    multiline: bool,
    line_cursor: &mut usize,
) -> Option<String> {
    if multiline {
        let haystack = String::from_utf8_lossy(accum);
        let m = regex.find(&haystack)?;
        let start = haystack[..m.start()]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = haystack[m.end()..]
            .find('\n')
            .map(|i| m.end() + i)
            .unwrap_or(haystack.len());
        let line = haystack[start..end].trim_end_matches('\r').to_string();
        return Some(line);
    }

    while *line_cursor < accum.len() {
        let rest = &accum[*line_cursor..];
        let Some(rel_nl) = rest.iter().position(|&b| b == b'\n') else {
            break;
        };
        let abs_end = *line_cursor + rel_nl;
        let line_bytes = &accum[*line_cursor..abs_end];
        let line_bytes = match line_bytes.last() {
            Some(&b'\r') => &line_bytes[..line_bytes.len() - 1],
            _ => line_bytes,
        };
        let line_str = String::from_utf8_lossy(line_bytes);
        if regex.is_match(&line_str) {
            *line_cursor = abs_end + 1;
            return Some(line_str.into_owned());
        }
        *line_cursor = abs_end + 1;
    }
    None
}

/// Best-effort check of whether the child has stopped running. Returns the
/// exit code (when available) if yes, None if still running.
fn check_child_exited(id: &str) -> Option<ChildExit> {
    // Fast path: if the .pid sidecar is gone, the daemon's cleanup already
    // ran, which only happens after the child exited and the linger expired.
    if !pid_path(id).exists() {
        return Some(ChildExit { code: None });
    }
    // Daemon is still up. Ask it.
    match client::status(id) {
        Ok(data) => {
            let state = data.get("state").and_then(|v| v.as_str()).unwrap_or("");
            if state == "exited" {
                let code = data.get("code").and_then(|v| v.as_i64()).map(|c| c as i32);
                Some(ChildExit { code })
            } else {
                None
            }
        }
        Err(_) => {
            // Daemon unreachable but pid file existed — most likely a race
            // window during shutdown. Treat as still-running so the next
            // iteration can re-check. If it persists, the pid file will be
            // removed and we'll fall through the fast path.
            None
        }
    }
}

#[derive(Clone, Copy)]
struct ChildExit {
    code: Option<i32>,
}

fn emit_match(line: &str, elapsed: Duration, json_mode: bool) -> ExitCode {
    if json_mode {
        println!(
            "{}",
            json!({
                "matched": true,
                "line": line,
                "elapsed_ms": elapsed.as_millis() as u64,
            })
        );
    } else {
        println!("{}", line);
    }
    ExitCode::SUCCESS
}

fn emit_timeout(elapsed: Duration, json_mode: bool) -> ExitCode {
    if json_mode {
        println!(
            "{}",
            json!({
                "matched": false,
                "reason": "timeout",
                "elapsed_ms": elapsed.as_millis() as u64,
            })
        );
    } else {
        eprintln!(
            "agent-terminal: wait: timed out after {} ms",
            elapsed.as_millis()
        );
    }
    ExitCode::from(1)
}

fn emit_child_exited(info: ChildExit, elapsed: Duration, json_mode: bool) -> ExitCode {
    if json_mode {
        println!(
            "{}",
            json!({
                "matched": false,
                "reason": "process_exited",
                "code": info.code,
                "elapsed_ms": elapsed.as_millis() as u64,
            })
        );
    } else {
        match info.code {
            Some(c) => eprintln!(
                "agent-terminal: wait: process exited (code {c}) before pattern matched"
            ),
            None => eprintln!(
                "agent-terminal: wait: process exited before pattern matched"
            ),
        }
    }
    ExitCode::from(2)
}

fn fail_json_or_text(json_mode: bool, msg: &str, code: u8, then: impl FnOnce(&str)) -> ExitCode {
    if json_mode {
        println!(
            "{}",
            json!({
                "matched": false,
                "reason": "error",
                "error": msg,
            })
        );
    } else {
        then(msg);
    }
    ExitCode::from(code)
}

/// Parses durations: bare digits = seconds; suffixes `ms`, `s`, `m`, `h`.
fn parse_timeout(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty timeout".into());
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    let (n_str, unit) = s.split_at(split);
    if n_str.is_empty() {
        return Err(format!("invalid timeout: {s}"));
    }
    let n: u64 = n_str
        .parse()
        .map_err(|_| format!("invalid timeout: {s}"))?;
    let ms = match unit.trim() {
        "" | "s" => n.saturating_mul(1_000),
        "ms" => n,
        "m" => n.saturating_mul(60_000),
        "h" => n.saturating_mul(3_600_000),
        other => return Err(format!("unknown timeout unit: {other}")),
    };
    Ok(Duration::from_millis(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_timeout("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_timeout("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_timeout("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_timeout("").is_err());
        assert!(parse_timeout("ms").is_err());
        assert!(parse_timeout("10x").is_err());
    }

    #[test]
    fn line_mode_finds_complete_line() {
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut cursor = 0;
        let buf = b"loading\r\nREADY\r\n".to_vec();
        let m = find_match(&re, &buf, false, &mut cursor).unwrap();
        assert_eq!(m, "READY");
    }

    #[test]
    fn line_mode_skips_incomplete_trailing_line() {
        // The partial "READY" without trailing newline must not match yet —
        // a future chunk could turn it into "READYNOT".
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut cursor = 0;
        let buf = b"booting\r\nREADY".to_vec();
        assert!(find_match(&re, &buf, false, &mut cursor).is_none());
    }

    #[test]
    fn line_mode_cursor_advances_past_checked_lines() {
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut cursor = 0;
        let mut buf = b"a\nb\n".to_vec();
        assert!(find_match(&re, &buf, false, &mut cursor).is_none());
        assert_eq!(cursor, buf.len());

        buf.extend_from_slice(b"READY\n");
        let m = find_match(&re, &buf, false, &mut cursor).unwrap();
        assert_eq!(m, "READY");
    }

    #[test]
    fn multiline_mode_matches_across_lines() {
        let re = RegexBuilder::new("start\\n.*\\nend")
            .multi_line(true)
            .dot_matches_new_line(false)
            .build()
            .unwrap();
        let mut cursor = 0;
        let buf = b"prelude\nstart\nmiddle\nend\nepilogue\n".to_vec();
        let m = find_match(&re, &buf, true, &mut cursor).unwrap();
        // Multiline returns the match extended to line boundaries on either side.
        assert_eq!(m, "start\nmiddle\nend");
    }

    #[test]
    fn multiline_anchors_use_line_boundaries() {
        let re = RegexBuilder::new("^READY$").multi_line(true).build().unwrap();
        let mut cursor = 0;
        let buf = b"chunk1 boot\nREADY\nchunk3 done\n".to_vec();
        let m = find_match(&re, &buf, true, &mut cursor).unwrap();
        assert_eq!(m, "READY");
    }
}

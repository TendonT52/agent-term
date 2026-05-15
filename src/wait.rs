use std::fs;
use std::io::Read;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use regex::RegexBuilder;
use serde_json::json;

use crate::client;
use crate::ids::is_valid_id;
use crate::pty::{parse_ms_prefix, AnsiStripper};
use crate::state::{log_path, pid_path};

const POLL: Duration = Duration::from_millis(100);
/// Size of the rolling tail kept of the *raw* (pre-strip) bytes. Used only to
/// detect ANSI escapes for the timeout hint — small bound, never logged.
const RAW_TAIL_CAP: usize = 4096;

pub struct WaitOptions {
    pub id: String,
    pub pattern: Option<String>,
    pub pattern_file: Option<String>,
    pub timeout: Option<String>,
    pub multiline: bool,
    pub strip_ansi: bool,
    pub match_full_line: bool,
    pub json: bool,
    pub ignore_case: bool,
}

pub fn run(opts: WaitOptions) -> ExitCode {
    let WaitOptions {
        id,
        pattern,
        pattern_file,
        timeout,
        multiline,
        strip_ansi,
        match_full_line,
        json,
        ignore_case,
    } = opts;

    if !is_valid_id(&id) {
        return fail_json_or_text(json, "invalid id", 1, |_| {
            eprintln!("agent-term: invalid id {id:?}")
        });
    }

    let pattern_text = match (pattern, pattern_file) {
        (Some(p), None) => p,
        (None, Some(path)) => match fs::read_to_string(&path) {
            Ok(s) => s.trim_end_matches('\n').to_string(),
            Err(e) => {
                return fail_json_or_text(json, &format!("read pattern-file: {e}"), 1, |_| {
                    eprintln!("agent-term: wait: read --pattern-file {path}: {e}")
                });
            }
        },
        (Some(_), Some(_)) => {
            return fail_json_or_text(
                json,
                "--pattern and --pattern-file are mutually exclusive",
                1,
                |m| eprintln!("agent-term: wait: {m}"),
            );
        }
        (None, None) => {
            return fail_json_or_text(json, "--pattern or --pattern-file required", 1, |m| {
                eprintln!("agent-term: wait: {m}")
            });
        }
    };

    let regex = match RegexBuilder::new(&pattern_text)
        .multi_line(multiline)
        .case_insensitive(ignore_case)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            return fail_json_or_text(json, &format!("bad regex: {e}"), 1, |m| {
                eprintln!("agent-term: wait: {m}")
            });
        }
    };

    let timeout_dur = match timeout.as_deref() {
        Some(s) => match parse_timeout(s) {
            Ok(d) => Some(d),
            Err(e) => {
                return fail_json_or_text(json, &e, 1, |m| {
                    eprintln!("agent-term: wait: {m}")
                });
            }
        },
        None => None,
    };

    let log = log_path(&id);
    let start = Instant::now();

    let mut file: Option<fs::File> = None;
    let mut accum: Vec<u8> = Vec::with_capacity(8192);
    let mut next_line_start = 0usize;
    let mut stripper = strip_ansi.then(AnsiStripper::new);
    let mut raw_tail: Vec<u8> = Vec::with_capacity(RAW_TAIL_CAP);
    let mut raw_has_ansi = false;

    loop {
        if file.is_none() {
            if let Ok(f) = fs::File::open(&log) {
                file = Some(f);
            }
        }
        if let Some(f) = file.as_mut() {
            drain(f, &mut accum, stripper.as_mut(), &mut raw_tail, &mut raw_has_ansi);
        }

        if let Some(m) = find_match(
            &regex,
            &accum,
            multiline,
            match_full_line,
            &mut next_line_start,
        ) {
            return emit_match(&m, start.elapsed(), json);
        }

        if let Some(t) = timeout_dur {
            if start.elapsed() >= t {
                let hint = if !strip_ansi && raw_has_ansi {
                    Some("log contains ANSI escapes — retry with --strip-ansi")
                } else {
                    None
                };
                return emit_timeout(start.elapsed(), hint, json);
            }
        }

        if let Some(exit_info) = check_child_exited(&id) {
            if let Some(f) = file.as_mut() {
                drain(f, &mut accum, stripper.as_mut(), &mut raw_tail, &mut raw_has_ansi);
            }
            if let Some(m) = find_match(
                &regex,
                &accum,
                multiline,
                match_full_line,
                &mut next_line_start,
            ) {
                return emit_match(&m, start.elapsed(), json);
            }
            let hint = if !strip_ansi && raw_has_ansi {
                Some("log contains ANSI escapes — retry with --strip-ansi")
            } else {
                None
            };
            return emit_child_exited(exit_info, start.elapsed(), hint, json);
        }

        thread::sleep(POLL);
    }
}

fn drain(
    file: &mut fs::File,
    accum: &mut Vec<u8>,
    mut stripper: Option<&mut AnsiStripper>,
    raw_tail: &mut Vec<u8>,
    raw_has_ansi: &mut bool,
) {
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                if !*raw_has_ansi && chunk.contains(&0x1b) {
                    *raw_has_ansi = true;
                }
                // Keep a small rolling tail of raw bytes so future ANSI checks
                // (e.g. if the daemon emits escapes only late in startup) keep
                // working without holding the whole pre-strip buffer.
                push_raw_tail(raw_tail, chunk);
                match stripper.as_deref_mut() {
                    Some(s) => s.feed(chunk, accum),
                    None => accum.extend_from_slice(chunk),
                }
            }
            Err(_) => break,
        }
    }
}

fn push_raw_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    if chunk.len() >= RAW_TAIL_CAP {
        tail.clear();
        tail.extend_from_slice(&chunk[chunk.len() - RAW_TAIL_CAP..]);
        return;
    }
    let needed = tail.len() + chunk.len();
    if needed > RAW_TAIL_CAP {
        let drop = needed - RAW_TAIL_CAP;
        tail.drain(..drop);
    }
    tail.extend_from_slice(chunk);
}

/// In line-mode, walks newly-arrived complete lines (from `*line_cursor`
/// onward) and returns the first matching line text. In multiline mode,
/// runs the regex over the whole accumulator and returns the surrounding
/// line of the first match.
///
/// When `match_full_line` is false, the `[<ms>] ` prefix (if any) is stripped
/// from the candidate before matching but preserved in the returned line.
/// Multiline mode always matches against the buffer as-is — prefixes can't be
/// scrubbed across a region of variable-length lines without rewriting the
/// haystack, and that would invalidate the match offsets.
fn find_match(
    regex: &regex::Regex,
    accum: &[u8],
    multiline: bool,
    match_full_line: bool,
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
        let candidate: &[u8] = if match_full_line {
            line_bytes
        } else {
            match parse_ms_prefix(line_bytes) {
                Some((_, body_offset)) => &line_bytes[body_offset..],
                None => line_bytes,
            }
        };
        let candidate_str = String::from_utf8_lossy(candidate);
        if regex.is_match(&candidate_str) {
            *line_cursor = abs_end + 1;
            // Return the full line text so the caller sees the timestamp prefix
            // intact — useful for "what time did the readiness line arrive?".
            return Some(String::from_utf8_lossy(line_bytes).into_owned());
        }
        *line_cursor = abs_end + 1;
    }
    None
}

/// Best-effort check of whether the child has stopped running. Returns the
/// exit code (when available) if yes, None if still running.
fn check_child_exited(id: &str) -> Option<ChildExit> {
    if !pid_path(id).exists() {
        return Some(ChildExit { code: None });
    }
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
        Err(_) => None,
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

fn emit_timeout(elapsed: Duration, hint: Option<&str>, json_mode: bool) -> ExitCode {
    if json_mode {
        let mut payload = json!({
            "matched": false,
            "reason": "timeout",
            "elapsed_ms": elapsed.as_millis() as u64,
        });
        if let Some(h) = hint {
            payload["hint"] = json!(h);
        }
        println!("{}", payload);
    } else {
        eprintln!(
            "agent-term: wait: timed out after {} ms",
            elapsed.as_millis()
        );
        if let Some(h) = hint {
            eprintln!("agent-term: wait: hint: {h}");
        }
    }
    ExitCode::from(1)
}

fn emit_child_exited(
    info: ChildExit,
    elapsed: Duration,
    hint: Option<&str>,
    json_mode: bool,
) -> ExitCode {
    if json_mode {
        let mut payload = json!({
            "matched": false,
            "reason": "process_exited",
            "code": info.code,
            "elapsed_ms": elapsed.as_millis() as u64,
        });
        if let Some(h) = hint {
            payload["hint"] = json!(h);
        }
        println!("{}", payload);
    } else {
        match info.code {
            Some(c) => eprintln!(
                "agent-term: wait: process exited (code {c}) before pattern matched"
            ),
            None => eprintln!(
                "agent-term: wait: process exited before pattern matched"
            ),
        }
        if let Some(h) = hint {
            eprintln!("agent-term: wait: hint: {h}");
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
    fn case_insensitive_builder_matches_mixed_case() {
        let re = RegexBuilder::new("ready")
            .multi_line(false)
            .case_insensitive(true)
            .build()
            .unwrap();
        let mut cursor = 0;
        let buf = b"booting\r\nREADY\r\n".to_vec();
        let m = find_match(&re, &buf, false, false, &mut cursor).unwrap();
        assert_eq!(m, "READY");
    }

    #[test]
    fn line_mode_finds_complete_line() {
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut cursor = 0;
        let buf = b"loading\r\nREADY\r\n".to_vec();
        let m = find_match(&re, &buf, false, false, &mut cursor).unwrap();
        assert_eq!(m, "READY");
    }

    #[test]
    fn line_mode_skips_incomplete_trailing_line() {
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut cursor = 0;
        let buf = b"booting\r\nREADY".to_vec();
        assert!(find_match(&re, &buf, false, false, &mut cursor).is_none());
    }

    #[test]
    fn line_mode_cursor_advances_past_checked_lines() {
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut cursor = 0;
        let mut buf = b"a\nb\n".to_vec();
        assert!(find_match(&re, &buf, false, false, &mut cursor).is_none());
        assert_eq!(cursor, buf.len());

        buf.extend_from_slice(b"READY\n");
        let m = find_match(&re, &buf, false, false, &mut cursor).unwrap();
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
        let m = find_match(&re, &buf, true, false, &mut cursor).unwrap();
        assert_eq!(m, "start\nmiddle\nend");
    }

    #[test]
    fn multiline_anchors_use_line_boundaries() {
        let re = RegexBuilder::new("^READY$").multi_line(true).build().unwrap();
        let mut cursor = 0;
        let buf = b"chunk1 boot\nREADY\nchunk3 done\n".to_vec();
        let m = find_match(&re, &buf, true, false, &mut cursor).unwrap();
        assert_eq!(m, "READY");
    }

    #[test]
    fn ts_prefix_stripped_for_match_when_match_full_line_false() {
        // pattern targets body — should match because the prefix is stripped.
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut cursor = 0;
        let buf = b"[1700000000000] READY\n".to_vec();
        let m = find_match(&re, &buf, false, false, &mut cursor).unwrap();
        // Returned line keeps the prefix so callers can see arrival time.
        assert_eq!(m, "[1700000000000] READY");
    }

    #[test]
    fn ts_prefix_kept_when_match_full_line_true() {
        // With match_full_line, `^READY$` no longer matches the prefixed line.
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut cursor = 0;
        let buf = b"[1700000000000] READY\n".to_vec();
        assert!(find_match(&re, &buf, false, true, &mut cursor).is_none());

        // ...but a pattern designed for the prefixed line matches.
        let re_full = RegexBuilder::new(r"^\[\d+\] READY$")
            .multi_line(false)
            .build()
            .unwrap();
        let mut cursor2 = 0;
        assert!(find_match(&re_full, &buf, false, true, &mut cursor2).is_some());
    }

    #[test]
    fn line_without_ts_prefix_matches_in_either_mode() {
        let re = RegexBuilder::new("^READY$").multi_line(false).build().unwrap();
        let mut c1 = 0;
        let mut c2 = 0;
        let buf = b"READY\n".to_vec();
        assert!(find_match(&re, &buf, false, false, &mut c1).is_some());
        assert!(find_match(&re, &buf, false, true, &mut c2).is_some());
    }

    #[test]
    fn raw_tail_keeps_recent_bytes() {
        let mut tail = Vec::new();
        let mut seen = false;
        let feed = |bytes: &[u8], tail: &mut Vec<u8>, seen: &mut bool| {
            if !*seen && bytes.contains(&0x1b) {
                *seen = true;
            }
            push_raw_tail(tail, bytes);
        };
        feed(b"hello", &mut tail, &mut seen);
        assert_eq!(tail, b"hello");
        assert!(!seen);

        feed(b"\x1b[31mred", &mut tail, &mut seen);
        assert!(seen);
        assert!(tail.ends_with(b"red"));

        // Push past capacity: only the trailing RAW_TAIL_CAP bytes are kept.
        let big = vec![b'x'; RAW_TAIL_CAP + 100];
        feed(&big, &mut tail, &mut seen);
        assert_eq!(tail.len(), RAW_TAIL_CAP);
        assert!(tail.iter().all(|&b| b == b'x'));
    }

    #[test]
    fn stripper_removes_ansi_before_match() {
        // Simulates Vite's `ready in [0m[1m3688[0m ms` — naive regex on raw
        // bytes can't find `ready in [0-9]+`. With AnsiStripper feeding the
        // accumulator, the regex sees clean text.
        let mut stripper = AnsiStripper::new();
        let mut accum = Vec::new();
        stripper.feed(b"  VITE v5  ready in \x1b[1m3688\x1b[22m ms\n", &mut accum);

        let re = RegexBuilder::new(r"ready in \d+ ms")
            .multi_line(false)
            .build()
            .unwrap();
        let mut cursor = 0;
        let m = find_match(&re, &accum, false, false, &mut cursor).unwrap();
        assert!(m.contains("ready in 3688 ms"));
    }
}

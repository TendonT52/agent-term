use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

use regex::RegexBuilder;
use serde_json::json;

use crate::ids::is_valid_id;
use crate::pty::{parse_ms_prefix, AnsiStripper};
use crate::state::log_path;
use crate::tail::{now_ms, parse_time_spec};

pub struct GrepOptions {
    pub id: String,
    pub pattern: Option<String>,
    pub pattern_file: Option<String>,
    pub around: usize,
    pub limit: Option<u64>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub strip_ansi: bool,
    /// Match against the post-prefix body (default) or against the full line
    /// including the `[ms]` prefix.
    pub match_full_line: bool,
    pub json: bool,
    pub multiline: bool,
    pub ignore_case: bool,
}

#[derive(Debug)]
struct Hit {
    line_idx: usize,
    timestamp_ms: Option<u64>,
}

pub fn run(opts: GrepOptions) -> ExitCode {
    let GrepOptions {
        id,
        pattern,
        pattern_file,
        around,
        limit,
        since,
        until,
        strip_ansi,
        match_full_line,
        json,
        multiline,
        ignore_case,
    } = opts;

    if !is_valid_id(&id) {
        eprintln!("agent-term: grep: invalid id {id:?}");
        return ExitCode::from(1);
    }

    let pattern_text = match resolve_pattern(pattern, pattern_file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agent-term: grep: {e}");
            return ExitCode::from(1);
        }
    };

    let regex = match RegexBuilder::new(&pattern_text)
        .multi_line(multiline)
        .case_insensitive(ignore_case)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agent-term: grep: bad regex: {e}");
            return ExitCode::from(1);
        }
    };

    let now = now_ms();
    let since_ms = match since.as_deref().map(|s| parse_time_spec(s, now)) {
        Some(Ok(t)) => Some(t),
        Some(Err(e)) => {
            eprintln!("agent-term: grep: --since: {e}");
            return ExitCode::from(1);
        }
        None => None,
    };
    let until_ms = match until.as_deref().map(|s| parse_time_spec(s, now)) {
        Some(Ok(t)) => Some(t),
        Some(Err(e)) => {
            eprintln!("agent-term: grep: --until: {e}");
            return ExitCode::from(1);
        }
        None => None,
    };

    let log = log_path(&id);
    if !log.exists() {
        eprintln!("agent-term: grep: no log for id {id:?}");
        return ExitCode::from(1);
    }
    let mut file = match fs::File::open(&log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("agent-term: grep: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        eprintln!("agent-term: grep: {e}");
        return ExitCode::from(1);
    }
    let mut all = Vec::new();
    if let Err(e) = file.read_to_end(&mut all) {
        eprintln!("agent-term: grep: {e}");
        return ExitCode::from(1);
    }

    let lines = split_lines(&all);
    let hits = collect_hits(&lines, &regex, since_ms, until_ms, match_full_line, limit);

    if json {
        emit_json(&lines, &hits, around);
    } else {
        emit_plain(&lines, &hits, around, strip_ansi);
    }
    ExitCode::SUCCESS
}

fn resolve_pattern(
    pattern: Option<String>,
    pattern_file: Option<String>,
) -> Result<String, String> {
    match (pattern, pattern_file) {
        (Some(p), None) => Ok(p),
        (None, Some(path)) => fs::read_to_string(&path)
            .map(|s| s.trim_end_matches('\n').to_string())
            .map_err(|e| format!("read --pattern-file {path}: {e}")),
        (Some(_), Some(_)) => {
            Err("--pattern and --pattern-file are mutually exclusive".into())
        }
        (None, None) => Err("--pattern or --pattern-file required".into()),
    }
}

/// Splits raw log bytes into line slices, each ending at `\n` if one is
/// present. The last line may have no trailing `\n` (unterminated).
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

fn collect_hits(
    lines: &[&[u8]],
    regex: &regex::Regex,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
    match_full_line: bool,
    limit: Option<u64>,
) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let (ts, body_offset) = parse_ms_prefix(line)
            .map(|(t, o)| (Some(t), o))
            .unwrap_or((None, 0));
        if let Some(s) = since_ms {
            match ts {
                Some(t) if t < s => continue,
                None => continue, // strict: drop untimestamped lines under time filter
                _ => {}
            }
        }
        if let Some(u) = until_ms {
            match ts {
                Some(t) if t > u => break, // monotonic
                None => continue,
                _ => {}
            }
        }
        let haystack: &[u8] = if match_full_line {
            line
        } else {
            &line[body_offset..]
        };
        // strip trailing newline / cr so regex `$` and `^` behave naturally
        let haystack = haystack
            .strip_suffix(b"\n")
            .unwrap_or(haystack)
            .strip_suffix(b"\r")
            .unwrap_or_else(|| {
                haystack.strip_suffix(b"\n").unwrap_or(haystack)
            });
        let s = String::from_utf8_lossy(haystack);
        if regex.is_match(&s) {
            hits.push(Hit {
                line_idx: idx,
                timestamp_ms: ts,
            });
            if let Some(l) = limit {
                if hits.len() as u64 >= l {
                    break;
                }
            }
        }
    }
    hits
}

/// Emit each hit as a contiguous block of `around` lines before, the match
/// line, and `around` lines after. Adjacent blocks coalesce; separated blocks
/// are joined by `--\n` (the grep -A/-B convention).
fn emit_plain(lines: &[&[u8]], hits: &[Hit], around: usize, strip_ansi: bool) {
    let mut stdout = io::stdout().lock();
    let blocks = coalesce_windows(hits, lines.len(), around);
    let mut stripper = strip_ansi.then(AnsiStripper::new);
    for (i, (start, end)) in blocks.iter().enumerate() {
        if i > 0 {
            let _ = stdout.write_all(b"--\n");
        }
        for line in &lines[*start..*end] {
            match stripper.as_mut() {
                Some(s) => {
                    let mut clean = Vec::with_capacity(line.len());
                    s.feed(line, &mut clean);
                    let _ = stdout.write_all(&clean);
                }
                None => {
                    let _ = stdout.write_all(line);
                }
            }
        }
    }
    let _ = stdout.flush();
}

fn emit_json(lines: &[&[u8]], hits: &[Hit], around: usize) {
    let mut blocks_out = Vec::new();
    let blocks = coalesce_windows(hits, lines.len(), around);
    let hit_set: BTreeSet<usize> = hits.iter().map(|h| h.line_idx).collect();
    for (start, end) in &blocks {
        let mut block_lines = Vec::with_capacity(end - start);
        let mut block_hits = Vec::new();
        for (idx, line_bytes) in lines[*start..*end].iter().enumerate() {
            let abs_idx = *start + idx;
            let line = String::from_utf8_lossy(line_bytes);
            block_lines.push(json!({
                "line_no": abs_idx + 1,
                "is_match": hit_set.contains(&abs_idx),
                "content": line,
            }));
            if hit_set.contains(&abs_idx) {
                let ts = hits
                    .iter()
                    .find(|h| h.line_idx == abs_idx)
                    .and_then(|h| h.timestamp_ms);
                block_hits.push(json!({
                    "line_no": abs_idx + 1,
                    "timestamp_ms": ts,
                }));
            }
        }
        blocks_out.push(json!({
            "start_line_no": start + 1,
            "end_line_no": end, // half-open, matches python convention
            "matches": block_hits,
            "lines": block_lines,
        }));
    }
    let body = json!({
        "hits": hits.len(),
        "blocks": blocks_out,
    });
    println!("{}", body);
}

/// Given a list of hit indices, compute coalesced `[start, end)` windows of
/// size `2*around + 1` around each hit. Overlapping or adjacent windows
/// merge. Clamped to `[0, total_lines)`.
fn coalesce_windows(hits: &[Hit], total_lines: usize, around: usize) -> Vec<(usize, usize)> {
    let mut windows: Vec<(usize, usize)> = hits
        .iter()
        .map(|h| {
            let start = h.line_idx.saturating_sub(around);
            let end = (h.line_idx + around + 1).min(total_lines);
            (start, end)
        })
        .collect();
    windows.sort_by_key(|w| w.0);

    let mut merged = Vec::with_capacity(windows.len());
    for w in windows {
        match merged.last_mut() {
            Some((_, last_end)) if w.0 <= *last_end => {
                if w.1 > *last_end {
                    *last_end = w.1;
                }
            }
            _ => merged.push(w),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(s: &str) -> Vec<&[u8]> {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut start = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                out.push(&bytes[start..=i]);
                start = i + 1;
            }
        }
        if start < bytes.len() {
            out.push(&bytes[start..]);
        }
        out
    }

    #[test]
    fn split_lines_preserves_terminators() {
        let v = split_lines(b"a\nb\nc");
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], b"a\n");
        assert_eq!(v[1], b"b\n");
        assert_eq!(v[2], b"c");
    }

    #[test]
    fn collect_hits_finds_lines() {
        let body = "info ok\nERROR boom\ninfo ok\nERROR again\n";
        let lines = lines_of(body);
        let re = RegexBuilder::new("^ERROR").build().unwrap();
        let hits = collect_hits(&lines, &re, None, None, false, None);
        assert_eq!(hits.iter().map(|h| h.line_idx).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn case_insensitive_builder_matches_mixed_case() {
        let body = "info ok\nError boom\nWARNING low\nerror again\n";
        let lines = lines_of(body);
        let re = RegexBuilder::new("error")
            .case_insensitive(true)
            .build()
            .unwrap();
        let hits = collect_hits(&lines, &re, None, None, false, None);
        assert_eq!(hits.iter().map(|h| h.line_idx).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn collect_hits_respects_limit() {
        let body = "a\nERROR\nb\nERROR\nc\nERROR\n";
        let lines = lines_of(body);
        let re = RegexBuilder::new("^ERROR").build().unwrap();
        let hits = collect_hits(&lines, &re, None, None, false, Some(2));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn collect_hits_filters_by_since() {
        let body = "[10] a\n[20] ERROR\n[30] b\n[40] ERROR\n";
        let lines = lines_of(body);
        let re = RegexBuilder::new("^ERROR").build().unwrap();
        // since=25 drops the first ERROR (ts=20), keeps the second (ts=40)
        let hits = collect_hits(&lines, &re, Some(25), None, false, None);
        assert_eq!(hits.iter().map(|h| h.line_idx).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn collect_hits_strict_drops_untimestamped_when_since_set() {
        let body = "raw no ts\nERROR raw\n[100] ERROR ts\n";
        let lines = lines_of(body);
        let re = RegexBuilder::new("ERROR").build().unwrap();
        let hits = collect_hits(&lines, &re, Some(50), None, false, None);
        assert_eq!(hits.iter().map(|h| h.line_idx).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn coalesce_adjacent_windows() {
        // hits at 5 and 7 with around=2 → windows [3,8) and [5,10) → merged [3,10)
        let hits = vec![
            Hit { line_idx: 5, timestamp_ms: None },
            Hit { line_idx: 7, timestamp_ms: None },
        ];
        let merged = coalesce_windows(&hits, 100, 2);
        assert_eq!(merged, vec![(3, 10)]);
    }

    #[test]
    fn coalesce_separated_windows() {
        // hits at 0 and 50 with around=1 → [0,2) and [49,52) — disjoint
        let hits = vec![
            Hit { line_idx: 0, timestamp_ms: None },
            Hit { line_idx: 50, timestamp_ms: None },
        ];
        let merged = coalesce_windows(&hits, 100, 1);
        assert_eq!(merged, vec![(0, 2), (49, 52)]);
    }

    #[test]
    fn coalesce_clamps_to_bounds() {
        // hit at 0, around=5 — start saturates to 0
        let hits = vec![Hit { line_idx: 0, timestamp_ms: None }];
        let merged = coalesce_windows(&hits, 3, 5);
        assert_eq!(merged, vec![(0, 3)]);
    }
}

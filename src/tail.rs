use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::ids::is_valid_id;
use crate::pty::{parse_ms_prefix, AnsiStripper};
use crate::state::{is_pid_alive, log_path, pid_path};

const POLL: Duration = Duration::from_millis(100);
const READ_CHUNK: usize = 8192;

/// CLI options forwarded by the subcommand parser. Mutual-exclusion is enforced
/// here, not in clap, because clap's `conflicts_with` doesn't compose well with
/// `Option<u64>` defaults.
pub struct TailOptions {
    pub id: String,
    pub follow: bool,
    pub strip_ansi: bool,
    pub lines: Option<u64>,
    pub bytes: Option<String>,
    pub head: Option<u64>,
    pub reverse: bool,
    pub cursor: Option<u64>,
    pub json: bool,
    /// `--since` time spec: `30s`, `5m`, absolute ms-since-epoch, or `now`.
    pub since: Option<String>,
    pub until: Option<String>,
    /// Keep the `[<ms>] ` prefix in emitted lines (default strips it).
    pub keep_timestamps: bool,
}

pub fn run(opts: TailOptions) -> ExitCode {
    let TailOptions {
        id,
        follow,
        strip_ansi,
        lines,
        bytes,
        head,
        reverse,
        cursor,
        json,
        since,
        until,
        keep_timestamps,
    } = opts;

    if !is_valid_id(&id) {
        eprintln!("agent-term: invalid id {id:?}");
        return ExitCode::from(1);
    }

    if let Err(msg) = validate_flag_combinations(
        lines,
        bytes.is_some(),
        head,
        reverse,
        follow,
        cursor,
        json,
        since.is_some() || until.is_some(),
    ) {
        eprintln!("agent-term: tail: {msg}");
        return ExitCode::from(1);
    }

    // Time-window filtering (--since/--until). Implemented as a separate path
    // because it filters line-by-line and ignores other bounded-read flags.
    let now_ms = now_ms();
    let since_ms = match since.as_deref().map(|s| parse_time_spec(s, now_ms)) {
        Some(Ok(t)) => Some(t),
        Some(Err(e)) => {
            eprintln!("agent-term: tail: --since: {e}");
            return ExitCode::from(1);
        }
        None => None,
    };
    let until_ms = match until.as_deref().map(|s| parse_time_spec(s, now_ms)) {
        Some(Ok(t)) => Some(t),
        Some(Err(e)) => {
            eprintln!("agent-term: tail: --until: {e}");
            return ExitCode::from(1);
        }
        None => None,
    };

    let bytes_n = match bytes.as_deref().map(parse_bytes_suffix) {
        Some(Ok(n)) => Some(n),
        Some(Err(e)) => {
            eprintln!("agent-term: tail: --bytes: {e}");
            return ExitCode::from(1);
        }
        None => None,
    };

    let log = log_path(&id);
    if !log.exists() {
        eprintln!("agent-term: no log for id {id:?}");
        return ExitCode::from(1);
    }

    let mut stripper = strip_ansi.then(AnsiStripper::new);
    let mut stdout = io::stdout().lock();

    let mut file = match fs::File::open(&log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
    };

    // ---- Time-window filter path (--since/--until) ----
    if since_ms.is_some() || until_ms.is_some() {
        return emit_time_window(
            &mut file,
            since_ms,
            until_ms,
            stripper.as_mut(),
            keep_timestamps,
            json,
            &mut stdout,
        );
    }

    // ---- JSON mode ----
    // When --json is on, every read returns a single envelope object. The
    // cursor field is the position the agent should pass back next time.
    if json {
        return emit_json(&mut file, cursor, lines, bytes_n, head, reverse, stripper.as_mut());
    }

    // ---- Cursor-based plain-text read ----
    // Mutually exclusive with the bounded flags (validate above); on stale
    // cursor we emit nothing to stdout and a notice on stderr.
    if let Some(pos) = cursor {
        let len = file.seek(SeekFrom::End(0)).unwrap_or(0);
        if pos > len {
            eprintln!(
                "agent-term: tail: cursor {pos} past EOF ({len}); log was rotated or truncated"
            );
            return ExitCode::from(1);
        }
        if let Err(e) = file.seek(SeekFrom::Start(pos)) {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
        let res = drain_to_stdout(&mut file, stripper.as_mut(), &mut stdout);
        if let Err(e) = res {
            if e.kind() != io::ErrorKind::BrokenPipe {
                eprintln!("agent-term: tail: {e}");
                return ExitCode::from(1);
            }
        }
        if !follow {
            return ExitCode::SUCCESS;
        }
        return follow_loop(&id, &log, &mut file, stripper.as_mut(), &mut stdout);
    }

    // ---- Bounded initial dump ----
    let initial_result: io::Result<()> = if let Some(n) = head {
        emit_head_n_lines(&mut file, n, stripper.as_mut(), &mut stdout)
    } else if reverse {
        emit_reverse_bounded(
            &mut file,
            lines,
            bytes_n,
            stripper.as_mut(),
            &mut stdout,
        )
    } else if let Some(n) = lines {
        match seek_to_last_n_lines(&mut file, n) {
            Ok(()) => drain_to_stdout(&mut file, stripper.as_mut(), &mut stdout),
            Err(e) => Err(e),
        }
    } else if let Some(n) = bytes_n {
        match seek_to_last_n_bytes(&mut file, n) {
            Ok(()) => drain_to_stdout(&mut file, stripper.as_mut(), &mut stdout),
            Err(e) => Err(e),
        }
    } else {
        drain_to_stdout(&mut file, stripper.as_mut(), &mut stdout)
    };

    if let Err(e) = initial_result {
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    if !follow {
        return ExitCode::SUCCESS;
    }

    follow_loop(&id, &log, &mut file, stripper.as_mut(), &mut stdout)
}

#[allow(clippy::too_many_arguments)]
fn validate_flag_combinations(
    lines: Option<u64>,
    has_bytes: bool,
    head: Option<u64>,
    reverse: bool,
    follow: bool,
    cursor: Option<u64>,
    json: bool,
    has_time_window: bool,
) -> Result<(), String> {
    if head.is_some() && (lines.is_some() || has_bytes || reverse || follow) {
        return Err(
            "--head is mutually exclusive with --lines/--bytes/--reverse/--follow".into(),
        );
    }
    if reverse && follow {
        return Err("--reverse and --follow can't combine".into());
    }
    if reverse && lines.is_none() && !has_bytes {
        return Err("--reverse requires --lines N or --bytes N (so the slice is bounded)".into());
    }
    if lines.is_some() && has_bytes {
        return Err("--lines and --bytes are mutually exclusive".into());
    }
    if cursor.is_some() && (lines.is_some() || has_bytes || head.is_some() || reverse) {
        return Err(
            "--cursor / --since-cursor is mutually exclusive with --lines/--bytes/--head/--reverse"
                .into(),
        );
    }
    if json && follow {
        return Err("--json and --follow can't combine (use repeated --cursor reads instead)".into());
    }
    if has_time_window
        && (lines.is_some() || has_bytes || head.is_some() || reverse || cursor.is_some() || follow)
    {
        return Err(
            "--since / --until is mutually exclusive with --lines/--bytes/--head/--reverse/--cursor/--follow"
                .into(),
        );
    }
    Ok(())
}

// ----------------------- bounded-read helpers -----------------------

/// Seeks `file` so that subsequent reads cover the last `n` lines of the log.
/// A trailing `\n` does not count as a line separator (otherwise asking for
/// the last 1 line of `a\nb\n` would give just an empty line).
pub fn seek_to_last_n_lines(file: &mut fs::File, n: u64) -> io::Result<()> {
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(());
    }
    if n == 0 {
        file.seek(SeekFrom::Start(len))?;
        return Ok(());
    }

    // Probe the last byte to decide whether a trailing \n exists. If it does,
    // we want to ignore it when counting line separators.
    let mut last_byte = [0u8; 1];
    file.seek(SeekFrom::Start(len - 1))?;
    file.read_exact(&mut last_byte)?;
    let trailing_nl = last_byte[0] == b'\n';

    let chunk = 4096usize;
    let mut buf = vec![0u8; chunk];
    let mut pos = len;
    let mut newlines = 0u64;
    let mut found_start: Option<u64> = None;

    'outer: while pos > 0 {
        let read = (chunk as u64).min(pos) as usize;
        let start = pos - read as u64;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf[..read])?;

        for i in (0..read).rev() {
            let abs = start + i as u64;
            if buf[i] != b'\n' {
                continue;
            }
            if trailing_nl && abs == len - 1 {
                continue;
            }
            newlines += 1;
            if newlines == n {
                found_start = Some(abs + 1);
                break 'outer;
            }
        }
        pos = start;
    }

    let target = found_start.unwrap_or(0);
    file.seek(SeekFrom::Start(target))?;
    Ok(())
}

/// Seeks `file` so that subsequent reads cover roughly the last `n` bytes,
/// aligned forward to the next newline so the first emitted line is whole.
/// If `raw_start` already lands on a line boundary (start of file, or byte
/// after a `\n`) no advance happens — we keep the byte exactly. If `n >=
/// file_len` the seek is to byte 0.
pub fn seek_to_last_n_bytes(file: &mut fs::File, n: u64) -> io::Result<()> {
    let len = file.seek(SeekFrom::End(0))?;
    if len <= n {
        file.seek(SeekFrom::Start(0))?;
        return Ok(());
    }
    let raw_start = len - n;
    if raw_start == 0 {
        file.seek(SeekFrom::Start(0))?;
        return Ok(());
    }
    // Already at a line boundary (previous byte is \n)? Keep it.
    let mut prev = [0u8; 1];
    file.seek(SeekFrom::Start(raw_start - 1))?;
    file.read_exact(&mut prev)?;
    if prev[0] == b'\n' {
        file.seek(SeekFrom::Start(raw_start))?;
        return Ok(());
    }

    // Otherwise advance forward to the next \n.
    file.seek(SeekFrom::Start(raw_start))?;
    let mut probe = [0u8; 4096];
    let mut cursor = raw_start;
    loop {
        let r = file.read(&mut probe)?;
        if r == 0 {
            file.seek(SeekFrom::Start(raw_start))?;
            return Ok(());
        }
        if let Some(idx) = probe[..r].iter().position(|&b| b == b'\n') {
            cursor += idx as u64 + 1;
            file.seek(SeekFrom::Start(cursor))?;
            return Ok(());
        }
        cursor += r as u64;
    }
}

/// Emit the first `n` lines from the start of `file`. Stripping is applied
/// per-chunk via the same shared state machine the unbounded path uses.
fn emit_head_n_lines(
    file: &mut fs::File,
    n: u64,
    mut stripper: Option<&mut AnsiStripper>,
    out: &mut impl Write,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    if n == 0 {
        return Ok(());
    }

    let mut buf = vec![0u8; READ_CHUNK];
    let mut clean = Vec::with_capacity(READ_CHUNK);
    let mut emitted = 0u64;

    loop {
        let r = file.read(&mut buf)?;
        if r == 0 {
            break;
        }
        // Find the byte index where we've counted n newlines (inclusive).
        let mut chunk_end = r;
        let mut nl_in_this_chunk = 0u64;
        for (i, &b) in buf[..r].iter().enumerate() {
            if b == b'\n' {
                nl_in_this_chunk += 1;
                if emitted + nl_in_this_chunk == n {
                    chunk_end = i + 1;
                    break;
                }
            }
        }
        let slice = &buf[..chunk_end];
        match stripper.as_deref_mut() {
            Some(s) => {
                clean.clear();
                s.feed(slice, &mut clean);
                out.write_all(&clean)?;
            }
            None => out.write_all(slice)?,
        }
        emitted += nl_in_this_chunk;
        if emitted >= n {
            break;
        }
    }
    out.flush().ok();
    Ok(())
}

/// Emit the bounded slice in reverse order, line by line. Requires either
/// `lines` or `bytes_n` to bound the in-memory buffer.
fn emit_reverse_bounded(
    file: &mut fs::File,
    lines: Option<u64>,
    bytes_n: Option<u64>,
    stripper: Option<&mut AnsiStripper>,
    out: &mut impl Write,
) -> io::Result<()> {
    if let Some(n) = lines {
        seek_to_last_n_lines(file, n)?;
    } else if let Some(n) = bytes_n {
        seek_to_last_n_bytes(file, n)?;
    }

    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    // Strip ANSI on the whole buffer first (state machine works across the
    // entire slice — no chunk boundaries to worry about here).
    let bytes: Vec<u8> = match stripper {
        Some(s) => {
            let mut clean = Vec::with_capacity(buf.len());
            s.feed(&buf, &mut clean);
            clean
        }
        None => buf,
    };

    let reversed = reverse_lines(&bytes);
    out.write_all(&reversed)?;
    out.flush().ok();
    Ok(())
}

/// Parses an integer with optional suffix: `100`, `16K`, `2M`, `1G`. Both
/// `K`/`Ki` are accepted and mean 1024 (binary units everywhere — it's a
/// byte count, not a marketing figure).
pub fn parse_bytes_suffix(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty value".into());
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    let (n_str, unit_raw) = s.split_at(split);
    if n_str.is_empty() {
        return Err(format!("missing number in {s:?}"));
    }
    let n: u64 = n_str
        .parse()
        .map_err(|_| format!("invalid integer in {s:?}"))?;
    let unit = unit_raw.trim().to_ascii_uppercase();
    let unit = unit.strip_suffix('I').unwrap_or(&unit).to_string();
    let mult: u64 = match unit.as_str() {
        "" | "B" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => return Err(format!("unknown unit {unit_raw:?}")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("byte count overflow: {s}"))
}

// ----------------------- time-spec parsing -----------------------

/// Returns the current wall-clock time in ms since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parses a `--since` / `--until` value into an absolute ms-since-epoch.
/// Supported forms:
///   - `now`                              → exactly `now_ms`
///   - `Ns`, `Nm`, `Nh`, `Nd`             → relative ("ago"); result = now - N units
///   - `N ago` (any of the above plus " ago") → relative; the "ago" is decorative
///   - integer with no suffix              → absolute ms since epoch
pub fn parse_time_spec(input: &str, now_ms: u64) -> Result<u64, String> {
    let s = input.trim().trim_end_matches(" ago").trim();
    if s.is_empty() {
        return Err("empty time spec".into());
    }
    if s.eq_ignore_ascii_case("now") {
        return Ok(now_ms);
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n_str, unit) = s.split_at(split);
    if n_str.is_empty() {
        return Err(format!("invalid time spec {input:?}"));
    }
    let n: u64 = n_str
        .parse()
        .map_err(|_| format!("invalid integer in time spec {input:?}"))?;
    let unit = unit.trim();
    let ms_per_unit: u64 = match unit {
        "" => return Ok(n),
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        other => return Err(format!("unknown time unit {other:?}")),
    };
    let delta = n.saturating_mul(ms_per_unit);
    Ok(now_ms.saturating_sub(delta))
}

// ----------------------- time-window emitter -----------------------

/// Reads `file` line-by-line, parses the `[ms] ` prefix off each line, and
/// emits the lines whose timestamp falls in `[since, until]`. Lines without
/// a parseable prefix are dropped silently (strict mode — keeps output
/// trustworthy when the user expects time semantics).
///
/// When `keep_timestamps` is true the prefix is preserved in the output; the
/// default strips it for clean LLM consumption.
fn emit_time_window(
    file: &mut fs::File,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
    stripper: Option<&mut AnsiStripper>,
    keep_timestamps: bool,
    json_mode: bool,
    out: &mut impl Write,
) -> ExitCode {
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        eprintln!("agent-term: tail: {e}");
        return ExitCode::from(1);
    }

    let mut emitted = Vec::<u8>::new();
    let mut lines_emitted: u64 = 0;
    let mut buf = Vec::<u8>::new();
    if let Err(e) = file.read_to_end(&mut buf) {
        eprintln!("agent-term: tail: {e}");
        return ExitCode::from(1);
    }
    let mut saw_timestamped_line = false;
    let mut start = 0usize;
    for i in 0..buf.len() {
        if buf[i] != b'\n' {
            continue;
        }
        let line = &buf[start..=i];
        start = i + 1;

        let Some((ts, body_start)) = parse_ms_prefix(line) else {
            continue;
        };
        saw_timestamped_line = true;
        if let Some(s) = since_ms {
            if ts < s {
                continue;
            }
        }
        if let Some(u) = until_ms {
            if ts > u {
                // Lines are time-ordered, so we can stop early.
                break;
            }
        }
        let body = if keep_timestamps {
            line
        } else {
            &line[body_start..]
        };
        emitted.extend_from_slice(body);
        lines_emitted += 1;
    }

    if !saw_timestamped_line {
        eprintln!(
            "agent-term: tail: --since/--until require timestamped logs; \
             spawn with --timestamps (or AGENT_TERM_TIMESTAMPS=1)"
        );
        return ExitCode::from(1);
    }

    // ANSI strip if requested. We apply to the assembled output so the stripper
    // state machine doesn't have to span chunk boundaries.
    let final_bytes = match stripper {
        Some(s) => {
            let mut clean = Vec::with_capacity(emitted.len());
            s.feed(&emitted, &mut clean);
            clean
        }
        None => emitted,
    };

    if json_mode {
        let body = json!({
            "lines_emitted": lines_emitted,
            "bytes_emitted": final_bytes.len(),
            "since_ms": since_ms,
            "until_ms": until_ms,
            "content": String::from_utf8_lossy(&final_bytes),
        });
        println!("{}", body);
    } else if let Err(e) = out.write_all(&final_bytes) {
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}

// ----------------------- JSON envelope emitter -----------------------

/// Produces a `{cursor_start, cursor, content, lines_emitted, bytes_emitted}`
/// JSON object on stdout. With `cursor` past EOF the envelope is empty but
/// the call still succeeds — the field `cursor_stale: true` lets the agent
/// recover by resetting to `cursor` (which is current EOF).
fn emit_json(
    file: &mut fs::File,
    cursor: Option<u64>,
    lines: Option<u64>,
    bytes_n: Option<u64>,
    head: Option<u64>,
    reverse: bool,
    stripper: Option<&mut AnsiStripper>,
) -> ExitCode {
    let len = match file.seek(SeekFrom::End(0)) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
    };

    // 1. Stale-cursor short-circuit.
    if let Some(pos) = cursor {
        if pos > len {
            let body = json!({
                "cursor_start": pos,
                "cursor": len,
                "cursor_stale": true,
                "stale_reason": "cursor past EOF (rotation or truncation)",
                "lines_emitted": 0,
                "bytes_emitted": 0,
                "content": "",
            });
            println!("{}", body);
            return ExitCode::SUCCESS;
        }
    }

    // 2. Compute the byte slice to read.
    let cursor_start = if let Some(pos) = cursor {
        if let Err(e) = file.seek(SeekFrom::Start(pos)) {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
        pos
    } else if let Some(n) = head {
        if let Err(e) = file.seek(SeekFrom::Start(0)) {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
        // For head, we'll cap by line count during read, so cursor_start = 0.
        let _ = n;
        0
    } else if reverse {
        // Bounded slice computed below; cursor_start mirrors the slice start.
        match (lines, bytes_n) {
            (Some(n), _) => {
                if let Err(e) = seek_to_last_n_lines(file, n) {
                    eprintln!("agent-term: tail: {e}");
                    return ExitCode::from(1);
                }
                file.stream_position().unwrap_or(0)
            }
            (_, Some(n)) => {
                if let Err(e) = seek_to_last_n_bytes(file, n) {
                    eprintln!("agent-term: tail: {e}");
                    return ExitCode::from(1);
                }
                file.stream_position().unwrap_or(0)
            }
            _ => 0,
        }
    } else if let Some(n) = lines {
        if let Err(e) = seek_to_last_n_lines(file, n) {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
        file.stream_position().unwrap_or(0)
    } else if let Some(n) = bytes_n {
        if let Err(e) = seek_to_last_n_bytes(file, n) {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
        file.stream_position().unwrap_or(0)
    } else {
        if let Err(e) = file.seek(SeekFrom::Start(0)) {
            eprintln!("agent-term: tail: {e}");
            return ExitCode::from(1);
        }
        0
    };

    // 3. Read the slice.
    let mut buf = Vec::new();
    if let Some(n) = head {
        // Head: read forward and stop after N newlines.
        let mut tmp = vec![0u8; READ_CHUNK];
        let mut emitted_nl = 0u64;
        'outer: loop {
            let r = match file.read(&mut tmp) {
                Ok(0) => break,
                Ok(r) => r,
                Err(e) => {
                    eprintln!("agent-term: tail: {e}");
                    return ExitCode::from(1);
                }
            };
            for (i, &b) in tmp[..r].iter().enumerate() {
                if b == b'\n' {
                    emitted_nl += 1;
                    if emitted_nl == n {
                        buf.extend_from_slice(&tmp[..=i]);
                        break 'outer;
                    }
                }
            }
            buf.extend_from_slice(&tmp[..r]);
            if emitted_nl >= n {
                break;
            }
        }
    } else if let Err(e) = file.read_to_end(&mut buf) {
        eprintln!("agent-term: tail: {e}");
        return ExitCode::from(1);
    }

    // 4. Apply stripper.
    let content_bytes: Vec<u8> = match stripper {
        Some(s) => {
            let mut clean = Vec::with_capacity(buf.len());
            s.feed(&buf, &mut clean);
            clean
        }
        None => buf,
    };

    // 5. Reverse if requested.
    let content_bytes = if reverse {
        reverse_lines(&content_bytes)
    } else {
        content_bytes
    };

    let lines_emitted = content_bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    let bytes_emitted = content_bytes.len() as u64;

    // 6. Cursor: where the next read should pick up. For head/reverse/bounded
    //    reads, "next" semantics aren't meaningful, but we still set it to the
    //    end of the read so an unbounded follow-up returns nothing.
    let cursor_end = if cursor.is_some() {
        len
    } else if head.is_some() {
        cursor_start + bytes_emitted
    } else {
        len
    };

    let body = json!({
        "cursor_start": cursor_start,
        "cursor": cursor_end,
        "lines_emitted": lines_emitted,
        "bytes_emitted": bytes_emitted,
        "content": String::from_utf8_lossy(&content_bytes),
    });
    println!("{}", body);
    ExitCode::SUCCESS
}

/// Reverse the order of lines in `bytes`. If the input has a trailing `\n`,
/// the output keeps it; otherwise the output has no trailing `\n`. Internal
/// line separators are always preserved between adjacent reversed lines.
fn reverse_lines(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let trailing_nl = *bytes.last().unwrap() == b'\n';
    let body = if trailing_nl {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    let mut lines: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
    lines.reverse();

    let mut out = Vec::with_capacity(bytes.len());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(line);
    }
    if trailing_nl {
        out.push(b'\n');
    }
    out
}

// ----------------------- unbounded / follow path -----------------------

fn drain_to_stdout(
    file: &mut fs::File,
    mut stripper: Option<&mut AnsiStripper>,
    out: &mut impl Write,
) -> io::Result<()> {
    let mut buf = [0u8; READ_CHUNK];
    let mut clean = Vec::with_capacity(READ_CHUNK);
    loop {
        let r = file.read(&mut buf)?;
        if r == 0 {
            break;
        }
        match stripper.as_deref_mut() {
            Some(s) => {
                clean.clear();
                s.feed(&buf[..r], &mut clean);
                out.write_all(&clean)?;
            }
            None => out.write_all(&buf[..r])?,
        }
    }
    out.flush().ok();
    Ok(())
}

fn follow_loop(
    id: &str,
    log: &std::path::Path,
    file: &mut fs::File,
    mut stripper: Option<&mut AnsiStripper>,
    out: &mut impl Write,
) -> ExitCode {
    loop {
        thread::sleep(POLL);

        if let Ok(meta) = fs::metadata(log) {
            if let Ok(pos) = file.stream_position() {
                if pos > meta.len() {
                    match fs::File::open(log) {
                        Ok(f) => *file = f,
                        Err(_) => continue,
                    }
                }
            }
        }

        match drain_to_stdout(file, stripper.as_deref_mut(), out) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agent-term: tail: {e}");
                return ExitCode::from(1);
            }
        }

        if !daemon_alive(id) {
            let _ = drain_to_stdout(file, stripper.as_deref_mut(), out);
            return ExitCode::SUCCESS;
        }
    }
}

fn daemon_alive(id: &str) -> bool {
    fs::read_to_string(pid_path(id))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(is_pid_alive)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn write_file(content: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f
    }

    fn dump_from_pos(file: &mut fs::File) -> Vec<u8> {
        let mut v = Vec::new();
        file.read_to_end(&mut v).unwrap();
        v
    }

    // ---------- parse_bytes_suffix ----------

    #[test]
    fn parse_bytes_plain_int() {
        assert_eq!(parse_bytes_suffix("100").unwrap(), 100);
    }

    #[test]
    fn parse_bytes_units() {
        assert_eq!(parse_bytes_suffix("1K").unwrap(), 1024);
        assert_eq!(parse_bytes_suffix("16K").unwrap(), 16 * 1024);
        assert_eq!(parse_bytes_suffix("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_bytes_suffix("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes_suffix("1B").unwrap(), 1);
    }

    #[test]
    fn parse_bytes_binary_suffix() {
        assert_eq!(parse_bytes_suffix("4Ki").unwrap(), 4096);
        assert_eq!(parse_bytes_suffix("1Mi").unwrap(), 1024 * 1024);
    }

    #[test]
    fn parse_bytes_rejects_garbage() {
        assert!(parse_bytes_suffix("").is_err());
        assert!(parse_bytes_suffix("K").is_err());
        assert!(parse_bytes_suffix("10X").is_err());
        assert!(parse_bytes_suffix("abc").is_err());
    }

    // ---------- seek_to_last_n_lines ----------

    #[test]
    fn last_n_lines_basic() {
        let tmp = write_file(b"alpha\nbeta\ngamma\ndelta\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        seek_to_last_n_lines(&mut f, 2).unwrap();
        assert_eq!(dump_from_pos(&mut f), b"gamma\ndelta\n");
    }

    #[test]
    fn last_n_lines_no_trailing_newline() {
        let tmp = write_file(b"alpha\nbeta\ngamma\ndelta");
        let mut f = fs::File::open(tmp.path()).unwrap();
        seek_to_last_n_lines(&mut f, 2).unwrap();
        assert_eq!(dump_from_pos(&mut f), b"gamma\ndelta");
    }

    #[test]
    fn last_n_lines_more_than_present() {
        let tmp = write_file(b"only\none\nline\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        seek_to_last_n_lines(&mut f, 999).unwrap();
        assert_eq!(dump_from_pos(&mut f), b"only\none\nline\n");
    }

    #[test]
    fn last_n_lines_empty_file() {
        let tmp = write_file(b"");
        let mut f = fs::File::open(tmp.path()).unwrap();
        seek_to_last_n_lines(&mut f, 5).unwrap();
        assert_eq!(dump_from_pos(&mut f), b"");
    }

    #[test]
    fn last_n_lines_zero_returns_nothing() {
        let tmp = write_file(b"alpha\nbeta\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        seek_to_last_n_lines(&mut f, 0).unwrap();
        assert_eq!(dump_from_pos(&mut f), b"");
    }

    #[test]
    fn last_n_lines_chunk_boundary() {
        // Build content larger than the internal 4 KiB chunk so we exercise
        // the multi-chunk backwards walk.
        let mut content = Vec::new();
        for i in 0..2000 {
            content.extend_from_slice(format!("line-{i:04}\n").as_bytes());
        }
        let tmp = write_file(&content);
        let mut f = fs::File::open(tmp.path()).unwrap();
        seek_to_last_n_lines(&mut f, 3).unwrap();
        let got = dump_from_pos(&mut f);
        assert_eq!(got, b"line-1997\nline-1998\nline-1999\n");
    }

    // ---------- seek_to_last_n_bytes ----------

    #[test]
    fn last_n_bytes_aligns_to_newline() {
        let tmp = write_file(b"alpha\nbeta\ngamma\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        // 8 bytes from end = "eta\ngamma\n"... but we should align to next \n.
        seek_to_last_n_bytes(&mut f, 8).unwrap();
        assert_eq!(dump_from_pos(&mut f), b"gamma\n");
    }

    #[test]
    fn last_n_bytes_larger_than_file_returns_all() {
        let tmp = write_file(b"hi\nworld\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        seek_to_last_n_bytes(&mut f, 999).unwrap();
        assert_eq!(dump_from_pos(&mut f), b"hi\nworld\n");
    }

    #[test]
    fn last_n_bytes_starts_at_newline_boundary() {
        let tmp = write_file(b"alpha\nbeta\ngamma\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        // Exactly aligned: 6 bytes from end = "gamma\n", and the byte at the
        // pivot is already past the \n that ends "beta".
        seek_to_last_n_bytes(&mut f, 6).unwrap();
        assert_eq!(dump_from_pos(&mut f), b"gamma\n");
    }

    // ---------- emit_head_n_lines / emit_reverse_bounded ----------

    #[test]
    fn head_n_emits_first_n_lines_only() {
        let tmp = write_file(b"a\nb\nc\nd\ne\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        let mut out = Vec::new();
        emit_head_n_lines(&mut f, 2, None, &mut out).unwrap();
        assert_eq!(out, b"a\nb\n");
    }

    #[test]
    fn head_n_more_than_lines_dumps_all() {
        let tmp = write_file(b"a\nb\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        let mut out = Vec::new();
        emit_head_n_lines(&mut f, 99, None, &mut out).unwrap();
        assert_eq!(out, b"a\nb\n");
    }

    #[test]
    fn reverse_with_lines_emits_newest_first() {
        let tmp = write_file(b"alpha\nbeta\ngamma\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        let mut out = Vec::new();
        emit_reverse_bounded(&mut f, Some(3), None, None, &mut out).unwrap();
        assert_eq!(out, b"gamma\nbeta\nalpha\n");
    }

    #[test]
    fn reverse_with_lines_subset() {
        let tmp = write_file(b"alpha\nbeta\ngamma\ndelta\n");
        let mut f = fs::File::open(tmp.path()).unwrap();
        let mut out = Vec::new();
        emit_reverse_bounded(&mut f, Some(2), None, None, &mut out).unwrap();
        assert_eq!(out, b"delta\ngamma\n");
    }

    // ---------- reverse_lines helper ----------

    #[test]
    fn reverse_lines_simple() {
        assert_eq!(reverse_lines(b"a\nb\nc\n"), b"c\nb\na\n");
    }

    #[test]
    fn reverse_lines_no_trailing_newline() {
        // Input has no trailing `\n`; output preserves that property.
        assert_eq!(reverse_lines(b"a\nb\nc"), b"c\nb\na");
    }

    #[test]
    fn reverse_lines_empty() {
        assert_eq!(reverse_lines(b""), b"");
    }

    #[test]
    fn reverse_lines_single() {
        assert_eq!(reverse_lines(b"only\n"), b"only\n");
    }

    // ---------- validate_flag_combinations ----------

    #[test]
    fn validates_cursor_vs_bounded() {
        // cursor + lines → error
        assert!(
            validate_flag_combinations(Some(10), false, None, false, false, Some(0), false, false)
                .is_err()
        );
        // cursor + bytes → error
        assert!(
            validate_flag_combinations(None, true, None, false, false, Some(0), false, false)
                .is_err()
        );
        // cursor + head → error
        assert!(
            validate_flag_combinations(None, false, Some(10), false, false, Some(0), false, false)
                .is_err()
        );
        // cursor + reverse → error
        assert!(
            validate_flag_combinations(None, false, None, true, false, Some(0), false, false)
                .is_err()
        );
    }

    #[test]
    fn validates_json_vs_follow() {
        assert!(
            validate_flag_combinations(None, false, None, false, true, None, true, false).is_err()
        );
    }

    #[test]
    fn validates_cursor_alone_ok() {
        assert!(
            validate_flag_combinations(None, false, None, false, false, Some(0), false, false)
                .is_ok()
        );
    }

    #[test]
    fn validates_cursor_with_json_ok() {
        assert!(validate_flag_combinations(None, false, None, false, false, Some(42), true, false).is_ok());
    }

    // ---------- parse_time_spec ----------

    #[test]
    fn time_spec_now() {
        assert_eq!(parse_time_spec("now", 1_000_000).unwrap(), 1_000_000);
    }

    #[test]
    fn time_spec_relative() {
        let now = 1_700_000_000_000u64;
        assert_eq!(parse_time_spec("30s", now).unwrap(), now - 30_000);
        assert_eq!(parse_time_spec("5m", now).unwrap(), now - 300_000);
        assert_eq!(parse_time_spec("2h", now).unwrap(), now - 7_200_000);
        assert_eq!(parse_time_spec("1d", now).unwrap(), now - 86_400_000);
        assert_eq!(parse_time_spec("500ms", now).unwrap(), now - 500);
    }

    #[test]
    fn time_spec_with_ago_suffix() {
        let now = 1_700_000_000_000u64;
        assert_eq!(parse_time_spec("30s ago", now).unwrap(), now - 30_000);
    }

    #[test]
    fn time_spec_absolute_ms() {
        assert_eq!(parse_time_spec("1234567890", 999).unwrap(), 1234567890);
    }

    #[test]
    fn time_spec_rejects_garbage() {
        assert!(parse_time_spec("", 0).is_err());
        assert!(parse_time_spec("abc", 0).is_err());
        assert!(parse_time_spec("10x", 0).is_err());
    }

    #[test]
    fn time_spec_saturates_at_zero() {
        // 10d ago from time = 100 should saturate to 0, not underflow.
        assert_eq!(parse_time_spec("10d", 100).unwrap(), 0);
    }

    // ---------- time-window flag mutex ----------

    #[test]
    fn validates_time_window_vs_bounded() {
        // since + lines → error
        assert!(
            validate_flag_combinations(Some(5), false, None, false, false, None, false, true)
                .is_err()
        );
        // since + cursor → error
        assert!(
            validate_flag_combinations(None, false, None, false, false, Some(0), false, true)
                .is_err()
        );
        // since + follow → error
        assert!(
            validate_flag_combinations(None, false, None, false, true, None, false, true).is_err()
        );
    }
}

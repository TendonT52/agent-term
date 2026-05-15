use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

use serde_json::json;

use crate::ids::is_valid_id;
use crate::pty::{parse_ms_prefix, AnsiStripper};
use crate::state::log_path;
use crate::tail::{now_ms, parse_time_spec};

/// CLI options for the `slice` verb. Exactly one selector family must be set:
/// time (`from` + `to`), cursor (`from_cursor` + `to_cursor`), or none (full
/// log). Half-open ranges are allowed: omitting one end means "from start" or
/// "to EOF" respectively.
pub struct SliceOptions {
    pub id: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub from_cursor: Option<u64>,
    pub to_cursor: Option<u64>,
    pub strip_ansi: bool,
    pub keep_timestamps: bool,
    pub json: bool,
}

pub fn run(opts: SliceOptions) -> ExitCode {
    let SliceOptions {
        id,
        from,
        to,
        from_cursor,
        to_cursor,
        strip_ansi,
        keep_timestamps,
        json,
    } = opts;

    if !is_valid_id(&id) {
        eprintln!("agent-terminal: slice: invalid id {id:?}");
        return ExitCode::from(1);
    }

    let time_mode = from.is_some() || to.is_some();
    let cursor_mode = from_cursor.is_some() || to_cursor.is_some();
    if time_mode && cursor_mode {
        eprintln!(
            "agent-terminal: slice: time selectors (--from/--to) and byte selectors \
             (--from-cursor/--to-cursor) are mutually exclusive"
        );
        return ExitCode::from(1);
    }

    let log = log_path(&id);
    if !log.exists() {
        eprintln!("agent-terminal: slice: no log for id {id:?}");
        return ExitCode::from(1);
    }
    let mut file = match fs::File::open(&log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("agent-terminal: slice: {e}");
            return ExitCode::from(1);
        }
    };

    if cursor_mode {
        return run_cursor_slice(
            &mut file,
            from_cursor,
            to_cursor,
            strip_ansi,
            json,
        );
    }

    // Default: time mode (also handles "no selectors" = full log).
    let now = now_ms();
    let from_ms = match from.as_deref().map(|s| parse_time_spec(s, now)) {
        Some(Ok(t)) => Some(t),
        Some(Err(e)) => {
            eprintln!("agent-terminal: slice: --from: {e}");
            return ExitCode::from(1);
        }
        None => None,
    };
    let to_ms = match to.as_deref().map(|s| parse_time_spec(s, now)) {
        Some(Ok(t)) => Some(t),
        Some(Err(e)) => {
            eprintln!("agent-terminal: slice: --to: {e}");
            return ExitCode::from(1);
        }
        None => None,
    };

    run_time_slice(&mut file, from_ms, to_ms, strip_ansi, keep_timestamps, json)
}

fn run_cursor_slice(
    file: &mut fs::File,
    from_cursor: Option<u64>,
    to_cursor: Option<u64>,
    strip_ansi: bool,
    json: bool,
) -> ExitCode {
    let len = match file.seek(SeekFrom::End(0)) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("agent-terminal: slice: {e}");
            return ExitCode::from(1);
        }
    };
    let from = from_cursor.unwrap_or(0).min(len);
    let to = to_cursor.unwrap_or(len).min(len);
    if from > to {
        eprintln!(
            "agent-terminal: slice: --from-cursor {from} is past --to-cursor {to}"
        );
        return ExitCode::from(1);
    }
    if let Err(e) = file.seek(SeekFrom::Start(from)) {
        eprintln!("agent-terminal: slice: {e}");
        return ExitCode::from(1);
    }
    let want = (to - from) as usize;
    let mut buf = vec![0u8; want];
    if let Err(e) = file.read_exact(&mut buf) {
        eprintln!("agent-terminal: slice: {e}");
        return ExitCode::from(1);
    }

    let stripped = if strip_ansi {
        let mut clean = Vec::with_capacity(buf.len());
        AnsiStripper::new().feed(&buf, &mut clean);
        clean
    } else {
        buf
    };

    emit(stripped, json, |bytes, lines| {
        json!({
            "from_cursor": from,
            "to_cursor": to,
            "lines_emitted": lines,
            "bytes_emitted": bytes.len(),
            "content": String::from_utf8_lossy(bytes),
        })
    })
}

fn run_time_slice(
    file: &mut fs::File,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    strip_ansi: bool,
    keep_timestamps: bool,
    json: bool,
) -> ExitCode {
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        eprintln!("agent-terminal: slice: {e}");
        return ExitCode::from(1);
    }
    let mut all = Vec::new();
    if let Err(e) = file.read_to_end(&mut all) {
        eprintln!("agent-terminal: slice: {e}");
        return ExitCode::from(1);
    }

    let mut emitted = Vec::<u8>::new();
    let mut saw_timestamped = false;
    let mut start = 0usize;
    for i in 0..all.len() {
        if all[i] != b'\n' {
            continue;
        }
        let line = &all[start..=i];
        start = i + 1;

        let Some((ts, body_start)) = parse_ms_prefix(line) else {
            continue;
        };
        saw_timestamped = true;
        if let Some(f) = from_ms {
            if ts < f {
                continue;
            }
        }
        if let Some(t) = to_ms {
            if ts > t {
                break;
            }
        }
        let body = if keep_timestamps {
            line
        } else {
            &line[body_start..]
        };
        emitted.extend_from_slice(body);
    }

    let time_selectors_used = from_ms.is_some() || to_ms.is_some();
    if time_selectors_used && !saw_timestamped {
        eprintln!(
            "agent-terminal: slice: --from/--to require timestamped logs; \
             spawn with --timestamps (or AGENT_TERMINAL_TIMESTAMPS=1)"
        );
        return ExitCode::from(1);
    }

    // No selectors at all = pure dump. Don't drop non-timestamped lines.
    if !time_selectors_used && !saw_timestamped {
        emitted = all;
    }

    let stripped = if strip_ansi {
        let mut clean = Vec::with_capacity(emitted.len());
        AnsiStripper::new().feed(&emitted, &mut clean);
        clean
    } else {
        emitted
    };

    emit(stripped, json, |bytes, lines| {
        json!({
            "from_ms": from_ms,
            "to_ms": to_ms,
            "lines_emitted": lines,
            "bytes_emitted": bytes.len(),
            "content": String::from_utf8_lossy(bytes),
        })
    })
}

fn emit<F: Fn(&[u8], u64) -> serde_json::Value>(
    bytes: Vec<u8>,
    json: bool,
    json_body: F,
) -> ExitCode {
    let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    if json {
        println!("{}", json_body(&bytes, lines));
    } else {
        let mut stdout = io::stdout().lock();
        if let Err(e) = stdout.write_all(&bytes) {
            if e.kind() != io::ErrorKind::BrokenPipe {
                eprintln!("agent-terminal: slice: {e}");
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}

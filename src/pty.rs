use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Render the current `[<ms>] ` prefix used by timestamped log writes.
/// Public so tail's filter / slice can parse it back with the same width.
pub fn current_ms_prefix() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("[{ms}] ")
}

/// Parses a `[<ms>] ` prefix at the start of `line`. Returns `(timestamp_ms,
/// line_body_offset)` on success. `None` if the line doesn't start with a
/// recognised prefix.
pub fn parse_ms_prefix(line: &[u8]) -> Option<(u64, usize)> {
    if line.first() != Some(&b'[') {
        return None;
    }
    let close = line.iter().position(|&b| b == b']')?;
    if close < 2 {
        return None;
    }
    // Reject if the brackets contain non-digit characters — keeps the parser
    // from matching ANSI bracketed sequences like `[31m`.
    let inner = &line[1..close];
    if !inner.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let s = std::str::from_utf8(inner).ok()?;
    let ms: u64 = s.parse().ok()?;
    // The render appends a single space after `]`; tolerate its absence
    // (a future tweak might omit it) by reporting the body offset right
    // after `] ` if present, else right after `]`.
    let body_start = if line.get(close + 1) == Some(&b' ') {
        close + 2
    } else {
        close + 1
    };
    Some((ms, body_start))
}

pub const DEFAULT_LOG_SIZE: u64 = 10 * 1024 * 1024;
pub const DEFAULT_LOG_SEGMENTS: usize = 3;

/// Resolves the configured log rotation size from the environment, or the
/// default. Setting the env var to `0` disables rotation (effectively
/// unbounded log).
pub fn log_size_from_env() -> u64 {
    env::var("AGENT_TERM_LOG_SIZE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_LOG_SIZE)
}

pub fn log_segments_from_env() -> usize {
    env::var("AGENT_TERM_LOG_SEGMENTS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LOG_SEGMENTS)
}

pub fn timestamps_from_env() -> bool {
    matches!(env::var("AGENT_TERM_TIMESTAMPS").as_deref(), Ok("1"))
}

/// Append-only writer that rotates the underlying file once it reaches
/// `max_size` bytes. After rotation the current file becomes `.log.1`, the
/// previous `.log.1` becomes `.log.2`, etc. `.log.{max_segments}` is dropped
/// before the shift. `max_size == 0` disables rotation entirely.
///
/// When `timestamps` is true, each logical line is prefixed with
/// `[<ms_since_epoch>] ` exactly once. Prefix insertion tracks "at the start
/// of a line" state across `write_all` calls, so a chunk that splits a line
/// won't double-stamp.
pub struct LogWriter {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    max_size: u64,
    max_segments: usize,
    timestamps: bool,
    at_line_start: bool,
}

impl LogWriter {
    pub fn open_with(
        path: PathBuf,
        max_size: u64,
        max_segments: usize,
        timestamps: bool,
    ) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        // If the file already exists and ends mid-line (no trailing \n), the
        // next write continues that line — no leading prefix. Otherwise the
        // next byte is at a line start.
        let at_line_start = if written == 0 {
            true
        } else {
            let mut f2 = OpenOptions::new().read(true).open(&path)?;
            f2.seek(SeekFrom::End(-1))?;
            let mut last = [0u8; 1];
            f2.read_exact(&mut last)?;
            last[0] == b'\n'
        };
        Ok(Self {
            path,
            file: Some(file),
            written,
            max_size,
            max_segments,
            timestamps,
            at_line_start,
        })
    }

    pub fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        if !self.timestamps {
            return self.write_raw(buf);
        }

        // Walk the input, emitting a [<ms>] prefix whenever we're at the
        // start of a logical line. Logical lines are \n-delimited; \r is
        // treated as part of the line, not a separator.
        let mut start = 0usize;
        for i in 0..buf.len() {
            if self.at_line_start {
                if start < i {
                    self.write_raw(&buf[start..i])?;
                    start = i;
                }
                let prefix = current_ms_prefix();
                self.write_raw(prefix.as_bytes())?;
                self.at_line_start = false;
            }
            if buf[i] == b'\n' {
                self.write_raw(&buf[start..=i])?;
                start = i + 1;
                self.at_line_start = true;
            }
        }
        if start < buf.len() {
            self.write_raw(&buf[start..])?;
        }
        Ok(())
    }

    fn write_raw(&mut self, buf: &[u8]) -> std::io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        // Rotation disabled — single growing file.
        if self.max_size == 0 || self.max_segments == 0 {
            if let Some(f) = self.file.as_mut() {
                f.write_all(buf)?;
                self.written += buf.len() as u64;
            }
            return Ok(());
        }

        let mut start = 0;
        while start < buf.len() {
            let room = self.max_size.saturating_sub(self.written) as usize;
            if room == 0 {
                self.rotate()?;
                continue;
            }
            let end = (start + room).min(buf.len());
            if let Some(f) = self.file.as_mut() {
                f.write_all(&buf[start..end])?;
                self.written += (end - start) as u64;
            }
            start = end;
            if self.written >= self.max_size {
                self.rotate()?;
            }
        }
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        // Drop the writer's file handle so the rename below succeeds on every
        // platform (Windows requires the file to be closed; Unix is more lenient
        // but explicit close keeps semantics consistent).
        self.file = None;

        // Drop the oldest segment if we're at the cap.
        let oldest = numbered_path(&self.path, self.max_segments);
        let _ = fs::remove_file(&oldest);

        // Shift: .log.{N-1} → .log.N, …, .log.1 → .log.2
        for i in (1..self.max_segments).rev() {
            let from = numbered_path(&self.path, i);
            if from.exists() {
                let to = numbered_path(&self.path, i + 1);
                let _ = fs::rename(&from, &to);
            }
        }

        // Current → .log.1
        if self.path.exists() {
            let to = numbered_path(&self.path, 1);
            fs::rename(&self.path, &to)?;
        }

        // Reopen current.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.file = Some(file);
        self.written = 0;
        Ok(())
    }
}

/// Build the path for a numbered rotation segment: foo.log.1, foo.log.2, …
fn numbered_path(path: &Path, n: usize) -> PathBuf {
    let mut buf: OsString = path.as_os_str().to_owned();
    buf.push(format!(".{n}"));
    PathBuf::from(buf)
}

/// Drains a `Read` (e.g. PTY master) into a `LogWriter` until EOF or error.
/// Intended to run on a blocking std thread; not async.
///
/// When `counters` is `Some`, updates `line_count` / `last_line_at_ms` /
/// `bytes_written` so `summary` can be answered without re-scanning the log.
pub fn stream_to_log(
    mut reader: Box<dyn Read + Send>,
    mut writer: LogWriter,
    counters: Option<std::sync::Arc<crate::daemon::DaemonCounters>>,
) {
    use std::sync::atomic::Ordering;

    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let _ = writer.write_all(&buf[..n]);
                if let Some(ref c) = counters {
                    let mut newlines = 0u64;
                    for &b in &buf[..n] {
                        if b == b'\n' {
                            newlines += 1;
                        }
                    }
                    if newlines > 0 {
                        c.line_count.fetch_add(newlines, Ordering::Relaxed);
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        c.last_line_at_ms.store(now, Ordering::Relaxed);
                    }
                    c.bytes_written.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            // On Linux the master read returns EIO once every slave handle is
            // closed (i.e. the child has exited); macOS returns 0 instead.
            // Either way we stop reading.
            Err(_) => break,
        }
    }
}

/// State machine that strips ANSI escape sequences across arbitrary buffer
/// splits. Recognises CSI (`ESC [ … finalByte`), OSC (`ESC ] … BEL | ESC \`),
/// and generic two-byte escapes (`ESC X`).
pub struct AnsiStripper {
    state: StripState,
}

#[derive(Clone, Copy)]
enum StripState {
    Normal,
    AfterEsc,
    Csi,
    Osc,
    OscEscPending,
}

impl Default for AnsiStripper {
    fn default() -> Self {
        Self {
            state: StripState::Normal,
        }
    }
}

impl AnsiStripper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed input bytes; clean bytes are appended to `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                StripState::Normal => {
                    if b == 0x1b {
                        self.state = StripState::AfterEsc;
                    } else {
                        out.push(b);
                    }
                }
                StripState::AfterEsc => match b {
                    b'[' => self.state = StripState::Csi,
                    b']' => self.state = StripState::Osc,
                    // ESC X (2-byte escape) — consume X.
                    _ => self.state = StripState::Normal,
                },
                StripState::Csi => {
                    // Final byte of a CSI sequence is in 0x40..=0x7e.
                    if (0x40..=0x7e).contains(&b) {
                        self.state = StripState::Normal;
                    }
                }
                StripState::Osc => match b {
                    // BEL ends OSC.
                    0x07 => self.state = StripState::Normal,
                    // ST: ESC \ — wait for the trailing \\ before ending.
                    0x1b => self.state = StripState::OscEscPending,
                    _ => {}
                },
                StripState::OscEscPending => match b {
                    b'\\' => self.state = StripState::Normal,
                    // Anything else: not a real ST; treat as fresh ESC in OSC.
                    0x1b => self.state = StripState::OscEscPending,
                    _ => self.state = StripState::Osc,
                },
            }
        }
    }
}

/// One-shot convenience for stripping a buffer when state need not persist.
#[cfg(test)]
pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    AnsiStripper::new().feed(input, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn log_writer_no_rotation_writes_through() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.log");
        let mut w = LogWriter::open_with(path.clone(), 0, 0, false).unwrap();
        w.write_all(b"hello\nworld\n").unwrap();
        let content = fs::read(&path).unwrap();
        assert_eq!(content, b"hello\nworld\n");
    }

    #[test]
    fn log_writer_rotates_and_caps_segments() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.log");
        let mut w = LogWriter::open_with(path.clone(), 10, 2, false).unwrap();

        // Write 35 bytes in chunks crossing rotation boundaries multiple times.
        // Each rotation: current .log → .log.1, previous .log.1 → .log.2,
        // previous .log.2 dropped. With segments=2, we keep at most .log.1, .log.2.
        w.write_all(b"aaaaaaaaaa").unwrap(); // 10 bytes → rotate to .log.1
        w.write_all(b"bbbbbbbbbb").unwrap(); // 10 → .log.1 (b's), prev .log.1 (a) → .log.2
        w.write_all(b"cccccccccc").unwrap(); // 10 → .log.1 (c's), b → .log.2, a dropped
        w.write_all(b"ddddd").unwrap(); // 5 → current .log

        assert_eq!(fs::read(&path).unwrap(), b"ddddd");
        assert_eq!(fs::read(numbered_path(&path, 1)).unwrap(), b"cccccccccc");
        assert_eq!(fs::read(numbered_path(&path, 2)).unwrap(), b"bbbbbbbbbb");
        assert!(!numbered_path(&path, 3).exists());
    }

    #[test]
    fn strip_ansi_removes_csi_sgr() {
        assert_eq!(strip_ansi(b"\x1b[31mred\x1b[0m"), b"red");
    }

    #[test]
    fn strip_ansi_removes_cursor_movement() {
        assert_eq!(strip_ansi(b"hi\x1b[2Athere\x1b[1;31mX\x1b[mY"), b"hithereXY");
    }

    #[test]
    fn strip_ansi_handles_osc_bel() {
        // Set window title: ESC ] 0 ; t i t l e BEL
        assert_eq!(
            strip_ansi(b"before\x1b]0;some title\x07after"),
            b"beforeafter"
        );
    }

    #[test]
    fn strip_ansi_handles_osc_st() {
        // OSC terminated by ESC \\ (ST).
        assert_eq!(
            strip_ansi(b"before\x1b]0;title\x1b\\after"),
            b"beforeafter"
        );
    }

    #[test]
    fn strip_ansi_survives_chunk_split_mid_csi() {
        let mut s = AnsiStripper::new();
        let mut out = Vec::new();
        s.feed(b"hello\x1b[3", &mut out);
        s.feed(b"1mred\x1b[0m world", &mut out);
        assert_eq!(out, b"hellored world");
    }

    #[test]
    fn strip_ansi_passes_through_plain_text() {
        let raw = b"plain ascii\n and unicode \xe2\x9c\x93";
        assert_eq!(strip_ansi(raw), raw);
    }

    // ---------- timestamped LogWriter ----------


    #[test]
    fn timestamped_writer_prepends_prefix_per_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ts.log");
        let mut w = LogWriter::open_with(path.clone(), 0, 0, true).unwrap();
        w.write_all(b"first line\nsecond line\n").unwrap();
        let content = fs::read(&path).unwrap();

        // Both lines should have a `[<ms>] ` prefix and the same trailing body.
        let mut lines = content
            .split(|&b| b == b'\n')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        // PTY wouldn't add trailing \n on empty; we have 2 non-empty lines.
        assert_eq!(lines.len(), 2);
        let (ts1, body1) = parse_ms_prefix(lines.remove(0)).unwrap();
        let (ts2, body2) = parse_ms_prefix(lines.remove(0)).unwrap();
        assert!(ts1 > 0 && ts2 >= ts1);
        // body1 starts after "] " and runs until the next byte
        let content_str_1 = std::str::from_utf8(&content).unwrap();
        assert!(content_str_1.contains("first line"));
        assert!(content_str_1.contains("second line"));
        // body indices line up with the original content
        let _ = (body1, body2);
    }

    #[test]
    fn timestamped_writer_handles_split_chunks() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("split.log");
        let mut w = LogWriter::open_with(path.clone(), 0, 0, true).unwrap();
        // Mid-line split: "hello wo" then "rld\nbye\n"
        w.write_all(b"hello wo").unwrap();
        w.write_all(b"rld\nbye\n").unwrap();
        let content = fs::read(&path).unwrap();
        // Exactly two prefixes: one at start, one after the first \n.
        let prefix_count = content.windows(1).filter(|w| w[0] == b'[').count();
        assert_eq!(prefix_count, 2, "content = {:?}", String::from_utf8_lossy(&content));
        let s = String::from_utf8_lossy(&content);
        assert!(s.contains("hello world\n"));
        assert!(s.contains("bye\n"));
    }

    #[test]
    fn non_timestamped_writer_is_unchanged() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("no-ts.log");
        let mut w = LogWriter::open_with(path.clone(), 0, 0, false).unwrap();
        w.write_all(b"plain output\nno prefix\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"plain output\nno prefix\n");
    }

    #[test]
    fn parse_ms_prefix_basic() {
        let (ts, body) = parse_ms_prefix(b"[1700000000000] hello\n").unwrap();
        assert_eq!(ts, 1_700_000_000_000);
        assert_eq!(body, 16);
    }

    #[test]
    fn parse_ms_prefix_rejects_ansi() {
        // ANSI sequences look like "[31m..." — must not be mistaken for a ts prefix.
        assert!(parse_ms_prefix(b"\x1b[31mred\x1b[0m\n").is_none());
        assert!(parse_ms_prefix(b"[31m...").is_none());
    }

    #[test]
    fn parse_ms_prefix_no_prefix() {
        assert!(parse_ms_prefix(b"no prefix here\n").is_none());
    }

    #[test]
    fn parse_ms_prefix_tolerates_no_space_after_bracket() {
        let (ts, body) = parse_ms_prefix(b"[42]x").unwrap();
        assert_eq!(ts, 42);
        assert_eq!(body, 4);
    }

    #[test]
    fn timestamped_writer_resumes_correctly_after_open() {
        // Simulate an existing partially-written line: opener should NOT
        // prepend a prefix to the continuation.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("resume.log");
        fs::write(&path, b"[42] partial").unwrap();
        let mut w = LogWriter::open_with(path.clone(), 0, 0, true).unwrap();
        w.write_all(b" more\n").unwrap();
        let content = fs::read(&path).unwrap();
        assert_eq!(content, b"[42] partial more\n");
    }

    #[test]
    fn timestamped_writer_after_newline_emits_prefix() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("resume2.log");
        fs::write(&path, b"[10] alpha\n").unwrap();
        let mut w = LogWriter::open_with(path.clone(), 0, 0, true).unwrap();
        w.write_all(b"beta\n").unwrap();
        let content = fs::read(&path).unwrap();
        let s = String::from_utf8_lossy(&content);
        assert!(s.starts_with("[10] alpha\n["));
        assert!(s.ends_with("beta\n"));
    }
}

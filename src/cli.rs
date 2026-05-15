use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::client::{ensure_daemon, send_request, status, SpawnOptions};
use crate::ids::{gen_id, is_valid_id};
use crate::ipc::Request;
use crate::meta::{canonicalize_project, list_active_metas, Meta};
use crate::state::{
    cleanup_stale_files, get_state_dir, is_pid_alive, log_path, pid_path, sock_path,
};

#[derive(Parser)]
#[command(
    name = "agent-term",
    version,
    about = "Detached, observable subprocess runner for AI agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Spawn a command as a detached, managed subprocess.
    Spawn {
        /// Explicit id. If omitted, a random short id is generated.
        #[arg(long)]
        id: Option<String>,
        /// Filesystem path scoping this daemon. Canonicalised. Defaults to $PWD.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
        /// Human-readable name. Must be unique within the project.
        #[arg(long, value_name = "STR")]
        name: Option<String>,
        /// Free-form annotation, repeatable: `--tag env=staging --tag region=us`.
        #[arg(long = "tag", value_name = "K=V")]
        tags: Vec<String>,
        /// Prepend `[<ms_since_epoch>] ` to every line written to the log.
        /// Required for `tail --since`, `--until`, and the `slice` verb's
        /// time-based selectors. Equivalent to AGENT_TERM_TIMESTAMPS=1.
        #[arg(long)]
        timestamps: bool,
        /// Command followed by its arguments. To run shell text, use `sh -c '...'`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },
    /// List managed subprocesses. Filtered to the current project by default.
    List {
        /// Restrict to entries whose project equals this path (canonicalised).
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
        /// Filter entries: every k=v must be present in tags. Repeatable.
        #[arg(long = "tag", value_name = "K=V")]
        tags: Vec<String>,
        /// Ignore project scoping — show every active daemon.
        #[arg(long)]
        all: bool,
        /// Emit machine-parseable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show the daemon's view of a managed subprocess.
    Status { id: String },
    /// Stream the captured log for a managed subprocess.
    Tail {
        id: String,
        /// Follow log growth (like `tail -f`). Exits when the daemon for
        /// this id is gone.
        #[arg(short, long)]
        follow: bool,
        /// Strip ANSI/CSI/OSC escape sequences from the output.
        #[arg(long)]
        strip_ansi: bool,
        /// Emit only the last N lines of the log. Mutually exclusive with --bytes.
        #[arg(long, value_name = "N")]
        lines: Option<u64>,
        /// Emit only the last N bytes (line-aligned). Accepts K/M/G/Ki/Mi/Gi suffixes.
        #[arg(long, value_name = "N")]
        bytes: Option<String>,
        /// Emit the first N lines from the start of the log. Mutually exclusive
        /// with --lines/--bytes/--reverse/--follow.
        #[arg(long, value_name = "N")]
        head: Option<u64>,
        /// Emit selected lines newest-first. Requires --lines or --bytes.
        #[arg(long)]
        reverse: bool,
        /// Resume reading at the given byte offset. Stale cursors (past EOF)
        /// emit an empty result with `cursor_stale: true`.
        #[arg(long, value_name = "POS", visible_alias = "since-cursor")]
        cursor: Option<u64>,
        /// Emit a JSON envelope `{cursor_start, cursor, content, lines_emitted}`
        /// instead of raw bytes. Always includes `cursor` so the agent can
        /// pass it back via --cursor on the next call.
        #[arg(long)]
        json: bool,
        /// Emit only lines whose timestamp prefix is at or after this time
        /// spec. Accepts `30s` / `5m` / `2h` / `1d` (relative to now), `now`,
        /// or an integer ms-since-epoch. Requires `spawn --timestamps`.
        #[arg(long, value_name = "TIME")]
        since: Option<String>,
        /// Emit only lines whose timestamp is at or before this time spec.
        #[arg(long, value_name = "TIME")]
        until: Option<String>,
        /// Preserve the `[<ms>] ` prefix in emitted lines (default strips it).
        #[arg(long)]
        keep_timestamps: bool,
        /// Sugar: forwards to `agent-term grep`. Use the full `grep` verb
        /// for richer output.
        #[arg(long, value_name = "REGEX")]
        grep: Option<String>,
        /// Context lines around each --grep match (like `grep -C N`).
        #[arg(long, value_name = "N", default_value_t = 0)]
        around: usize,
        /// Cap --grep matches.
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
    },
    /// Block until a regex pattern matches in a subprocess's log.
    Wait {
        id: String,
        /// Regular expression to match. Mutually exclusive with --pattern-file.
        #[arg(long)]
        pattern: Option<String>,
        /// Read the regex from a file (handy when the pattern fights shell quoting).
        #[arg(long, value_name = "PATH")]
        pattern_file: Option<String>,
        /// Maximum time to wait. Accepts `30s`, `500ms`, `2m`, `1h`. Default: no timeout.
        #[arg(long)]
        timeout: Option<String>,
        /// Match across the whole log buffer with `^`/`$` honoring line boundaries.
        #[arg(long)]
        multiline: bool,
        /// Emit a machine-readable JSON result on stdout instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// Send a signal to a managed subprocess.
    Kill {
        id: String,
        #[arg(long, default_value = "TERM")]
        signal: String,
    },
    /// Pattern-match a managed subprocess's log. Like `grep -A N -B N` but
    /// time-window aware and JSON-capable.
    Grep {
        id: String,
        /// Regular expression. Mutually exclusive with --pattern-file.
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long, value_name = "PATH")]
        pattern_file: Option<String>,
        /// Number of context lines on each side of a match (like `grep -C N`).
        #[arg(long, value_name = "N", default_value_t = 0)]
        around: usize,
        /// Stop after N matches.
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
        /// Only consider lines whose `[<ms>] ` prefix is at or after this
        /// time spec. Requires `spawn --timestamps`.
        #[arg(long, value_name = "TIME")]
        since: Option<String>,
        #[arg(long, value_name = "TIME")]
        until: Option<String>,
        #[arg(long)]
        strip_ansi: bool,
        /// Match against the entire line including any `[ms]` prefix. Default
        /// is to match against the body (post-prefix).
        #[arg(long)]
        match_full_line: bool,
        #[arg(long)]
        json: bool,
        /// Enable multi-line regex semantics so `^`/`$` honour line boundaries.
        #[arg(long)]
        multiline: bool,
    },
    /// Read an explicit byte-range or time-range slice of a managed log.
    /// Two selector families, mutually exclusive: time (`--from/--to`,
    /// requires --timestamps spawn) or byte offsets (`--from-cursor/--to-cursor`).
    Slice {
        id: String,
        /// Time-window start: `30s` / `5m` / `now` / integer ms-since-epoch.
        #[arg(long, value_name = "TIME")]
        from: Option<String>,
        /// Time-window end (same syntax as --from).
        #[arg(long, value_name = "TIME")]
        to: Option<String>,
        /// Byte-offset start (inclusive).
        #[arg(long, value_name = "POS")]
        from_cursor: Option<u64>,
        /// Byte-offset end (exclusive). Defaults to EOF.
        #[arg(long, value_name = "POS")]
        to_cursor: Option<u64>,
        /// Strip ANSI escape sequences from the emitted content.
        #[arg(long)]
        strip_ansi: bool,
        /// Preserve the `[<ms>] ` prefix in emitted lines.
        #[arg(long)]
        keep_timestamps: bool,
        /// Emit a JSON envelope.
        #[arg(long)]
        json: bool,
    },
    /// Cheap health snapshot: state, log size/lines/segments, last-line age,
    /// recent error/warning counts. ~200 token JSON output.
    Summary {
        id: String,
        #[arg(long)]
        json: bool,
        /// Time window for `recent.errors` / `recent.warnings`. Accepts the
        /// same syntax as `tail --since` (e.g. `30s`, `5m`, `1h`).
        #[arg(long, value_name = "TIME", default_value = "60s")]
        recent_window: String,
        /// Regex used to classify an "error" line. Default: `(?i)error|fatal`.
        #[arg(long, value_name = "REGEX")]
        error_pattern: Option<String>,
        /// Regex used to classify a "warning" line. Default: `(?i)warn`.
        #[arg(long, value_name = "REGEX")]
        warning_pattern: Option<String>,
    },
    /// Diagnose the daemon state directory: live/stale/orphans and misuse heuristics.
    Doctor {
        /// Clean up stale sidecars and kill orphan children.
        #[arg(long)]
        fix: bool,
        /// Emit machine-readable JSON instead of a human-friendly summary.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Spawn {
            id,
            project,
            name,
            tags,
            timestamps,
            argv,
        } => run_spawn(id, project, name, tags, timestamps, argv),
        Command::List {
            project,
            tags,
            all,
            json,
        } => run_list(project, tags, all, json),
        Command::Status { id } => run_status(&id),
        Command::Kill { id, signal } => run_kill(&id, &signal),
        Command::Tail {
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
            grep,
            around,
            limit,
        } => {
            // `tail --grep PATTERN` is sugar over `agent-term grep`.
            if let Some(pat) = grep {
                return crate::grep::run(crate::grep::GrepOptions {
                    id,
                    pattern: Some(pat),
                    pattern_file: None,
                    around,
                    limit,
                    since,
                    until,
                    strip_ansi,
                    match_full_line: false,
                    json,
                    multiline: false,
                });
            }
            crate::tail::run(crate::tail::TailOptions {
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
            })
        }
        Command::Wait {
            id,
            pattern,
            pattern_file,
            timeout,
            multiline,
            json,
        } => crate::wait::run(crate::wait::WaitOptions {
            id,
            pattern,
            pattern_file,
            timeout,
            multiline,
            json,
        }),
        Command::Grep {
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
        } => crate::grep::run(crate::grep::GrepOptions {
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
        }),
        Command::Slice {
            id,
            from,
            to,
            from_cursor,
            to_cursor,
            strip_ansi,
            keep_timestamps,
            json,
        } => crate::slice::run(crate::slice::SliceOptions {
            id,
            from,
            to,
            from_cursor,
            to_cursor,
            strip_ansi,
            keep_timestamps,
            json,
        }),
        Command::Summary {
            id,
            json,
            recent_window,
            error_pattern,
            warning_pattern,
        } => crate::summary::run(crate::summary::SummaryOptions {
            id,
            json,
            recent_window: Some(recent_window),
            error_pattern,
            warning_pattern,
        }),
        Command::Doctor { fix, json } => {
            crate::doctor::run(crate::doctor::DoctorOptions { fix, json })
        }
    }
}


fn run_spawn(
    id: Option<String>,
    project: Option<PathBuf>,
    name: Option<String>,
    raw_tags: Vec<String>,
    timestamps: bool,
    argv: Vec<String>,
) -> ExitCode {
    let id = match id {
        Some(s) if is_valid_id(&s) => s,
        Some(bad) => {
            eprintln!(
                "agent-term: invalid --id {bad:?}: must be 1-64 chars, [a-z0-9_-] only"
            );
            return ExitCode::from(1);
        }
        None => gen_id(),
    };

    let project_path = match project {
        Some(p) => canonicalize_project(&p),
        None => match std::env::current_dir() {
            Ok(cwd) => canonicalize_project(&cwd),
            Err(e) => {
                eprintln!("agent-term: spawn: cannot read $PWD: {e}");
                return ExitCode::from(1);
            }
        },
    };
    let project_str = project_path.to_string_lossy().into_owned();

    if let Some(ref n) = name {
        if let Err(msg) = validate_name(n) {
            eprintln!("agent-term: spawn: {msg}");
            return ExitCode::from(1);
        }
    }

    let tags = match parse_tags(&raw_tags) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("agent-term: spawn: {e}");
            return ExitCode::from(1);
        }
    };

    // Name uniqueness within project. Checked best-effort: another spawn
    // could race past this check, so the daemon does not also enforce it —
    // the CLI is the single point of policy here.
    if let Some(ref desired) = name {
        for (existing_id, m) in list_active_metas() {
            if m.project == project_str && m.name.as_deref() == Some(desired.as_str()) {
                eprintln!(
                    "agent-term: spawn: name {desired:?} already in use by id {existing_id}"
                );
                return ExitCode::from(1);
            }
        }
    }

    let started_by_pid = parent_pid();

    let opts = SpawnOptions {
        project: &project_str,
        name: name.as_deref(),
        tags: &tags,
        started_by_pid,
        timestamps,
    };

    match ensure_daemon(&id, &argv, &opts) {
        Ok(res) => {
            println!("{}", res.id);
            if res.already_running {
                eprintln!(
                    "agent-term: existing daemon for id {} reused",
                    res.id
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("agent-term: spawn failed: {e}");
            ExitCode::from(1)
        }
    }
}

fn validate_name(n: &str) -> Result<(), String> {
    if n.is_empty() {
        return Err("--name cannot be empty".into());
    }
    if n.len() > 128 {
        return Err("--name must be ≤128 chars".into());
    }
    if n.contains('/') || n.contains('\\') || n.contains('\n') || n.contains('\0') {
        return Err("--name must not contain '/', '\\\\', newline, or NUL".into());
    }
    Ok(())
}

fn parse_tags(raw: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for entry in raw {
        let Some((k, v)) = entry.split_once('=') else {
            return Err(format!("--tag {entry:?} is missing '=' (expected K=V)"));
        };
        if k.is_empty() {
            return Err(format!("--tag {entry:?} has empty key"));
        }
        // Later --tag overrides earlier with same key — consistent with most CLI tools.
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    let p = unsafe { libc::getppid() };
    if p > 0 {
        Some(p as u32)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn parent_pid() -> Option<u32> {
    None
}

/// One row of `list` output, also the per-entry JSON schema.
#[derive(Clone, Debug, Serialize)]
pub struct ListEntry {
    pub schema_version: u32,
    pub id: String,
    pub name: Option<String>,
    pub project: String,
    pub tags: BTreeMap<String, String>,
    pub cmd: Vec<String>,
    pub cwd: String,
    pub started_at: u64,
    pub started_by_pid: Option<u32>,
    pub log_path: String,
    /// "running" or "exited". Determined by IPC `status` when the daemon is
    /// reachable; falls back to "running" when only the pid file is known
    /// alive (e.g. socket race during shutdown).
    pub state: String,
    pub child_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub uptime_ms: u64,
}

fn run_list(
    project: Option<PathBuf>,
    raw_tags: Vec<String>,
    all: bool,
    json: bool,
) -> ExitCode {
    let project_filter = if all {
        None
    } else {
        let p = match project {
            Some(p) => canonicalize_project(&p),
            None => match std::env::current_dir() {
                Ok(cwd) => canonicalize_project(&cwd),
                Err(e) => {
                    eprintln!("agent-term: list: cannot read $PWD: {e}");
                    return ExitCode::from(1);
                }
            },
        };
        Some(p.to_string_lossy().into_owned())
    };

    let tag_filters = match parse_tags(&raw_tags) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("agent-term: list: {e}");
            return ExitCode::from(1);
        }
    };

    // Walk + classify. Stale entries get cleaned as a side effect so `list`
    // doubles as garbage collection.
    let mut metas = walk_and_clean_state_dir();

    metas.retain(|(_, m)| {
        if let Some(ref filter) = project_filter {
            if &m.project != filter {
                return false;
            }
        }
        for (k, v) in &tag_filters {
            if m.tags.get(k).map(String::as_str) != Some(v.as_str()) {
                return false;
            }
        }
        true
    });

    metas.sort_by(|a, b| a.1.started_at.cmp(&b.1.started_at).then(a.0.cmp(&b.0)));

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let entries: Vec<ListEntry> = metas
        .into_iter()
        .map(|(id, m)| build_entry(&id, m, now))
        .collect();

    if json {
        let out = serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string());
        println!("{}", out);
    } else {
        render_table(&entries);
    }

    ExitCode::SUCCESS
}

/// Walk the state directory: collect (id, Meta) for every alive daemon and
/// remove sidecars for dead ones along the way.
fn walk_and_clean_state_dir() -> Vec<(String, Meta)> {
    let dir = get_state_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut alive: Vec<(String, Meta)> = Vec::new();
    let mut sock_ids: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_suffix(".sock") {
            if !id.is_empty() {
                sock_ids.push(id.to_string());
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
                // Meta might not have been written yet — daemon is mid-startup.
                // Fall back to a minimal Meta so the entry is still listed.
                let meta = Meta::load(&crate::state::meta_path(id))
                    .unwrap_or_else(|_| fallback_meta(id));
                alive.push((id.to_string(), meta));
            }
            _ => {
                cleanup_stale_files(id);
            }
        }
    }

    // Orphan .sock with no .pid → cleanup.
    for id in &sock_ids {
        if !pid_path(id).exists() {
            cleanup_stale_files(id);
        }
    }

    alive
}

fn fallback_meta(id: &str) -> Meta {
    Meta {
        schema_version: crate::meta::SCHEMA_VERSION,
        id: id.to_string(),
        name: None,
        project: String::new(),
        tags: BTreeMap::new(),
        cmd: Vec::new(),
        cwd: String::new(),
        started_at: 0,
        started_by_pid: None,
        child_pid: None,
        log_path: log_path(id).to_string_lossy().into_owned(),
    }
}

fn build_entry(id: &str, meta: Meta, now_secs: u64) -> ListEntry {
    let (state, child_pid, exit_code) = query_state(id);
    let uptime_ms = now_secs
        .checked_sub(meta.started_at)
        .map(|d| d * 1000)
        .unwrap_or(0);
    ListEntry {
        schema_version: meta.schema_version,
        id: id.to_string(),
        name: meta.name,
        project: meta.project,
        tags: meta.tags,
        cmd: meta.cmd,
        cwd: meta.cwd,
        started_at: meta.started_at,
        started_by_pid: meta.started_by_pid,
        log_path: meta.log_path,
        state,
        child_pid,
        exit_code,
        uptime_ms,
    }
}

fn query_state(id: &str) -> (String, Option<u32>, Option<i32>) {
    // Only ask the daemon if its socket exists; avoids a multi-second connect
    // wait if the daemon is mid-shutdown.
    if !sock_path(id).exists() {
        return ("running".to_string(), None, None);
    }
    match status(id) {
        Ok(data) => {
            let state = data
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("running")
                .to_string();
            let child_pid = data.get("child_pid").and_then(|v| v.as_u64()).map(|v| v as u32);
            let exit_code = data.get("code").and_then(|v| v.as_i64()).map(|v| v as i32);
            (state, child_pid, exit_code)
        }
        Err(_) => ("running".to_string(), None, None),
    }
}

fn render_table(entries: &[ListEntry]) {
    println!(
        "{:<14} {:<14} {:<28} {:<8} {:<8} CMD",
        "ID", "NAME", "PROJECT", "STATE", "UPTIME"
    );
    for e in entries {
        println!(
            "{:<14} {:<14} {:<28} {:<8} {:<8} {}",
            truncate(&e.id, 13),
            truncate(e.name.as_deref().unwrap_or("-"), 13),
            truncate(&e.project, 27),
            truncate(&e.state, 7),
            format_uptime(e.uptime_ms),
            shell_join(&e.cmd)
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

fn format_uptime(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else if s < 86400 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{}h", s / 86400, (s % 86400) / 3600)
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c == '\'') {
                format!("{:?}", a)
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_status(id: &str) -> ExitCode {
    if !is_valid_id(id) {
        eprintln!("agent-term: invalid id {id:?}");
        return ExitCode::from(1);
    }
    match status(id) {
        Ok(data) => {
            println!("{}", data);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("agent-term: status: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_kill(id: &str, signal: &str) -> ExitCode {
    if !is_valid_id(id) {
        eprintln!("agent-term: invalid id {id:?}");
        return ExitCode::from(1);
    }
    // `kill` semantics: signal the child via the daemon, then ask the daemon
    // to close so its sidecars and the child are gone when this command
    // returns. The `close` action kills the child unconditionally, so a
    // separate signal request is only needed when the user picked something
    // other than TERM.
    let upper = signal.to_ascii_uppercase();
    let sig = upper.strip_prefix("SIG").unwrap_or(&upper);
    if sig != "TERM" {
        let resp = match send_request(
            id,
            &Request {
                action: "signal".into(),
                sig: Some(signal.to_string()),
            },
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("agent-term: kill: {e}");
                return ExitCode::from(1);
            }
        };
        if !resp.success {
            eprintln!(
                "agent-term: kill: {}",
                resp.error.unwrap_or_else(|| "unknown error".into())
            );
            return ExitCode::from(1);
        }
    }

    let resp = match send_request(
        id,
        &Request {
            action: "close".into(),
            sig: None,
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agent-term: kill: {e}");
            return ExitCode::from(1);
        }
    };
    if !resp.success {
        eprintln!(
            "agent-term: kill: {}",
            resp.error.unwrap_or_else(|| "unknown error".into())
        );
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
#[cfg(test)]
mod schema_tests {
    use super::*;
    use serde_json::Value;

    const SCHEMA: &str = include_str!("../schemas/list-entry.schema.json");

    fn sample_entry() -> ListEntry {
        let mut tags = BTreeMap::new();
        tags.insert("env".into(), "staging".into());
        ListEntry {
            schema_version: 1,
            id: "abc12345".into(),
            name: Some("dev".into()),
            project: "/a/proj".into(),
            tags,
            cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
            cwd: "/a/proj/sub".into(),
            started_at: 1_700_000_000,
            started_by_pid: Some(42),
            log_path: "/tmp/abc12345.log".into(),
            state: "running".into(),
            child_pid: Some(100),
            exit_code: None,
            uptime_ms: 5_000,
        }
    }

    /// Validates a JSON value against a subset of JSON Schema sufficient to
    /// cover the list-entry schema: `required`, `properties`, type checks,
    /// `oneOf` for nullable types, `enum`, and `additionalProperties: false`.
    fn validate(schema: &Value, value: &Value) -> Result<(), String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "value is not an object".to_string())?;
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for r in &required {
            if !obj.contains_key(*r) {
                return Err(format!("missing required field: {r}"));
            }
        }
        let props = schema["properties"].as_object().unwrap();
        // additionalProperties: false → object keys must be a subset of props.
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            for k in obj.keys() {
                if !props.contains_key(k) {
                    return Err(format!("unexpected field: {k}"));
                }
            }
        }
        for (k, sub) in props {
            if let Some(v) = obj.get(k) {
                check_type(sub, v).map_err(|e| format!("field {k}: {e}"))?;
            }
        }
        Ok(())
    }

    fn check_type(sub: &Value, v: &Value) -> Result<(), String> {
        if let Some(tp) = sub.get("type").and_then(|t| t.as_str()) {
            type_matches(tp, v)?;
            if tp == "object" {
                if let Some(ap) = sub.get("additionalProperties") {
                    if let Some(ap_obj) = ap.as_object() {
                        let inner = Value::Object(ap_obj.clone());
                        for (k, vv) in v.as_object().unwrap() {
                            check_type(&inner, vv).map_err(|e| format!("entry {k}: {e}"))?;
                        }
                    }
                }
            }
            if tp == "array" {
                if let Some(items) = sub.get("items") {
                    for (i, vv) in v.as_array().unwrap().iter().enumerate() {
                        check_type(items, vv).map_err(|e| format!("item[{i}]: {e}"))?;
                    }
                }
            }
            if let Some(en) = sub.get("enum").and_then(|e| e.as_array()) {
                if !en.iter().any(|allowed| allowed == v) {
                    return Err(format!("value {v} not in enum {en:?}"));
                }
            }
            return Ok(());
        }
        if let Some(variants) = sub.get("oneOf").and_then(|o| o.as_array()) {
            let mut matched = 0;
            for variant in variants {
                if check_type(variant, v).is_ok() {
                    matched += 1;
                }
            }
            if matched == 0 {
                return Err(format!("value {v} matches none of oneOf"));
            }
            return Ok(());
        }
        Ok(())
    }

    fn type_matches(tp: &str, v: &Value) -> Result<(), String> {
        let ok = match tp {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            _ => true,
        };
        if ok {
            Ok(())
        } else {
            Err(format!("expected {tp}, got {v}"))
        }
    }

    #[test]
    fn schema_parses() {
        let _: Value = serde_json::from_str(SCHEMA).expect("schema is valid JSON");
    }

    #[test]
    fn sample_entry_validates_against_schema() {
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let entry = sample_entry();
        let v = serde_json::to_value(&entry).unwrap();
        validate(&schema, &v).expect("sample entry must conform to schema");
    }

    #[test]
    fn exited_entry_validates() {
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let mut entry = sample_entry();
        entry.state = "exited".into();
        entry.child_pid = None;
        entry.exit_code = Some(0);
        let v = serde_json::to_value(&entry).unwrap();
        validate(&schema, &v).expect("exited entry must conform");
    }

    #[test]
    fn bad_state_value_fails_validation() {
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let mut entry = sample_entry();
        entry.state = "frobnicating".into();
        let v = serde_json::to_value(&entry).unwrap();
        assert!(validate(&schema, &v).is_err());
    }

    #[test]
    fn missing_required_field_fails_validation() {
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let entry = sample_entry();
        let mut v = serde_json::to_value(&entry).unwrap();
        v.as_object_mut().unwrap().remove("id");
        assert!(validate(&schema, &v).is_err());
    }
}

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{get_state_dir, is_pid_alive, meta_path, pid_path};

/// The frozen-at-startup metadata for a managed daemon. Schema version is
/// bumped (`schema_version`) when fields are added/removed in a
/// backwards-incompatible way. Older readers must reject unrecognised versions
/// instead of silently misinterpreting fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
    pub id: String,
    /// Human-readable name. Optional; uniqueness is enforced per-project at
    /// spawn time.
    pub name: Option<String>,
    /// Canonicalised filesystem path that scopes this daemon. Defaults to
    /// the cwd at spawn time.
    pub project: String,
    /// Free-form key/value annotations. BTreeMap so on-disk order is stable.
    pub tags: BTreeMap<String, String>,
    /// Argv the daemon was asked to run. The first element is the program.
    pub cmd: Vec<String>,
    /// Cwd the child inherits (may differ from project).
    pub cwd: String,
    /// Seconds since the Unix epoch when the daemon process began.
    pub started_at: u64,
    /// PID that invoked the CLI (typically the user's shell). Useful for
    /// "what shell spawned this?" debugging.
    pub started_by_pid: Option<u32>,
    /// PID of the spawned child process. Recorded so `doctor` can detect
    /// orphaned children after a daemon is SIGKILLed.
    pub child_pid: Option<u32>,
    /// Path of the active log file (rotated segments are not listed here).
    pub log_path: String,
}

pub const SCHEMA_VERSION: u32 = 1;

impl Meta {
    pub fn write_atomically(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(path, &body)
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }
}

/// Atomic write: write to a sibling .tmp, then rename. POSIX guarantees the
/// rename within the same directory replaces atomically; if the process dies
/// mid-write the destination file is untouched.
pub fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let _parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent"))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)
}

/// Canonicalise a path for project comparison. Falls back to lexical
/// absolutisation (against cwd) when the path doesn't exist, so non-existent
/// paths still compare consistently with each other.
pub fn canonicalize_project(input: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(input) {
        return c;
    }
    if input.is_absolute() {
        return input.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(input),
        Err(_) => input.to_path_buf(),
    }
}

/// Walk the state dir, collect every active daemon's (id, meta) pair. A
/// daemon is "active" if its `.pid` sidecar exists and the pid is alive.
/// Daemons whose `.meta` has not yet been written are skipped (early-startup
/// race). Stale sidecar cleanup is intentionally NOT done here — that's
/// `list`'s job; callers like the name-uniqueness check shouldn't have
/// side-effects on disk.
pub fn list_active_metas() -> Vec<(String, Meta)> {
    let dir = get_state_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".pid") else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let pid: Option<u32> = fs::read_to_string(pid_path(id))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let Some(pid) = pid else { continue };
        if !is_pid_alive(pid) {
            continue;
        }
        let Ok(meta) = Meta::load(&meta_path(id)) else {
            continue;
        };
        out.push((id.to_string(), meta));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn meta_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.meta");
        let mut tags = BTreeMap::new();
        tags.insert("env".to_string(), "staging".to_string());
        let m = Meta {
            schema_version: SCHEMA_VERSION,
            id: "abc12345".to_string(),
            name: Some("dev".to_string()),
            project: "/a/proj".to_string(),
            tags,
            cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
            cwd: "/a/proj/sub".into(),
            started_at: 1_700_000_000,
            started_by_pid: Some(42),
            child_pid: Some(99),
            log_path: "/tmp/abc.log".into(),
        };
        m.write_atomically(&path).unwrap();
        let loaded = Meta::load(&path).unwrap();
        assert_eq!(loaded.id, m.id);
        assert_eq!(loaded.name, m.name);
        assert_eq!(loaded.tags.get("env").unwrap(), "staging");
        assert_eq!(loaded.cmd, m.cmd);
        assert_eq!(loaded.started_at, m.started_at);
    }

    #[test]
    fn write_atomic_leaves_no_tmp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("y.meta");
        write_atomic(&path, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":1}");

        // The tmp file should be renamed away, not left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover tmp files: {:?}",
            leftovers
        );
    }

    #[test]
    fn canonicalize_resolves_symlinks_when_possible() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("real");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let from_link = canonicalize_project(&link);
        let from_real = canonicalize_project(&target);
        assert_eq!(from_link, from_real);
    }

    #[test]
    fn canonicalize_nonexistent_path_is_absolute() {
        let p = std::path::Path::new("/this/does/not/exist/anywhere/12345");
        let c = canonicalize_project(p);
        assert!(c.is_absolute());
    }
}

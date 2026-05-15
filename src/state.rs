use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Sidecar suffixes the daemon writes and cleans up. The `.log` (and rotated
/// `.log.N`) files are deliberately excluded so post-mortem inspection of a
/// dead daemon's output remains possible.
pub const SIDECAR_SUFFIXES: &[&str] = &[".pid", ".version", ".cmd", ".sock", ".meta"];

/// Resolves the directory where daemon sidecars (.pid, .sock, .log, etc.) live.
///
/// Precedence:
///   1. `$AGENT_TERM_STATE_DIR` (explicit override)
///   2. `$XDG_RUNTIME_DIR/agent-term`
///   3. `~/.agent-term`
///   4. `$TMPDIR/agent-term` (last resort, when no home is resolvable)
pub fn get_state_dir() -> PathBuf {
    resolve_state_dir(dirs::home_dir)
}

fn resolve_state_dir<F>(home_dir: F) -> PathBuf
where
    F: FnOnce() -> Option<PathBuf>,
{
    if let Ok(dir) = env::var("AGENT_TERM_STATE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }

    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        if !runtime_dir.is_empty() {
            return PathBuf::from(runtime_dir).join("agent-term");
        }
    }

    if let Some(home) = home_dir() {
        return home.join(".agent-term");
    }

    env::temp_dir().join("agent-term")
}

pub fn pid_path(id: &str) -> PathBuf {
    get_state_dir().join(format!("{id}.pid"))
}

pub fn version_path(id: &str) -> PathBuf {
    get_state_dir().join(format!("{id}.version"))
}

pub fn cmd_path(id: &str) -> PathBuf {
    get_state_dir().join(format!("{id}.cmd"))
}

pub fn sock_path(id: &str) -> PathBuf {
    get_state_dir().join(format!("{id}.sock"))
}

pub fn meta_path(id: &str) -> PathBuf {
    get_state_dir().join(format!("{id}.meta"))
}

pub fn log_path(id: &str) -> PathBuf {
    get_state_dir().join(format!("{id}.log"))
}

/// Per-state-dir JSONL log of recent daemon lifetimes. Doctor reads this to
/// surface the "spawning many short-lived daemons" misuse heuristic.
pub fn recent_log_path() -> PathBuf {
    get_state_dir().join("recent.jsonl")
}

/// Remove every known sidecar for a given id from the state directory.
/// Missing files are not an error. Files with unrelated suffixes are left alone.
pub fn cleanup_stale_files(id: &str) {
    cleanup_stale_files_in(&get_state_dir(), id);
}

fn cleanup_stale_files_in(dir: &Path, id: &str) {
    for suffix in SIDECAR_SUFFIXES {
        let _ = fs::remove_file(dir.join(format!("{id}{suffix}")));
    }
}

/// Returns whether a process with the given PID is currently alive.
///
/// EPERM (process exists but we cannot signal it) counts as alive so a daemon
/// owned by a different uid is not mis-cleaned. Only ESRCH ("no such process")
/// is treated as dead.
#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

#[cfg(not(unix))]
pub fn is_pid_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct EnvGuard(Vec<(&'static str, Option<String>)>);

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            Self(keys.iter().map(|k| (*k, env::var(k).ok())).collect())
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(val) => env::set_var(k, val),
                    None => env::remove_var(k),
                }
            }
        }
    }

    fn clear_state_env() {
        env::remove_var("AGENT_TERM_STATE_DIR");
        env::remove_var("XDG_RUNTIME_DIR");
    }

    fn fake_home(path: &str) -> impl FnOnce() -> Option<PathBuf> + '_ {
        move || Some(PathBuf::from(path))
    }

    fn no_home() -> impl FnOnce() -> Option<PathBuf> {
        || None
    }

    #[test]
    fn explicit_override_wins() {
        let _lock = lock_env();
        let _g = EnvGuard::capture(&["AGENT_TERM_STATE_DIR", "XDG_RUNTIME_DIR"]);
        clear_state_env();
        env::set_var("AGENT_TERM_STATE_DIR", "/tmp/explicit-override");
        env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");

        assert_eq!(
            resolve_state_dir(fake_home("/home/user")),
            PathBuf::from("/tmp/explicit-override")
        );
    }

    #[test]
    fn empty_override_falls_through() {
        let _lock = lock_env();
        let _g = EnvGuard::capture(&["AGENT_TERM_STATE_DIR", "XDG_RUNTIME_DIR"]);
        clear_state_env();
        env::set_var("AGENT_TERM_STATE_DIR", "");
        env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");

        assert_eq!(
            resolve_state_dir(fake_home("/home/user")),
            PathBuf::from("/run/user/1000/agent-term")
        );
    }

    #[test]
    fn xdg_runtime_dir_when_no_override() {
        let _lock = lock_env();
        let _g = EnvGuard::capture(&["AGENT_TERM_STATE_DIR", "XDG_RUNTIME_DIR"]);
        clear_state_env();
        env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");

        assert_eq!(
            resolve_state_dir(fake_home("/home/user")),
            PathBuf::from("/run/user/1000/agent-term")
        );
    }

    #[test]
    fn home_fallback_when_no_xdg() {
        let _lock = lock_env();
        let _g = EnvGuard::capture(&["AGENT_TERM_STATE_DIR", "XDG_RUNTIME_DIR"]);
        clear_state_env();

        assert_eq!(
            resolve_state_dir(fake_home("/home/agentuser")),
            PathBuf::from("/home/agentuser/.agent-term")
        );
    }

    #[test]
    fn temp_dir_last_resort() {
        let _lock = lock_env();
        let _g = EnvGuard::capture(&["AGENT_TERM_STATE_DIR", "XDG_RUNTIME_DIR"]);
        clear_state_env();

        assert_eq!(
            resolve_state_dir(no_home()),
            env::temp_dir().join("agent-term")
        );
    }

    #[test]
    fn cleanup_removes_every_known_sidecar() {
        let dir = TempDir::new().unwrap();
        let id = "abcd1234";

        for suffix in SIDECAR_SUFFIXES {
            fs::write(dir.path().join(format!("{id}{suffix}")), b"x").unwrap();
        }

        cleanup_stale_files_in(dir.path(), id);

        for suffix in SIDECAR_SUFFIXES {
            assert!(
                !dir.path().join(format!("{id}{suffix}")).exists(),
                "{id}{suffix} should have been removed"
            );
        }
    }

    #[test]
    fn cleanup_leaves_unrelated_files_alone() {
        let dir = TempDir::new().unwrap();
        let id = "deadbeef";

        // Sidecars for our id
        fs::write(dir.path().join(format!("{id}.pid")), b"1").unwrap();
        fs::write(dir.path().join(format!("{id}.sock")), b"").unwrap();

        // Files belonging to other ids or unrelated tools
        fs::write(dir.path().join("other-id.pid"), b"2").unwrap();
        fs::write(dir.path().join(format!("{id}.unrelated")), b"keep").unwrap();
        fs::write(dir.path().join("random.txt"), b"keep").unwrap();
        fs::write(dir.path().join(format!("{id}_pid")), b"keep").unwrap(); // no dot

        cleanup_stale_files_in(dir.path(), id);

        assert!(!dir.path().join(format!("{id}.pid")).exists());
        assert!(!dir.path().join(format!("{id}.sock")).exists());
        assert!(dir.path().join("other-id.pid").exists());
        assert!(dir.path().join(format!("{id}.unrelated")).exists());
        assert!(dir.path().join("random.txt").exists());
        assert!(dir.path().join(format!("{id}_pid")).exists());
    }
}

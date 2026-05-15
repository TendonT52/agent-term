use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Newline-delimited JSON request sent from CLI to daemon over the unix socket.
#[derive(Serialize, Deserialize, Debug)]
pub struct Request {
    pub action: String,
    /// For `signal`: signal name e.g. "TERM", "INT", "HUP", "KILL", "USR1", "USR2".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// Newline-delimited JSON response from daemon to CLI.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Response {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

/// Translate a signal name ("TERM", "INT", "HUP", ...) to its numeric value.
/// Returns None for unrecognized names.
#[cfg(unix)]
pub fn parse_signal(name: &str) -> Option<i32> {
    let upper = name.trim().to_ascii_uppercase();
    let s = upper.strip_prefix("SIG").unwrap_or(&upper);
    Some(match s {
        "TERM" => libc::SIGTERM,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "KILL" => libc::SIGKILL,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "QUIT" => libc::SIGQUIT,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_signal_handles_common_names() {
        assert_eq!(parse_signal("TERM"), Some(libc::SIGTERM));
        assert_eq!(parse_signal("term"), Some(libc::SIGTERM));
        assert_eq!(parse_signal("SIGTERM"), Some(libc::SIGTERM));
        assert_eq!(parse_signal("HUP"), Some(libc::SIGHUP));
        assert_eq!(parse_signal("KILL"), Some(libc::SIGKILL));
        assert_eq!(parse_signal("bogus"), None);
    }
}

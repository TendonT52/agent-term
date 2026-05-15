use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// Generate a short, opaque, URL-safe identifier (12 lowercase hex chars).
///
/// Sources 6 bytes from `/dev/urandom`. Falls back to a time/pid mix if that
/// fails — which only happens on heavily sandboxed systems where /dev/urandom
/// is unavailable.
pub fn gen_id() -> String {
    let mut buf = [0u8; 6];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        return hex(&buf);
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    let mixed = nanos ^ (std::process::id() as u64);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((mixed >> (i * 8)) & 0xff) as u8;
    }
    hex(&buf)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// IDs accepted from --id or env: lowercase alphanumeric plus `-` and `_`,
/// length 1..=64. Restrictive enough to be safe as a filename component on
/// every OS without escaping.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen_id_is_12_hex_chars() {
        for _ in 0..16 {
            let id = gen_id();
            assert_eq!(id.len(), 12);
            assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
    }

    #[test]
    fn gen_id_is_unique_enough() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1024 {
            assert!(seen.insert(gen_id()), "duplicate id within 1024 calls");
        }
    }

    #[test]
    fn validates_ids() {
        assert!(is_valid_id("abcd1234"));
        assert!(is_valid_id("a"));
        assert!(is_valid_id("a-b_c"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("has space"));
        assert!(!is_valid_id("../escape"));
        assert!(!is_valid_id(&"x".repeat(65)));
    }
}

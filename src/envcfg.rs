//! Load a `KEY=value` env file into the process environment at startup, so a
//! renderer (orbit-kms) started from this environment inherits it.
//!
//! Existing environment variables are never overwritten -- the shell, the
//! systemd unit, or `env VAR=... orbit-rs` still wins over the file.

use std::ffi::CString;
use std::path::PathBuf;

use crate::common::log;

/// Search order, first existing file wins:
///   $ORBIT_ENV_FILE                 explicit path ("none" or "" => skip loading)
///   /data/orbit/config/orbit.env    the appliance's real, live config
///   /data/orbit/config/orbit-rs.env (what orbit-web writes to)
///   ./config/orbit.env              dev convenience: repo checkout as CWD
///   ./config/orbit-rs.env
///   <exe dir>/config/orbit.env      (and orbit-rs.env)
///   <exe dir>/../config/orbit.env   (and orbit-rs.env)
///   /etc/orbit-rs/orbit.env         (and orbit-rs.env)
pub fn load() {
    let path = match std::env::var_os("ORBIT_ENV_FILE") {
        Some(v) => {
            let s = v.to_string_lossy().into_owned();
            if s.is_empty() || s == "none" {
                return;
            }
            let p = PathBuf::from(&s);
            if !p.is_file() {
                log(format!("env file: ORBIT_ENV_FILE={} not found", s));
                return;
            }
            p
        }
        None => match find_default() {
            Some(p) => p,
            None => {
                log("env file: none found (checked /data/orbit/config, ./config, <exe dir>/config, /etc/orbit-rs)");
                return;
            }
        },
    };

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            log(format!("env file: cannot read {}: {}", path.display(), e));
            return;
        }
    };

    let mut applied: Vec<String> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r').trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let eq = match line.find('=') {
            Some(i) => i,
            None => {
                log(format!(
                    "env file: {}:{}: no '=', line skipped",
                    path.display(),
                    n + 1
                ));
                continue;
            }
        };
        let key = line[..eq].trim();
        if !valid_key(key) {
            log(format!(
                "env file: {}:{}: bad key '{}', skipped",
                path.display(),
                n + 1,
                key
            ));
            continue;
        }
        let val = parse_value(&line[eq + 1..]);
        if set_if_absent(key, &val) {
            applied.push(key.to_string());
        }
    }

    if applied.is_empty() {
        log(format!(
            "env file: {} (all keys already set in the environment)",
            path.display()
        ));
    } else {
        log(format!(
            "env file: {} -> {}",
            path.display(),
            applied.join(", ")
        ));
    }
}

fn find_default() -> Option<PathBuf> {
    let mut cands = vec![
        PathBuf::from("/data/orbit/config/orbit.env"),
        PathBuf::from("/data/orbit/config/orbit-rs.env"),
        PathBuf::from("config/orbit.env"),
        PathBuf::from("config/orbit-rs.env"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("config/orbit.env"));
            cands.push(dir.join("config/orbit-rs.env"));
            if let Some(up) = dir.parent() {
                cands.push(up.join("config/orbit.env"));
                cands.push(up.join("config/orbit-rs.env"));
            }
        }
    }
    cands.push(PathBuf::from("/etc/orbit-rs/orbit.env"));
    cands.push(PathBuf::from("/etc/orbit-rs/orbit-rs.env"));
    cands.into_iter().find(|p| p.is_file())
}

fn valid_key(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty()
        && (b[0] == b'_' || b[0].is_ascii_alphabetic())
        && b.iter().all(|&c| c == b'_' || c.is_ascii_alphanumeric())
}

/// Trim, strip one layer of matching quotes, or drop an unquoted trailing
/// ` #...` / `\t#...` comment (matches the reference file's inline comments).
fn parse_value(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        return s[1..s.len() - 1].to_string();
    }
    let mut cut = s.len();
    for i in 1..b.len() {
        if b[i] == b'#' && (b[i - 1] == b' ' || b[i - 1] == b'\t') {
            cut = i;
            break;
        }
    }
    s[..cut].trim_end().to_string()
}

/// `setenv(key, val, 0)` -- only writes when `key` is absent. Returns whether
/// it was actually set now.
fn set_if_absent(key: &str, val: &str) -> bool {
    if std::env::var_os(key).is_some() {
        return false;
    }
    let (ck, cv) = match (CString::new(key), CString::new(val)) {
        (Ok(k), Ok(v)) => (k, v),
        _ => return false, // embedded NUL -- ignore the line
    };
    // Runs single-threaded at startup, before any Worker is spawned.
    unsafe { libc::setenv(ck.as_ptr(), cv.as_ptr(), 0) == 0 }
}

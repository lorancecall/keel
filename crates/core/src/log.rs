//! Append-only text logs: one timestamped line per call, echoed to stderr, rotated once to
//! `<name>.log.1` past a size limit. The service writes `%ProgramData%\keel\service.log`; user-side
//! processes (tray, CLI) cannot, so they write `%LOCALAPPDATA%\keel\tray.log`. Each feature tags
//! its lines (`ram: ...`) so one log serves all of them.
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_BYTES: u64 = 2 << 20;

pub fn service_log_path() -> PathBuf { crate::paths::data_dir().join("service.log") }
pub fn user_log_path() -> PathBuf { crate::paths::user_data_dir().join("tray.log") }

/// The line as written: local time, then the message.
pub fn line(msg: &str) -> String { format!("{} {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")) }

/// Appends one line to `path` (never fails; logging must not take a feature down).
pub fn append(path: &Path, max_bytes: u64, msg: &str) {
    let line = line(msg);
    // Echo for consoles; never `eprintln!`, which panics when stderr is unusable (a
    // windows-subsystem exe started by Explorer or Setup has none) and the panic hook logs again.
    let _ = writeln!(std::io::stderr(), "{line}");
    if let Some(d) = path.parent() { let _ = std::fs::create_dir_all(d); }
    if std::fs::metadata(path).map(|m| m.len() > max_bytes).unwrap_or(false) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(path, old);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) { let _ = writeln!(f, "{line}"); }
}

/// A line in the service log.
pub fn service(msg: impl AsRef<str>) { append(&service_log_path(), MAX_BYTES, msg.as_ref()); }
/// A line in the user-side log.
pub fn user(msg: impl AsRef<str>) { append(&user_log_path(), MAX_BYTES, msg.as_ref()); }

#[cfg(test)]
mod tests {
    #[test]
    fn appends_and_rotates() {
        let dir = std::env::temp_dir().join(format!("keel-log-test-{}", std::process::id()));
        let p = dir.join("t.log");
        super::append(&p, 40, "first");
        super::append(&p, 40, "second");
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 2);
        super::append(&p, 40, "third");
        assert!(dir.join("t.log.1").exists());
        assert!(std::fs::read_to_string(&p).unwrap().ends_with("third\n"));
        let _ = std::fs::remove_dir_all(dir);
    }
}

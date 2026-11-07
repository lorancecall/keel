//! Paths of the eGPU feature (in Keel's data folders), its log lines and alerts.
use std::fs;
use std::path::PathBuf;

/// AppUserModelID of the Start-menu shortcut (installer); notifications show as "Keel".
pub const APP_ID: &str = "lorancecall.Keel";

/// `%ProgramData%\keel` (or `KEEL_DATA`): written by the service / admin commands only.
pub fn data_dir() -> PathBuf { keel_core::paths::data_dir() }
/// `%LOCALAPPDATA%\keel`: per-user records (integer scaling, AMD scaling).
pub fn user_dir() -> PathBuf { keel_core::paths::user_data_dir() }

pub fn config_path() -> PathBuf { data_dir().join("egpu.toml") }
pub fn status_path() -> PathBuf { data_dir().join("egpu-status.json") }
pub fn log_path() -> PathBuf { keel_core::log::service_log_path() }

/// A line tagged `egpu:` in the service log, or in the user log when this process may not create
/// the service's folder: a user process must never create `%ProgramData%\keel` (the service relies on users being unable to write there).
pub fn log(msg: impl AsRef<str>) {
    let m = format!("egpu: {}", msg.as_ref());
    if data_dir().is_dir() || keel_core::paths::may_write_data_dir() { keel_core::log::service(m) }
    else { keel_core::log::user(m) }
}

/// Reads a text file that may be UTF-16 (INF files usually are) or UTF-8.
pub fn read_text_any(path: &std::path::Path) -> std::io::Result<String> {
    let b = fs::read(path)?;
    if b.len() >= 2 && b[0] == 0xFF && b[1] == 0xFE {
        let u: Vec<u16> = b[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return Ok(String::from_utf16_lossy(&u));
    }
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// A user-facing event (the tray shows new ones as notifications). Also logged.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Alert {
    pub id: u64,
    pub utc: String,
    pub msg: String,
}

static ALERTS: std::sync::Mutex<Vec<Alert>> = std::sync::Mutex::new(Vec::new());

pub fn alert(msg: impl AsRef<str>) {
    log(msg.as_ref());
    let mut a = ALERTS.lock().unwrap();
    let id = a.last().map(|x| x.id + 1).unwrap_or(1);
    a.push(Alert { id, utc: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(), msg: msg.as_ref().to_string() });
    let n = a.len();
    if n > 10 { a.drain(..n - 10); }
}

/// The last (up to) 10 alerts, oldest first.
pub fn recent_alerts() -> Vec<Alert> { ALERTS.lock().unwrap().clone() }

/// True in session 0, i.e. inside the Keel service (the console `keel egpu run` is never there).
pub fn in_service() -> bool {
    let mut s = 0u32;
    unsafe { windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut s).is_ok() && s == 0 }
}

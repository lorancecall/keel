//! Duplicate tray-icon entries (Settings > Personalization > Taskbar > Other system tray icons).
//! Windows keys each entry by the icon GUID an app registers; an app that makes a new GUID on
//! every start (e.g. AMD's `dgtrayicon.exe`) adds one entry per start.
//! Pruning keeps one entry per app (the one shown on the taskbar, else the newest) after a
//! `reg export` backup. HKCU only, no admin.
use std::path::PathBuf;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const KEY: &str = r"Control Panel\NotifyIconSettings";
/// An app with at least this many entries is pruned (normal apps have 1-5).
pub const MIN_DUPLICATES: usize = 10;
const KEEP_BACKUPS: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    pub exe: String,
    pub promoted: bool,
    /// Registry last-write time, comparable (yyyymmddhhmmss).
    pub written: u64,
}

/// Keys to delete: for every exe with >= `min` entries, all but one (taskbar-promoted first,
/// then the most recently written). Pure.
pub fn stale(entries: &[Entry], min: usize) -> Vec<String> {
    let mut by: std::collections::BTreeMap<String, Vec<&Entry>> = Default::default();
    for e in entries { by.entry(e.exe.to_lowercase()).or_default().push(e); }
    let mut out = Vec::new();
    for (_, mut v) in by.into_iter().filter(|(_, v)| v.len() >= min) {
        v.sort_by(|a, b| (b.promoted, b.written).cmp(&(a.promoted, a.written)));
        out.extend(v.iter().skip(1).map(|e| e.key.clone()));
    }
    out
}

pub fn entries() -> Vec<Entry> {
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(KEY, KEY_READ) else { return vec![] };
    root.enum_keys().flatten().filter_map(|k| {
        let s = root.open_subkey_with_flags(&k, KEY_READ).ok()?;
        let exe: String = s.get_value("ExecutablePath").ok()?;
        let promoted = s.get_value::<u32, _>("IsPromoted").map(|v| v != 0).unwrap_or(false);
        let t = s.query_info().ok()?.get_last_write_time_system();
        let written = [t.wYear as u64, t.wMonth as u64, t.wDay as u64, t.wHour as u64, t.wMinute as u64, t.wSecond as u64]
            .iter().fold(0u64, |a, x| a * 100 + x);
        Some(Entry { key: k, exe, promoted, written })
    }).collect()
}

fn backup_dir() -> PathBuf {
    crate::paths::user_data_dir().join("tray-backup")
}

/// Backs the whole key up, then deletes the stale entries. Returns (deleted, backup file).
pub fn prune() -> anyhow::Result<(usize, Option<PathBuf>)> {
    let stale = stale(&entries(), MIN_DUPLICATES);
    if stale.is_empty() { return Ok((0, None)); }
    let dir = backup_dir();
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("NotifyIconSettings-{}.reg", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let o = crate::cmd::hidden("reg.exe").arg("export").arg(format!(r"HKCU\{KEY}")).arg(&file).arg("/y").output()?;
    if !o.status.success() { anyhow::bail!("backup failed, nothing deleted: {}", String::from_utf8_lossy(&o.stderr).trim()); }
    let mut old: Vec<PathBuf> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "reg")).collect();
    old.sort();
    for p in old.iter().take(old.len().saturating_sub(KEEP_BACKUPS)) { let _ = std::fs::remove_file(p); }
    let root = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(KEY, KEY_READ | KEY_WRITE)?;
    let mut n = 0;
    for k in &stale { if root.delete_subkey_all(k).is_ok() { n += 1; } }
    Ok((n, Some(file)))
}

/// Per exe: entry count, largest first.
pub fn counts() -> Vec<(String, usize)> {
    let mut m: std::collections::BTreeMap<String, usize> = Default::default();
    for e in entries() { *m.entry(e.exe).or_default() += 1; }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(key: &str, exe: &str, promoted: bool, written: u64) -> Entry { Entry { key: key.into(), exe: exe.into(), promoted, written } }

    #[test]
    fn keeps_one_per_noisy_app_and_leaves_normal_apps() {
        let mut v: Vec<Entry> = (0..12).map(|i| e(&format!("d{i}"), r"{SYS}\dgtrayicon.exe", false, 100 + i)).collect();
        v.push(e("s1", r"C:\slack.exe", false, 1));
        v.push(e("s2", r"C:\Slack.exe", false, 2));
        let s = stale(&v, 10);
        assert_eq!(s.len(), 11);
        assert!(!s.contains(&"d11".to_string()), "newest kept");
        assert!(!s.iter().any(|k| k.starts_with('s')), "apps with few entries untouched");
        v[3].promoted = true;
        let s = stale(&v, 10);
        assert!(!s.contains(&"d3".to_string()) && s.contains(&"d11".to_string()), "the one shown on the taskbar wins");
    }
}

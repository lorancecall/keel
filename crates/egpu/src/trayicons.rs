//! Duplicate tray-icon settings of AMD's dgtrayicon.exe (tray, user session, HKCU only).
//! dgtrayicon gets a new icon GUID on every start (boot, driver restart, eGPU attach / detach), so
//! Windows adds one `HKCU\Control Panel\NotifyIconSettings` subkey per start (well over a
//! hundred after a few weeks). We keep one entry and delete the rest, after a `reg export` backup.
use std::path::{Path, PathBuf};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const KEY: &str = r"Control Panel\NotifyIconSettings";
const EXE: &str = "dgtrayicon.exe";
/// Prune only above this many entries; a few are harmless.
pub const MAX_ENTRIES: usize = 3;
pub const KEEP_BACKUPS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub exe: String,
    pub promoted: bool,
    /// Registry last-write time (FILETIME as u64).
    pub written: u64,
}

fn is_dgtray(exe: &str) -> bool {
    let e = exe.to_ascii_lowercase();
    e == EXE || e.ends_with(&format!("\\{EXE}"))
}

/// Keys to delete: none at or below MAX_ENTRIES dgtrayicon entries; otherwise all but one, keeping
/// a promoted (shown on the taskbar) entry first, else the newest.
pub fn select_prune(entries: &[Entry]) -> Vec<String> {
    let mut mine: Vec<&Entry> = entries.iter().filter(|e| is_dgtray(&e.exe)).collect();
    if mine.len() <= MAX_ENTRIES { return Vec::new(); }
    mine.sort_by_key(|e| (e.promoted, e.written));
    mine.pop();
    mine.into_iter().map(|e| e.key.clone()).collect()
}

/// Old backups to delete (names sort by time: `tray-icons-YYYYmmdd-HHMMSS.reg`), keeping KEEP_BACKUPS.
pub fn old_backups(mut names: Vec<String>) -> Vec<String> {
    names.retain(|n| n.starts_with("tray-icons-") && n.ends_with(".reg"));
    names.sort();
    let n = names.len().saturating_sub(KEEP_BACKUPS);
    names.truncate(n);
    names
}

pub fn read_entries() -> std::io::Result<Vec<Entry>> { read_entries_in(KEY) }

fn read_entries_in(key: &str) -> std::io::Result<Vec<Entry>> {
    let root = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(key, KEY_READ)?;
    let mut out = Vec::new();
    for name in root.enum_keys().flatten() {
        let Ok(k) = root.open_subkey_with_flags(&name, KEY_READ) else { continue };
        let exe: String = k.get_value("ExecutablePath").unwrap_or_default();
        let promoted = k.get_value::<u32, _>("IsPromoted").map(|v| v != 0).unwrap_or(false);
        let written = k.query_info().map(|i| ((i.last_write_time.dwHighDateTime as u64) << 32) | i.last_write_time.dwLowDateTime as u64).unwrap_or(0);
        out.push(Entry { key: name, exe, promoted, written });
    }
    Ok(out)
}

fn backup(key: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join(format!("tray-icons-{}.reg", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let st = keel_core::cmd::hidden("reg.exe")
        .args(["export", &format!(r"HKCU\{key}"), &file.to_string_lossy(), "/y"])
        .output()?;
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    if !st.status.success() || size == 0 {
        anyhow::bail!("reg export failed ({}, {size} bytes)", st.status);
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        let names = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        for n in old_backups(names) { let _ = std::fs::remove_file(dir.join(n)); }
    }
    Ok(file)
}

/// One pass. Returns log lines (nothing when there is nothing to do).
pub fn prune(backup_dir: &Path) -> Vec<String> { prune_in(KEY, backup_dir) }

fn prune_in(key: &str, backup_dir: &Path) -> Vec<String> {
    let entries = match read_entries_in(key) {
        Ok(e) => e,
        Err(e) => return vec![format!("tray icons: cannot read HKCU\\{key}: {e}")],
    };
    let doomed = select_prune(&entries);
    if doomed.is_empty() { return Vec::new(); }
    let file = match backup(key, backup_dir) {
        Ok(f) => f,
        Err(e) => return vec![format!("tray icons: backup failed, nothing deleted: {e}")],
    };
    let root = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(key, KEY_READ | KEY_WRITE) {
        Ok(r) => r,
        Err(e) => return vec![format!("tray icons: cannot open HKCU\\{key} for writing: {e}")],
    };
    let mut gone = 0;
    let mut out = Vec::new();
    for k in &doomed {
        match root.delete_subkey_all(k) {
            Ok(()) => gone += 1,
            Err(e) => out.push(format!("tray icons: deleting {k} failed: {e}")),
        }
    }
    let total = entries.iter().filter(|e| is_dgtray(&e.exe)).count();
    out.insert(0, format!("tray icons: {EXE} had {total} entries; removed {gone}, kept 1 (backup {})", file.display()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(key: &str, exe: &str, promoted: bool, written: u64) -> Entry {
        Entry { key: key.into(), exe: exe.into(), promoted, written }
    }
    const DG: &str = r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\dgtrayicon.exe";

    #[test]
    fn few_entries_are_left_alone() {
        let v = vec![e("a", DG, false, 1), e("b", DG, false, 2), e("c", DG, false, 3), e("x", r"C:\x\other.exe", false, 9)];
        assert!(select_prune(&v).is_empty());
    }

    #[test]
    fn keeps_newest_and_ignores_other_apps() {
        let v = vec![e("a", DG, false, 1), e("b", DG, false, 5), e("c", DG, false, 3), e("d", DG, false, 2),
                     e("x", r"C:\x\notdgtrayicon.exe", false, 9)];
        let mut d = select_prune(&v);
        d.sort();
        assert_eq!(d, vec!["a", "c", "d"]);
    }

    #[test]
    fn promoted_wins_over_newer() {
        let v = vec![e("a", DG, false, 9), e("b", &DG.to_uppercase(), true, 1), e("c", DG, false, 3), e("d", "dgtrayicon.exe", false, 2)];
        let mut d = select_prune(&v);
        d.sort();
        assert_eq!(d, vec!["a", "c", "d"]);
    }

    #[test]
    fn backup_rotation_keeps_last_five() {
        let names: Vec<String> = (1..=7).map(|i| format!("tray-icons-2026110{i}-120000.reg")).chain(["other.txt".to_string()]).collect();
        assert_eq!(old_backups(names), vec!["tray-icons-20261101-120000.reg", "tray-icons-20261102-120000.reg"]);
        assert!(old_backups(vec!["tray-icons-20261101-120000.reg".into()]).is_empty());
    }

    /// Real registry + reg.exe on a scratch key under HKCU\Software: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn prune_scratch_key() {
        let base = r"Software\keel-egpu-test";
        let key = r"Software\keel-egpu-test\NotifyIconSettings";
        let hk = RegKey::predef(HKEY_CURRENT_USER);
        let _ = hk.delete_subkey_all(base);
        for i in 0..5 {
            let (k, _) = hk.create_subkey(format!(r"{key}\{i}")).unwrap();
            k.set_value("ExecutablePath", &DG.to_string()).unwrap();
            if i == 1 { k.set_value("IsPromoted", &1u32).unwrap(); }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (k, _) = hk.create_subkey(format!(r"{key}\other")).unwrap();
        k.set_value("ExecutablePath", &r"C:\x\other.exe".to_string()).unwrap();
        let dir = std::env::temp_dir().join("keel-egpu-trayicons-test");
        let log = prune_in(key, &dir);
        println!("{log:?}");
        let mut left: Vec<String> = read_entries_in(key).unwrap().into_iter().map(|e| e.key).collect();
        left.sort();
        let backups = std::fs::read_dir(&dir).unwrap().count();
        let _ = hk.delete_subkey_all(base);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(left, vec!["1", "other"]);
        assert!(log[0].contains("removed 4"), "{log:?}");
        assert_eq!(backups, 1);
    }
}

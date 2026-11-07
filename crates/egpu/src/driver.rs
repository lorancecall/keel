//! Per-device driver management (admin): install a driver INF only on devices with one exact
//! hardware ID, and keep Windows Update from replacing it (device-install deny policy).
use crate::util::{data_dir, log, read_text_any};
use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use windows::core::HSTRING;
use windows::Win32::Devices::DeviceAndDriverInstallation::{UpdateDriverForPlugAndPlayDevicesW, INSTALLFLAG_FORCE, UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS};
use winreg::RegKey;

const POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\DeviceInstall\Restrictions";

fn policy_key() -> Result<RegKey> {
    let (k, _) = RegKey::predef(HKEY_LOCAL_MACHINE).create_subkey(POLICY).context("open device-install policy (admin needed)")?;
    Ok(k)
}

/// Value names under DenyDeviceIDs that hold `hwid`.
fn blocked_entries(hwid: &str) -> Vec<String> {
    let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(format!(r"{POLICY}\DenyDeviceIDs")) else { return vec![] };
    k.enum_values().flatten()
        .filter(|(_, v)| v.to_string().trim_matches('"').eq_ignore_ascii_case(hwid))
        .map(|(n, _)| n).collect()
}

pub fn is_blocked(hwid: &str) -> bool { !blocked_entries(hwid).is_empty() }

pub fn block(hwid: &str) -> Result<()> {
    if is_blocked(hwid) { return Ok(()); }
    let root = policy_key()?;
    root.set_value("DenyDeviceIDs", &1u32)?;
    root.set_value("DenyDeviceIDsRetroactive", &0u32)?;
    let (list, _) = root.create_subkey_with_flags("DenyDeviceIDs", KEY_ALL_ACCESS)?;
    let next = list.enum_values().flatten().filter_map(|(n, _)| n.parse::<u32>().ok()).max().unwrap_or(0) + 1;
    list.set_value(next.to_string(), &hwid.to_string())?;
    log(format!("driver: Windows Update blocked for {hwid}"));
    Ok(())
}

pub fn unblock(hwid: &str) -> Result<()> {
    let names = blocked_entries(hwid);
    if names.is_empty() { return Ok(()); }
    let root = policy_key()?;
    let (list, _) = root.create_subkey_with_flags("DenyDeviceIDs", KEY_ALL_ACCESS)?;
    for n in names { list.delete_value(&n)?; }
    log(format!("driver: Windows Update unblocked for {hwid}"));
    Ok(())
}

fn inf_lists(inf: &Path, hwid: &str) -> bool {
    read_text_any(inf).map(|t| t.to_ascii_uppercase().contains(&hwid.to_ascii_uppercase())).unwrap_or(false)
}

fn find_inf_in_dir(dir: &Path, hwid: &str) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p); }
            else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("inf")) && inf_lists(&p, hwid) { return Some(p); }
        }
    }
    None
}

/// A reader that starts at `offset` (the 7z archive inside a self-extracting .exe).
struct Offset<R> { inner: R, offset: u64 }
impl<R: Read> Read for Offset<R> { fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> { self.inner.read(b) } }
impl<R: Seek> Seek for Offset<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let p = match pos { SeekFrom::Start(n) => self.inner.seek(SeekFrom::Start(n + self.offset))?, other => self.inner.seek(other)? };
        Ok(p.saturating_sub(self.offset))
    }
}

fn sevenz_offset(path: &Path) -> Result<u64> {
    const SIG: &[u8] = b"7z\xBC\xAF\x27\x1C";
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 64 << 20];
    let n = f.read(&mut buf)?;
    buf[..n].windows(SIG.len()).position(|w| w == SIG).map(|p| p as u64)
        .ok_or_else(|| anyhow!("{} has no embedded 7z archive", path.display()))
}

fn open_archive(path: &Path) -> Result<sevenz_rust2::ArchiveReader<Offset<std::fs::File>>> {
    let offset = sevenz_offset(path)?;
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    Ok(sevenz_rust2::ArchiveReader::new(Offset { inner: f, offset }, sevenz_rust2::Password::empty())?)
}

/// Extracts the display-driver folder that lists `hwid` from a vendor installer (.exe / .7z).
/// Pass 1 writes only *.inf files to find the right folder; pass 2 writes that folder.
fn extract_driver(installer: &Path, hwid: &str) -> Result<PathBuf> {
    let stem = installer.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let out = data_dir().join("driver-cache").join(&stem);
    if let Some(inf) = out.is_dir().then(|| find_inf_in_dir(&out, hwid)).flatten() { return Ok(inf); }
    log(format!("driver: scanning {} (this takes a few minutes)", installer.display()));
    let infs = out.join("_infs");
    let mut ar = open_archive(installer)?;
    ar.for_each_entries(|e, r| {
        if e.is_directory() || !e.name().to_ascii_lowercase().ends_with(".inf") { std::io::copy(r, &mut std::io::sink())?; return Ok(true); }
        let p = infs.join(e.name().replace('/', "\\"));
        if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
        std::io::copy(r, &mut std::fs::File::create(&p)?)?;
        Ok(true)
    })?;
    let found = find_inf_in_dir(&infs, hwid).ok_or_else(|| anyhow!("no INF in the package lists {hwid}"))?;
    let rel_dir = found.parent().unwrap().strip_prefix(&infs)?.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    log(format!("driver: {rel_dir} lists {hwid}; extracting that folder"));
    let mut ar = open_archive(installer)?;
    ar.for_each_entries(|e, r| {
        let name = e.name().replace('\\', "/");
        if e.is_directory() || !name.to_ascii_lowercase().starts_with(&format!("{rel_dir}/")) { std::io::copy(r, &mut std::io::sink())?; return Ok(true); }
        let p = out.join(name.replace('/', "\\"));
        if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
        std::io::copy(r, &mut std::fs::File::create(&p)?)?;
        Ok(true)
    })?;
    let _ = std::fs::remove_dir_all(&infs);
    find_inf_in_dir(&out, hwid).ok_or_else(|| anyhow!("extraction finished but the INF is missing"))
}

/// Resolves an installer / folder / .inf to the INF that lists `hwid`.
pub fn locate_inf(src: &Path, hwid: &str) -> Result<PathBuf> {
    if src.is_dir() { return find_inf_in_dir(src, hwid).ok_or_else(|| anyhow!("no INF under {} lists {hwid}", src.display())); }
    match src.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).as_deref() {
        Some("inf") if inf_lists(src, hwid) => Ok(src.to_path_buf()),
        Some("inf") => bail!("{} does not list {hwid}", src.display()),
        Some("exe") | Some("7z") => extract_driver(src, hwid),
        _ => bail!("expected a driver installer (.exe), a folder, or an .inf"),
    }
}

/// Runs a Windows PowerShell 5.1 script and returns stdout. PSModulePath is cleared: started
/// from PowerShell 7, the inherited 7.x module path stops 5.1 from loading its own modules
/// (Get-AuthenticodeSignature then fails and prints nothing).
fn winps(script: &str) -> Result<String> {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:/Windows"));
    let o = keel_core::cmd::hidden(root.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-Command", script]).output()?;
    let out = String::from_utf8_lossy(&o.stdout).trim_end().to_string();
    if out.trim().is_empty() {
        bail!("powershell gave no output: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(out)
}

fn signature_status(file: &Path) -> String {
    let script = format!("(Get-AuthenticodeSignature -LiteralPath '{}').Status", file.display().to_string().replace('\'', "''"));
    winps(&script).map(|s| s.trim().to_string()).unwrap_or_else(|e| format!("unknown ({e})"))
}

/// Driver version from the INF's `DriverVer = date, version` line.
pub fn inf_version(inf: &Path) -> Option<String> {
    read_text_any(inf).ok()?.lines()
        .find_map(|l| l.trim().strip_prefix("DriverVer").and_then(|r| r.split('=').nth(1)).map(str::to_string))
        .and_then(|v| v.split(',').nth(1).map(|s| s.trim().to_string()))
}

/// Installs `src` on devices with exactly `hwid`; keeps (or adds) the Windows Update block.
/// `force` also installs an older or equal version (rollback) and a version listed in
/// `known_issues` (version -> note), which is refused otherwise.
pub fn update(src: &Path, hwid: &str, keep_blocked: bool, force: bool,
              known_issues: &std::collections::BTreeMap<String, String>) -> Result<()> {
    let inf = locate_inf(src, hwid)?;
    let version = inf_version(&inf).unwrap_or_default();
    if let Some(note) = known_issues.get(&version) {
        if !force { bail!("driver {version} has a known issue on this machine: {note}
(pass --force to install it anyway)"); }
        log(format!("driver: {version} has a known issue ({note}); installing because of --force"));
    }
    let cat = read_text_any(&inf)?.lines()
        .find_map(|l| l.trim().strip_prefix("CatalogFile").and_then(|r| r.split('=').nth(1)).map(|s| s.trim().to_string()))
        .map(|c| inf.with_file_name(c))
        .ok_or_else(|| anyhow!("INF has no CatalogFile"))?;
    let sig = signature_status(&cat);
    if sig != "Valid" { bail!("catalog {} signature is '{sig}', refusing to install", cat.display()); }
    log(format!("driver: installing {version} from {} on {hwid} (catalog signature Valid{})", inf.display(), if force { ", --force" } else { "" }));

    let was_blocked = is_blocked(hwid);
    unblock(hwid)?;  // the deny policy also stops manual installs
    let mut reboot = windows::core::BOOL(0);
    let r = unsafe {
        UpdateDriverForPlugAndPlayDevicesW(None, &HSTRING::from(hwid), &HSTRING::from(inf.as_os_str()),
            if force { INSTALLFLAG_FORCE } else { UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS(0) }, Some(&mut reboot))
    };
    if keep_blocked || was_blocked { block(hwid)?; }
    match r {
        Ok(()) => {
            log(format!("driver: installed; reboot required: {}", reboot.as_bool()));
            // Adrenalin started during the swap can hang windowless; the tray restarts it.
            crate::apps::request_adrenalin_restart();
            Ok(())
        }
        Err(e) if e.code().0 as u32 == 0xE000_020B => bail!("no connected device has {hwid} - connect the eGPU first"),
        // ERROR_NO_MORE_ITEMS: the package is not newer/better than the installed driver.
        Err(e) if e.code().0 as u32 == 0x8007_0103 => bail!("{version} is not newer than the installed driver - pass --force to roll back"),
        Err(e) => bail!("install failed: {e}"),
    }
}

/// Display adapters with their driver versions (present and absent).
pub fn status() -> Result<String> {
    winps("Get-CimInstance Win32_PnPSignedDriver | ? DeviceClass -eq 'DISPLAY' | Select-Object DeviceName,DriverVersion,DriverDate,HardWareID | Format-Table -AutoSize | Out-String -Width 220")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: launched from PowerShell 7, the inherited PSModulePath broke the 5.1 child
    /// and every catalog came back unsigned. Uses a Windows-signed catalog that always exists.
    #[test]
    fn signature_check_survives_powershell7_module_path() {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:/Windows"));
        let dir = root.join("System32/CatRoot/{F750E6C3-38EE-11D1-85E5-00C04FC295EE}");
        let Some(cat) = std::fs::read_dir(&dir).ok().and_then(|mut d| d.find_map(|e| {
            let p = e.ok()?.path();
            p.extension().is_some_and(|x| x.eq_ignore_ascii_case("cat")).then_some(p)
        })) else { return };
        unsafe { std::env::set_var("PSModulePath", r"C:\Program Files\PowerShell\7\Modules"); }
        assert_eq!(signature_status(&cat), "Valid");
    }
}

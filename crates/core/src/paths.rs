//! Where Keel keeps its data. Machine data (service state, service log) lives in
//! `%ProgramData%\keel`, created by the service so users can read but not edit it; per-user data
//! (samples, tray log, backups) in `%LOCALAPPDATA%\keel`. Both come from the known-folder API: a
//! process started by an installer can lack the environment variables (a tray started that way
//! could not store its samples).
use std::path::PathBuf;
use windows::core::GUID;

/// Folder name under ProgramData / LocalAppData.
pub const APP_DIR: &str = "keel";
/// Overrides `data_dir()` for tests run without the service (never create `%ProgramData%\keel`
/// as the user: the service relies on users not being able to edit it).
pub const DATA_ENV: &str = "KEEL_DATA";

fn known_folder(id: &GUID) -> Option<String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
    unsafe {
        SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok().map(|p| {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as _));
            s
        })
    }.filter(|s| !s.is_empty())
}

/// `%ProgramData%\keel`, or `KEEL_DATA` when set.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os(DATA_ENV) { return PathBuf::from(d); }
    use windows::Win32::UI::Shell::FOLDERID_ProgramData;
    let base = known_folder(&FOLDERID_ProgramData).or_else(|| std::env::var("ProgramData").ok())
        .unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(base).join(APP_DIR)
}

/// Whether this process may write `data_dir()`: an override is set, or the process is elevated
/// (the service, an admin console). A plain user must never create `%ProgramData%\keel`.
pub fn may_write_data_dir() -> bool { std::env::var_os(DATA_ENV).is_some() || elevated() }

/// Fails with a clear message when `may_write_data_dir()` is false.
pub fn ensure_may_write_data_dir() -> anyhow::Result<()> {
    anyhow::ensure!(may_write_data_dir(),
        "{} is the service's folder: run this in an admin console (or set {DATA_ENV}=<scratch dir> to test)",
        data_dir().display());
    Ok(())
}

/// The process token is elevated (TokenElevation).
pub fn elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() { return false; }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenElevation, Some(&mut e as *mut _ as *mut _),
            size_of::<TOKEN_ELEVATION>() as u32, &mut len).is_ok();
        let _ = CloseHandle(token);
        ok && e.TokenIsElevated != 0
    }
}

/// SYSTEM and administrators full control, users read only, not inherited from ProgramData (whose
/// default lets users create files in subfolders: a planted `net.toml` must not reach the service).
const DATA_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

/// Creates `data_dir()` and (when it is the real `%ProgramData%\keel`) gives it the service's ACL.
/// Elevated only; called by the service at start and by the installer's commands.
pub fn create_data_dir() -> anyhow::Result<PathBuf> {
    ensure_may_write_data_dir()?;
    let d = data_dir();
    std::fs::create_dir_all(&d)?;
    if std::env::var_os(DATA_ENV).is_none() { protect(&d, DATA_SDDL)?; }
    Ok(d)
}

fn protect(dir: &std::path::Path, sddl: &str) -> anyhow::Result<()> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::*;
    use windows::Win32::Security::*;
    use windows::core::{BOOL, HSTRING};
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(sddl), SDDL_REVISION_1, &mut sd, None)?;
        let (mut present, mut defaulted) = (BOOL(0), BOOL(0));
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let r = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted);
        let e = r.ok().map(|_| SetNamedSecurityInfoW(&HSTRING::from(dir.as_os_str()), SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, None, None, Some(dacl), None));
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        match e {
            Some(c) if c.is_ok() => Ok(()),
            Some(c) => anyhow::bail!("cannot set the ACL of {}: {c:?}", dir.display()),
            None => anyhow::bail!("bad security descriptor"),
        }
    }
}

/// `%LOCALAPPDATA%\keel`.
pub fn user_data_dir() -> PathBuf {
    use windows::Win32::UI::Shell::FOLDERID_LocalAppData;
    let base = known_folder(&FOLDERID_LocalAppData).or_else(|| std::env::var("LOCALAPPDATA").ok()).unwrap_or_else(|| ".".into());
    PathBuf::from(base).join(APP_DIR)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dirs_end_in_keel() {
        assert!(super::user_data_dir().ends_with("keel"));
        if std::env::var_os(super::DATA_ENV).is_none() { assert!(super::data_dir().ends_with("keel")); }
    }
}

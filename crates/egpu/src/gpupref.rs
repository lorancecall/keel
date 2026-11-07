//! Windows' per-app GPU choice (Settings > Display > Graphics), stored per user in
//! `HKCU\Software\Microsoft\DirectX\UserGpuPreferences` as `<exe path> = "GpuPreference=2;..."`.
//! 2 is "high performance": the eGPU when one is attached. Other flags Windows keeps in the same
//! value (e.g. AutoHDR) stay.
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const KEY: &str = r"Software\Microsoft\DirectX\UserGpuPreferences";

/// Programs set to the high-performance GPU, as stored (full paths).
pub fn high_performance() -> Vec<String> {
    let Ok(k) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(KEY) else { return Vec::new() };
    let mut v: Vec<String> = k.enum_values().flatten()
        .filter(|(_, val)| val.to_string().split(';').any(|p| p.trim() == "GpuPreference=2"))
        .map(|(name, _)| name).collect();
    v.sort_by_key(|s| s.to_lowercase());
    v
}

/// Whether `exe` (full path) is set to the high-performance GPU.
pub fn is_high_performance(exe: &str) -> bool { high_performance().iter().any(|e| e.eq_ignore_ascii_case(exe)) }

/// Sets `exe` to the high-performance GPU (`true`) or drops Keel's choice (`false`).
pub fn set_high_performance(exe: &str, on: bool) -> anyhow::Result<()> {
    let (k, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(KEY)?;
    let old: String = k.get_value(exe).unwrap_or_default();
    let new = crate::legion::with_gpu_pref(&old, on.then_some(2));
    if new.is_empty() { let _ = k.delete_value(exe); } else { k.set_value(exe, &new)?; }
    Ok(())
}

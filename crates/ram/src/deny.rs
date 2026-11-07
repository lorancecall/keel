//! Hard-coded protection list. A protected process is never a candidate for any action, whatever
//! the config says; `scan` reports the reason.

/// Windows core / shell / security processes, by exe name (lowercase).
const NAMES: &[(&str, &str)] = &[
    ("system", "kernel"), ("idle", "kernel"), ("registry", "kernel"), ("secure system", "kernel"),
    ("memory compression", "kernel (compressed RAM store)"),
    ("smss.exe", "windows core"), ("csrss.exe", "windows core"), ("wininit.exe", "windows core"),
    ("winlogon.exe", "windows core"), ("services.exe", "windows core"), ("lsass.exe", "windows core"),
    ("lsaiso.exe", "windows core"), ("svchost.exe", "service host"), ("dwm.exe", "desktop compositor"),
    ("fontdrvhost.exe", "windows core"), ("sihost.exe", "shell"), ("explorer.exe", "shell"),
    ("ctfmon.exe", "input"), ("taskhostw.exe", "shell"), ("conhost.exe", "console host"),
    ("audiodg.exe", "audio"), ("wudfhost.exe", "driver host"), ("dashost.exe", "device host"),
    ("startmenuexperiencehost.exe", "shell"), ("shellexperiencehost.exe", "shell"),
    ("shellhost.exe", "shell"), ("textinputhost.exe", "input"), ("lockapp.exe", "shell"),
    ("msmpeng.exe", "security"), ("nissrv.exe", "security"), ("mpdefendercoreservice.exe", "security"),
    ("securityhealthservice.exe", "security"), ("securityhealthsystray.exe", "security"),
    ("smartscreen.exe", "security"), ("sgrmbroker.exe", "security"),
];

/// Path fragments (lowercase) of vendor / hardware / security installs and Keel's own.
const PATH_PARTS: &[(&str, &str)] = &[
    (r"\lenovo\", "lenovo"), ("legion", "legion space"), (r"\amd\", "amd driver/software"),
    (r"\ati technologies\", "amd driver/software"), ("nvidia", "nvidia driver/software"),
    (r"\keel\", "keel itself"),
    (r"\windows defender", "security"), (r"\microsoft\windows defender", "security"),
    (r"\windows\system32\drivers\", "driver"), (r"\driverstore\", "driver"),
];

/// Why this process must never be quarantined, or None. `name` and `path` may be any case.
pub fn protected(name: &str, path: Option<&str>) -> Option<&'static str> {
    let n = name.to_lowercase();
    if let Some((_, why)) = NAMES.iter().find(|(x, _)| *x == n) { return Some(why); }
    let p = path?.to_lowercase();
    PATH_PARTS.iter().find(|(x, _)| p.contains(x)).map(|(_, why)| *why)
}

#[cfg(test)]
mod tests {
    use super::protected;

    #[test]
    fn protects_core_vendor_and_self() {
        assert_eq!(protected("CSRSS.EXE", None), Some("windows core"));
        assert_eq!(protected("Memory Compression", None), Some("kernel (compressed RAM store)"));
        assert_eq!(protected("LegionSpace.exe", Some(r"C:\Program Files\Lenovo\Legion Space\LegionSpace.exe")), Some("lenovo"));
        assert_eq!(protected("x.exe", Some(r"C:\Program Files\NVIDIA Corporation\x.exe")), Some("nvidia driver/software"));
        assert_eq!(protected("keel-tray.exe", Some(r"C:\Program Files\Keel\keel-tray.exe")), Some("keel itself"));
        assert_eq!(protected("Discord.exe", Some(r"C:\Users\me\AppData\Local\Discord\Discord.exe")), None);
        assert_eq!(protected("Discord.exe", None), None);
    }
}

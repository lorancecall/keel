//! Read-only inventory of everything that can start a program: Run/RunOnce values, services
//! (registry ImagePath + running pids from the SCM), scheduled tasks and Startup-folder items.
use crate::paths::{self, command_exe};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ};
use windows::Win32::System::Services::{
    CloseServiceHandle, EnumServicesStatusExW, OpenSCManagerW, ENUM_SERVICE_STATUS_PROCESSW, SC_ENUM_PROCESS_INFO,
    SC_MANAGER_ENUMERATE_SERVICE, SERVICE_ACTIVE, SERVICE_DRIVER, SERVICE_WIN32,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{Interface, HSTRING, PCWSTR};
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};
use winreg::RegKey;

/// Something that launches an exe, already resolved to a normalized exe path.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Launcher {
    /// run | runonce | service | task | startup
    pub kind: &'static str,
    /// Registry key / task path / shortcut file.
    pub location: String,
    /// Value name, service name, task name or shortcut name.
    pub name: String,
    pub command: String,
    /// None when Windows keeps no on/off flag for it.
    pub enabled: Option<bool>,
    /// Service start type (auto, manual, disabled, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// How it could be turned off (`cleanup` does it; `scan` only says).
    pub disable: &'static str,
    /// svchost-hosted services: the ServiceDll, normalized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dll: Option<String>,
    #[serde(skip)]
    pub exe: Option<String>,
}

/// How each kind of launcher is switched off without deleting anything.
pub fn disable_hint(kind: &str) -> &'static str {
    match kind {
        "run" => "Task Manager > Startup apps > Disable (StartupApproved flag)",
        "runonce" => "runs once at the next logon; nothing to keep disabled",
        "service" => "start type Manual or Disabled (admin)",
        "driver" => "never delete; start type change only with admin, never for boot/system-critical drivers",
        "task" => "schtasks /change /tn <task> /disable (admin for most tasks)",
        "startup" => "Task Manager > Startup apps > Disable",
        _ => "",
    }
}

pub fn env_lookup(n: &str) -> Option<String> { std::env::var(n).ok() }

fn launcher(kind: &'static str, location: String, name: String, command: String, enabled: Option<bool>, start: Option<String>) -> Launcher {
    let exe = command_exe(&command, &env_lookup);
    Launcher { kind, location, name, command, enabled, start, disable: disable_hint(kind), dll: None, exe }
}

/// StartupApproved flags for one hive + subkey (value name -> enabled).
fn approved(root: &RegKey, sub: &str) -> HashMap<String, bool> {
    let path = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\{sub}");
    let Ok(k) = root.open_subkey_with_flags(path, KEY_READ) else { return HashMap::new() };
    k.enum_values().flatten()
        .filter_map(|(n, v)| paths::startup_approved_enabled(&v.bytes).map(|e| (n.to_lowercase(), e)))
        .collect()
}

pub fn run_entries() -> Vec<Launcher> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let cv = r"Software\Microsoft\Windows\CurrentVersion";
    let wow = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion";
    let sets = [
        ("HKCU", &hkcu, cv, "Run", approved(&hkcu, "Run")),
        ("HKCU", &hkcu, cv, "RunOnce", HashMap::new()),
        ("HKLM", &hklm, cv, "Run", approved(&hklm, "Run")),
        ("HKLM", &hklm, cv, "RunOnce", HashMap::new()),
        ("HKLM", &hklm, wow, "Run", approved(&hklm, "Run32")),
        ("HKLM", &hklm, wow, "RunOnce", HashMap::new()),
    ];
    let mut out = Vec::new();
    for (hive, root, base, sub, flags) in sets {
        let path = format!(r"{base}\{sub}");
        let Ok(k) = root.open_subkey_with_flags(&path, KEY_READ) else { continue };
        for (name, _) in k.enum_values().flatten() {
            let Ok(cmd) = k.get_value::<String, _>(&name) else { continue };
            let enabled = if sub == "Run" { Some(flags.get(&name.to_lowercase()).copied().unwrap_or(true)) } else { None };
            let kind = if sub == "Run" { "run" } else { "runonce" };
            out.push(launcher(kind, format!(r"{hive}\{path}"), name, cmd, enabled, None));
        }
    }
    out
}

fn start_name(v: u32) -> &'static str {
    match v { 0 => "boot", 1 => "system", 2 => "auto", 3 => "manual", 4 => "disabled", _ => "?" }
}

/// Services and kernel/file-system drivers from `HKLM\SYSTEM\CurrentControlSet\Services`.
/// Services hosted in svchost carry their `ServiceDll` (the code that is really theirs).
pub fn service_entries() -> Vec<Launcher> {
    let root = r"SYSTEM\CurrentControlSet\Services";
    let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(root, KEY_READ) else { return vec![] };
    let mut out = Vec::new();
    for name in k.enum_keys().flatten() {
        let Ok(s) = k.open_subkey_with_flags(&name, KEY_READ) else { continue };
        let ty: u32 = s.get_value("Type").unwrap_or(0);
        let kind = if ty & 0x30 != 0 { "service" } else if ty & 0x0F != 0 { "driver" } else { continue };
        let img = match s.get_value::<String, _>("ImagePath") {
            Ok(i) => i,
            Err(_) if kind == "driver" => format!(r"System32\drivers\{name}.sys"),
            Err(_) => continue,
        };
        let start: u32 = s.get_value("Start").unwrap_or(99);
        let enabled = Some(start != 4);
        let mut l = launcher(kind, format!(r"HKLM\{root}\{name}"), name, img, enabled, Some(start_name(start).into()));
        if kind == "service" {
            let dll = s.open_subkey_with_flags("Parameters", KEY_READ).ok().and_then(|p| p.get_value::<String, _>("ServiceDll").ok())
                .or_else(|| s.get_value::<String, _>("ServiceDll").ok());
            let root = env_lookup("SystemRoot").unwrap_or_default();
            l.dll = dll.map(|d| paths::normalize_path(&paths::expand_env(&d, &env_lookup), &root));
        }
        out.push(l);
    }
    out
}

/// Running services and drivers (SCM): pid -> hosted service names, and all active names (lowercase).
pub fn scm_active() -> (HashMap<u32, Vec<String>>, HashSet<String>) {
    let mut map: HashMap<u32, Vec<String>> = HashMap::new();
    let mut active = HashSet::new();
    let types = SERVICE_WIN32 | SERVICE_DRIVER;
    unsafe {
        let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ENUMERATE_SERVICE) else { return (map, active) };
        let mut need = 0u32;
        let mut count = 0u32;
        let _ = EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, types, SERVICE_ACTIVE, None, &mut need, &mut count, None, PCWSTR::null());
        let mut buf = vec![0u8; need as usize + 16 * 1024];
        let mut resume = 0u32;
        if EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, types, SERVICE_ACTIVE, Some(&mut buf), &mut need, &mut count, Some(&mut resume), PCWSTR::null()).is_ok() {
            let rows = std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize);
            for r in rows {
                let pid = r.ServiceStatusProcess.dwProcessId;
                let name = r.lpServiceName.to_string().unwrap_or_default();
                active.insert(name.to_lowercase());
                if pid != 0 { map.entry(pid).or_default().push(name); }
            }
        }
        let _ = CloseServiceHandle(scm);
    }
    (map, active)
}

/// Scheduled-task actions this user can read (`schtasks /query /xml ONE`, read-only).
pub fn task_entries() -> Vec<Launcher> {
    let Ok(o) = crate::cmd::hidden("schtasks").args(["/query", "/xml", "ONE"]).output() else { return vec![] };
    let xml = String::from_utf8_lossy(&o.stdout);
    paths::parse_tasks_xml(&xml).into_iter().map(|t| {
        let cmd = if t.arguments.is_empty() { t.command.clone() } else { format!("{} {}", t.command, t.arguments) };
        let mut l = launcher("task", t.task.clone(), t.task.rsplit('\\').next().unwrap_or("").to_string(), cmd, Some(t.enabled), None);
        l.exe = command_exe(&t.command, &env_lookup);
        l
    }).collect()
}

fn resolve_lnk(path: &Path) -> Option<String> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let pf: IPersistFile = link.cast().ok()?;
        pf.Load(&HSTRING::from(path.as_os_str()), STGM_READ).ok()?;
        let mut target = [0u16; 1024];
        link.GetPath(&mut target, std::ptr::null_mut(), 0).ok()?;
        let mut args = [0u16; 1024];
        let _ = link.GetArguments(&mut args);
        let s = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
        let (t, a) = (s(&target), s(&args));
        if t.is_empty() { return None; }
        Some(if a.is_empty() { format!("\"{t}\"") } else { format!("\"{t}\" {a}") })
    }
}

/// Items in the per-user and all-users Startup folders (shortcuts resolved).
pub fn startup_entries() -> Vec<Launcher> {
    unsafe { let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); }
    let user = approved(&RegKey::predef(HKEY_CURRENT_USER), "StartupFolder");
    let common = approved(&RegKey::predef(HKEY_LOCAL_MACHINE), "StartupFolder");
    let tail = r"Microsoft\Windows\Start Menu\Programs\Startup";
    let dirs = [(std::env::var_os("APPDATA"), &user), (std::env::var_os("ProgramData"), &common)];
    let mut out = Vec::new();
    for (base, flags) in dirs {
        let Some(base) = base else { continue };
        let dir = PathBuf::from(base).join(tail);
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let file = e.file_name().to_string_lossy().into_owned();
            if file.eq_ignore_ascii_case("desktop.ini") { continue; }
            let is_lnk = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"));
            let cmd = if is_lnk { resolve_lnk(&p).unwrap_or_default() } else { format!("\"{}\"", p.display()) };
            let enabled = Some(flags.get(&file.to_lowercase()).copied().unwrap_or(true));
            out.push(launcher("startup", p.display().to_string(), file, cmd, enabled, None));
        }
    }
    out
}

/// One entry under an `Uninstall` key (also the hidden `SystemComponent=1` ones).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Product {
    pub key: String,
    pub name: String,
    pub publisher: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    pub system_component: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub uninstall: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub quiet_uninstall: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub install_location: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub display_icon: String,
    /// `EstimatedSize`, KB.
    pub estimated_kb: u32,
}

pub fn products() -> Vec<Product> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let sets = [
        ("HKLM", &hklm, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
        ("HKLM", &hklm, r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        ("HKCU", &hkcu, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    let mut out = Vec::new();
    for (hive, root, path) in sets {
        let Ok(k) = root.open_subkey_with_flags(path, KEY_READ) else { continue };
        for sub in k.enum_keys().flatten() {
            let Ok(e) = k.open_subkey_with_flags(&sub, KEY_READ) else { continue };
            let s = |n: &str| e.get_value::<String, _>(n).unwrap_or_default().trim().to_string();
            let name = s("DisplayName");
            if name.is_empty() { continue; }
            out.push(Product {
                key: format!(r"{hive}\{path}\{sub}"),
                publisher: s("Publisher"),
                version: s("DisplayVersion"),
                system_component: e.get_value::<u32, _>("SystemComponent").unwrap_or(0) == 1,
                uninstall: s("UninstallString"),
                quiet_uninstall: s("QuietUninstallString"),
                install_location: s("InstallLocation"),
                display_icon: s("DisplayIcon"),
                estimated_kb: e.get_value::<u32, _>("EstimatedSize").unwrap_or(0),
                name,
            });
        }
    }
    out
}

/// All launchers, in one list.
pub fn all() -> Vec<Launcher> {
    let mut v = run_entries();
    v.extend(service_entries());
    v.extend(task_entries());
    v.extend(startup_entries());
    v
}

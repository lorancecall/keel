//! Switching primitives for on-demand families: service start types (SCM), scheduled tasks
//! (schtasks), StartupApproved values (registry). Changing services, tasks and HKLM values needs
//! the Keel service (LocalSystem); HKCU values work as the user.
use anyhow::{bail, Context, Result};
use windows::Win32::System::Services::*;
use windows::core::{HSTRING, PCWSTR};
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_BINARY};
use winreg::{RegKey, RegValue};

/// Start type and delayed flag from the registry (what the SCM has, as of now).
pub fn service_start(name: &str) -> Option<(String, bool)> {
    let k = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(format!(r"SYSTEM\CurrentControlSet\Services\{name}"), KEY_READ).ok()?;
    let start: u32 = k.get_value("Start").ok()?;
    let delayed: u32 = k.get_value("DelayedAutostart").unwrap_or(0);
    let s = match start { 2 => "auto", 3 => "manual", 4 => "disabled", 0 => "boot", 1 => "system", _ => "?" };
    Some((s.to_string(), delayed == 1))
}

struct Sc(SC_HANDLE);
impl Drop for Sc { fn drop(&mut self) { unsafe { let _ = CloseServiceHandle(self.0); } } }

fn open(name: &str, access: u32) -> Result<(Sc, Sc)> {
    unsafe {
        let mgr = Sc(OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT).context("open SCM")?);
        let svc = Sc(OpenServiceW(mgr.0, &HSTRING::from(name), access).with_context(|| format!("open service {name}"))?);
        Ok((mgr, svc))
    }
}

/// Windows services Keel never reconfigures, whatever a state file says.
const NEVER: &[&str] = &[
    "windefend", "wdnissvc", "sense", "mdcoresvc", "securityhealthservice", "wscsvc", "mpssvc", "bfe", "eventlog",
    "rpcss", "dcomlaunch", "rpceptmapper", "lsm", "profsvc", "winmgmt", "schedule", "samss", "power", "plugplay",
    "audiosrv", "audioendpointbuilder", "themes", "usermanager", "staterepository", "coremessagingregistrar",
    "brokerinfrastructure", "systemeventsbroker", "dhcp", "dnscache", "nsi", "netprofm", "wlansvc", "cryptsvc",
    "trustedinstaller", "wuauserv", "keel",
];

pub fn never_touch(name: &str) -> bool { NEVER.contains(&name.to_lowercase().as_str()) }

/// Sets a service's start type (`auto` | `manual` | `disabled`) and delayed-start flag.
pub fn set_service_start(name: &str, start: &str, delayed: bool) -> Result<()> {
    if never_touch(name) { bail!("{name} is a core Windows service; Keel never switches it"); }
    let t = match start { "auto" => SERVICE_AUTO_START, "manual" => SERVICE_DEMAND_START, "disabled" => SERVICE_DISABLED, x => bail!("start type {x} is not switchable") };
    let (_m, s) = open(name, SERVICE_CHANGE_CONFIG)?;
    unsafe {
        ChangeServiceConfigW(s.0, ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE), t, SERVICE_ERROR(SERVICE_NO_CHANGE), PCWSTR::null(), PCWSTR::null(),
            None, PCWSTR::null(), PCWSTR::null(), PCWSTR::null(), PCWSTR::null()).with_context(|| format!("change start type of {name}"))?;
        if start == "auto" {
            let info = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: delayed.into() };
            let _ = ChangeServiceConfig2W(s.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(&info as *const _ as *const _));
        }
    }
    Ok(())
}

/// Asks a running service to stop (no wait; a service that is already stopped is fine).
pub fn stop_service(name: &str) -> Result<()> {
    let (_m, s) = open(name, SERVICE_STOP | SERVICE_QUERY_STATUS)?;
    let mut st = SERVICE_STATUS::default();
    unsafe {
        let _ = QueryServiceStatus(s.0, &mut st);
        if st.dwCurrentState == SERVICE_STOPPED { return Ok(()); }
        ControlService(s.0, SERVICE_CONTROL_STOP, &mut st).with_context(|| format!("stop {name}"))?;
    }
    Ok(())
}

/// SERVICE_STATUS dwCurrentState (1 stopped, 3 stop pending, 4 running, ...), None if unknown.
pub fn service_state(name: &str) -> Option<u32> {
    let (_m, s) = open(name, SERVICE_QUERY_STATUS).ok()?;
    let mut st = SERVICE_STATUS::default();
    unsafe { QueryServiceStatus(s.0, &mut st).ok()?; }
    Some(st.dwCurrentState.0)
}

/// Stops all `names`, retrying those refused because a dependent still runs, until every one
/// is stopped or `timeout` passes. Returns the ones still not stopped, with the last error.
pub fn stop_all(names: &[String], timeout: std::time::Duration) -> Vec<String> {
    let end = std::time::Instant::now() + timeout;
    let mut last: std::collections::HashMap<&str, String> = Default::default();
    loop {
        let pending: Vec<&String> = names.iter().filter(|n| service_state(n).is_some_and(|s| s != SERVICE_STOPPED.0)).collect();
        if pending.is_empty() { return vec![]; }
        if std::time::Instant::now() >= end {
            return pending.iter().map(|n| format!("{n} did not stop{}", last.get(n.as_str()).map(|e| format!(": {e}")).unwrap_or_default())).collect();
        }
        for n in &pending {
            if service_state(n) == Some(SERVICE_RUNNING.0) {
                match stop_service(n) { Ok(()) => { last.remove(n.as_str()); } Err(e) => { last.insert(n.as_str(), format!("{e:#}")); } }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

pub fn start_service(name: &str) -> Result<()> {
    let (_m, s) = open(name, SERVICE_START)?;
    unsafe { StartServiceW(s.0, None).with_context(|| format!("start {name}")) }
}

/// Whether a scheduled task is enabled (None when it does not exist / cannot be read).
pub fn task_enabled(path: &str) -> Option<bool> {
    let o = crate::cmd::hidden("schtasks.exe").args(["/query", "/tn", path, "/xml"]).output().ok()?;
    if !o.status.success() { return None; }
    let xml = format!("<!-- {path} -->\n{}", String::from_utf8_lossy(&o.stdout));
    let t = crate::paths::parse_tasks_xml(&xml);
    // A task without an Exec action still has a Settings block; default to enabled.
    Some(t.first().map(|t| t.enabled).unwrap_or(!xml.contains("<Enabled>false</Enabled>")))
}

pub fn set_task(path: &str, enabled: bool) -> Result<()> {
    let o = crate::cmd::hidden("schtasks.exe").args(["/change", "/tn", path, if enabled { "/enable" } else { "/disable" }]).output()?;
    if !o.status.success() { bail!("schtasks /change {path}: {}", String::from_utf8_lossy(&o.stderr).trim()); }
    Ok(())
}

fn hive(key: &str) -> Result<(RegKey, &str)> {
    let (h, rest) = key.split_once('\\').context("bad key")?;
    Ok((RegKey::predef(match h { "HKLM" => HKEY_LOCAL_MACHINE, "HKCU" => HKEY_CURRENT_USER, x => bail!("hive {x}") }), rest))
}

/// Current StartupApproved bytes (None = no value, which Explorer treats as on).
pub fn startup_get(key: &str, value: &str) -> Option<Vec<u8>> {
    let (root, path) = hive(key).ok()?;
    root.open_subkey_with_flags(path, KEY_READ).ok()?.get_raw_value(value).ok().map(|v| v.bytes.into_owned())
}

/// Turns a startup item off (03 ...) or restores the original bytes (None = delete the value).
pub fn startup_set(key: &str, value: &str, bytes: Option<&[u8]>) -> Result<()> {
    let (root, path) = hive(key)?;
    let (k, _) = root.create_subkey_with_flags(path, KEY_SET_VALUE | KEY_READ)?;
    match bytes {
        Some(b) => k.set_raw_value(value, &RegValue { bytes: b.to_vec().into(), vtype: REG_BINARY })?,
        None => { let _ = k.delete_value(value); }
    }
    Ok(())
}

pub const STARTUP_OFF: [u8; 12] = [3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Ends a process (no grace: a LocalSystem service cannot reach a user-session window).
pub fn terminate(pid: u32) -> Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, false, pid).context("open")?;
        let r = TerminateProcess(h, 1).context("terminate");
        let _ = CloseHandle(h);
        r
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn core_services_are_never_switched() {
        assert!(super::never_touch("WinDefend") && super::never_touch("Keel"));
        assert!(!super::never_touch("FileSyncHelper"));
        assert!(super::set_service_start("WinDefend", "disabled", false).is_err());
    }
}

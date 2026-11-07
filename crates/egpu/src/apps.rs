//! Other tuning apps on this machine (user session): Adrenalin stuck-detection / restart and a
//! start/stop log of apps that may touch the same settings (who owns which setting).
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible};

/// Store (DCH) Adrenalin; used when no running instance tells us where it lives.
const ADRENALIN_STORE_AUMID: &str = "AdvancedMicroDevicesInc-2.AMDRadeonSoftware_0a9344xs7nr4m!AMDRadeonsoftwareUWP";
const ADRENALIN_CLASSIC_EXE: &str = r"C:\Program Files\AMD\CNext\CNext\RadeonSoftware.exe";

/// Tuning apps whose start/stop is logged (exe names, case-insensitive).
pub const WATCHED: &[&str] = &[
    "RadeonSoftware.exe", "AMDRSServ.exe", "LegionSpace.exe", "LegionSettingMenu.exe", "LSDaemon.exe",
    "HandheldCompanion.exe", "MSIAfterburner.exe", "RTSS.exe",
];

/// File the elevated `driver-update` touches so the tray restarts Adrenalin afterwards.
pub fn restart_request_path() -> PathBuf { crate::util::data_dir().join("adrenalin-restart.request") }

pub fn request_adrenalin_restart() {
    let _ = std::fs::write(restart_request_path(), chrono::Utc::now().to_rfc3339());
}

fn now_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

/// Pids that own at least one visible, titled top-level window.
fn pids_with_visible_window() -> Vec<u32> {
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                (*(lparam.0 as *mut Vec<u32>)).push(pid);
            }
        }
        BOOL(1)
    }
    let mut pids: Vec<u32> = Vec::new();
    unsafe { let _ = EnumWindows(Some(cb), LPARAM(&mut pids as *mut _ as isize)); }
    pids
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdrenalinState { NotRunning, Ok, Stuck }

/// Adrenalin is "stuck" when the user launched it again (a new cncmd.exe appeared after the
/// window was last seen) and no window showed up within `grace`. A running Adrenalin without a
/// window is normal (closed to the background), so old launches never count.
pub struct AppWatch {
    sys: System,
    last_window_seen: u64,
    running: Vec<(u32, String)>,
    pub grace: Duration,
}

impl AppWatch {
    /// None when the process list is unavailable.
    pub fn new() -> Option<Self> {
        Some(Self { sys: System::new().ok()?, last_window_seen: now_secs(), running: Vec::new(), grace: Duration::from_secs(20) })
    }

    fn refresh(&mut self) {
        self.sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet));
    }

    fn procs(&self, name: &str) -> Vec<(Pid, u64, Option<PathBuf>)> {
        self.sys.processes().values()
            .filter(|p| p.name().to_string_lossy().eq_ignore_ascii_case(name))
            .map(|p| (p.pid(), p.start_time(), p.exe().map(Path::to_path_buf)))
            .collect()
    }

    /// Refreshes processes; returns the Adrenalin state and start/stop lines for watched apps.
    pub fn check(&mut self) -> (AdrenalinState, Vec<String>) {
        self.refresh();
        let mut changes = Vec::new();
        let now: Vec<(u32, String)> = self.sys.processes().values()
            .filter_map(|p| {
                let n = p.name().to_string_lossy().to_string();
                WATCHED.iter().any(|w| w.eq_ignore_ascii_case(&n)).then(|| (p.pid().as_u32(), n))
            }).collect();
        for (pid, n) in &now { if !self.running.contains(&(*pid, n.clone())) { changes.push(format!("app started: {n} (pid {pid})")); } }
        for (pid, n) in &self.running { if !now.contains(&(*pid, n.clone())) { changes.push(format!("app exited: {n} (pid {pid})")); } }
        self.running = now;

        let rs = self.procs("RadeonSoftware.exe");
        if rs.is_empty() { return (AdrenalinState::NotRunning, changes); }
        let visible = pids_with_visible_window();
        if rs.iter().any(|(pid, _, _)| visible.contains(&pid.as_u32())) {
            self.last_window_seen = now_secs();
            return (AdrenalinState::Ok, changes);
        }
        let rs_start = rs.iter().map(|r| r.1).min().unwrap_or(0);
        let launches: Vec<u64> = self.procs("cncmd.exe").into_iter().map(|c| c.1).collect();
        let stuck = is_stuck(rs_start, self.last_window_seen, &launches, now_secs(), self.grace.as_secs());
        (if stuck { AdrenalinState::Stuck } else { AdrenalinState::Ok }, changes)
    }

    /// Ends Windows Search's UI process (SearchHost.exe, WebView2). It does not follow repeated
    /// resolution / scale changes of the main display and then shows only an error; Windows starts
    /// it again the next time search opens. Only this user's processes can be ended. Returns the count.
    pub fn end_search_host(&mut self) -> usize {
        self.refresh();
        self.sys.processes().values()
            .filter(|p| p.name().to_string_lossy().eq_ignore_ascii_case("SearchHost.exe"))
            .filter(|p| p.kill()).count()
    }

    /// Kills Adrenalin (+ cncmd) and starts it again. Returns a short description.
    pub fn restart_adrenalin(&mut self) -> String {
        self.refresh();
        let exe = self.procs("RadeonSoftware.exe").into_iter().find_map(|p| p.2);
        let mut killed = 0;
        for name in ["RadeonSoftware.exe", "cncmd.exe"] {
            for p in self.sys.processes().values().filter(|p| p.name().to_string_lossy().eq_ignore_ascii_case(name)) {
                if p.kill() { killed += 1; }
            }
        }
        std::thread::sleep(Duration::from_millis(1500));
        let target = launch_target(exe.as_deref());
        let r = keel_core::cmd::windowed("explorer.exe").arg(&target).spawn();
        self.last_window_seen = now_secs();
        match r {
            Ok(_) => format!("Adrenalin restarted ({killed} processes ended)"),
            Err(e) => format!("Adrenalin restart failed: {e}"),
        }
    }
}

/// Windowless Adrenalin is stuck when a launch (cncmd.exe) newer than both the last visible
/// window and Adrenalin's own start (+15 s: its companion cncmd) is older than `grace` seconds.
fn is_stuck(rs_start: u64, last_window_seen: u64, cncmd_starts: &[u64], now: u64, grace: u64) -> bool {
    let since = last_window_seen.max(rs_start + 15);
    cncmd_starts.iter().filter(|&&t| t > since).min().is_some_and(|&t| now.saturating_sub(t) >= grace)
}

/// Store installs live under WindowsApps\<Name>_<ver>_<arch>__<publisherId>; launch them through
/// the AppsFolder AUMID. Classic installs launch the exe directly.
fn launch_target(exe: Option<&Path>) -> String {
    if let Some(e) = exe {
        let s = e.to_string_lossy();
        if let Some(dir) = s.split('\\').find(|c| c.contains("__") && c.contains('_')) {
            let name = dir.split('_').next().unwrap_or_default();
            let publisher = dir.rsplit("__").next().unwrap_or_default();
            if !name.is_empty() && !publisher.is_empty() {
                return format!("shell:AppsFolder\\{name}_{publisher}!AMDRadeonsoftwareUWP");
            }
        }
        return s.to_string();
    }
    if Path::new(ADRENALIN_CLASSIC_EXE).exists() { ADRENALIN_CLASSIC_EXE.to_string() } else { format!("shell:AppsFolder\\{ADRENALIN_STORE_AUMID}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_only_after_a_fresh_launch_without_window() {
        // Adrenalin up at 1000 with its companion cncmd at 1002; window last seen at 1500.
        assert!(!is_stuck(1000, 1500, &[1002], 2000, 20), "companion cncmd is not a launch");
        assert!(!is_stuck(1000, 1500, &[1002, 1400], 2000, 20), "launch before the window showed");
        assert!(!is_stuck(1000, 1500, &[1002, 1990], 2000, 20), "fresh launch still within grace");
        assert!(is_stuck(1000, 1500, &[1002, 1900], 2000, 20), "launch 100 s ago, still no window");
        assert!(is_stuck(1000, 1000, &[1001, 1300], 2000, 20), "the 18:11 case: never had a window");
    }

    #[test]
    fn store_path_maps_to_aumid() {
        let p = Path::new(r"C:\Program Files\WindowsApps\AdvancedMicroDevicesInc-2.AMDRadeonSoftware_10.25.30167.0_x64__0a9344xs7nr4m\radeonsoftware\Radeonsoftware.exe");
        assert_eq!(launch_target(Some(p)), format!("shell:AppsFolder\\{ADRENALIN_STORE_AUMID}"));
    }

    #[test]
    fn classic_path_is_launched_directly() {
        let p = Path::new(r"C:\Program Files\AMD\CNext\CNext\RadeonSoftware.exe");
        assert_eq!(launch_target(Some(p)), p.to_string_lossy());
    }
}

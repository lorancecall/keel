//! Keeps Legion Space on the iGPU and the built-in screen (opt-in, tray only). With an eGPU or
//! an external display attached, Legion Space otherwise lands on the external monitor and mixes
//! with the eGPU. Only its window position and the Windows graphics preference are touched,
//! never Legion Space's own settings.
use crate::display::{self, Monitor};
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const EXES: [&str; 3] = ["LegionSpace.exe", "LegionSettingMenu.exe", "LSDaemon.exe"];
const GPU_PREFS: &str = r"Software\Microsoft\DirectX\UserGpuPreferences";

/// Sets (`Some(n)`) or removes (`None`) `GpuPreference=n;` in a UserGpuPreferences value,
/// keeping the other flags Windows stores there (e.g. `AutoHDREnable=..;`).
pub fn with_gpu_pref(existing: &str, pref: Option<u8>) -> String {
    let mut parts: Vec<String> = existing.split(';').map(str::trim)
        .filter(|p| !p.is_empty() && !p.starts_with("GpuPreference=")).map(String::from).collect();
    if let Some(n) = pref { parts.push(format!("GpuPreference={n}")); }
    parts.iter().map(|p| format!("{p};")).collect()
}

/// Installed Legion Space version folders (updates add a new one).
pub fn version_dirs() -> Vec<PathBuf> {
    let base = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("Lenovo").join("LegionSpace");
    let Ok(dirs) = std::fs::read_dir(base) else { return Vec::new() };
    dirs.flatten().map(|d| d.path()).filter(|d| d.is_dir() && d.join("LegionSpace.exe").exists()).collect()
}

/// Legion Space exes in every installed version folder.
pub fn exe_paths() -> Vec<PathBuf> {
    version_dirs().into_iter().flat_map(|d| EXES.iter().map(move |e| d.join(e))).filter(|p| p.exists()).collect()
}

// --- ADL redirect (service / admin CLI): Legion Space loads atiadlxx.dll from its own folder
// before System32, so a copy of the iGPU driver's file there binds its ADLX to the iGPU. ---

const ADL: &str = "atiadlxx.dll";
/// Written next to each copy, so `remove_adl` only deletes files we put there.
const MARKER: &str = "atiadlxx.dll.keel";

fn ours(d: &Path) -> bool { d.join(MARKER).exists() }

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.len() == y.len() && x.modified().ok() == y.modified().ok(),
        _ => false,
    }
}

/// Copies atiadlxx.dll from `igpu_dir` into every Legion Space folder where it is missing or stale.
/// Returns the folders that were updated. Needs admin (Program Files).
pub fn sync_adl(igpu_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let src = igpu_dir.join(ADL);
    anyhow::ensure!(src.exists(), "{} not found", src.display());
    let mut done = Vec::new();
    for d in version_dirs() {
        let dst = d.join(ADL);
        if same_file(&src, &dst) {
            // An identical copy made by hand is adopted, so `legion-fix off` can remove it.
            if !ours(&d) { std::fs::write(d.join(MARKER), src.to_string_lossy().as_bytes())?; }
            continue;
        }
        // A file we did not put there (e.g. shipped by Lenovo) is left alone.
        if dst.exists() && !ours(&d) { continue; }
        std::fs::copy(&src, &dst)?;
        std::fs::write(d.join(MARKER), src.to_string_lossy().as_bytes())?;
        done.push(d);
    }
    Ok(done)
}

/// Removes the copies we made (either marker name). Returns the folders cleaned.
pub fn remove_adl() -> Vec<PathBuf> {
    version_dirs().into_iter().filter(|d| ours(d)).filter(|d| {
        std::fs::remove_file(d.join(ADL)).is_ok()
            && (!d.join(MARKER).exists() || std::fs::remove_file(d.join(MARKER)).is_ok())
    }).collect()
}

/// The iGPU's driver folder: the AMD adapter that is not an eGPU from the config.
pub fn igpu_dir(cfg: &crate::config::Config) -> Option<PathBuf> {
    crate::gpu::adapters().ok()?.into_iter()
        .find(|a| a.vendor_id == crate::gpu::VENDOR_AMD && cfg.profile_for(a.vendor_id, a.device_id).is_none())
        .and_then(|a| crate::amd::driver_dir(a.vendor_id, a.device_id))
}

/// One service step: keep the copies current while `legion_space_fix` is on.
pub fn maintain(cfg: &crate::config::Config) {
    let Some(dir) = igpu_dir(cfg) else { return };
    match sync_adl(&dir) {
        Ok(v) => for d in v { crate::util::log(format!("legion-fix: iGPU ADL copied into {}", d.display())); },
        Err(e) => crate::util::log(format!("legion-fix: {e:#}")),
    }
}

/// (folder, has our copy) for `legion-fix status`.
pub fn adl_status(igpu_dir: Option<&Path>) -> Vec<(PathBuf, bool)> {
    version_dirs().into_iter().map(|d| {
        let igpu = igpu_dir.is_some_and(|s| same_file(&s.join(ADL), &d.join(ADL)));
        (d, igpu)
    }).collect()
}

/// "Power saving" (iGPU) graphics preference for Legion Space, or back to the Windows default.
/// Returns the exes whose value changed.
pub fn set_igpu_preference(on: bool) -> Vec<String> {
    let Ok((key, _)) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey(GPU_PREFS) else { return Vec::new() };
    let mut changed = Vec::new();
    for p in exe_paths() {
        let name = p.to_string_lossy().to_string();
        let old: String = key.get_value(&name).unwrap_or_default();
        let new = with_gpu_pref(&old, on.then_some(1));
        if new == old { continue; }
        let r = if new.is_empty() { key.delete_value(&name) } else { key.set_value(&name, &new) };
        if r.is_ok() { changed.push(name); }
    }
    changed
}

/// Ends the quick menu (Legion Space starts it again on the next Legion button press), so a
/// changed graphics preference applies: Windows reads it only at process start. With the iGPU
/// preferred, the menu's first GPU is the iGPU and its resolution list follows the built-in
/// screen instead of the eGPU monitor.
pub fn restart_quick_menu() -> bool {
    keel_core::cmd::hidden("taskkill.exe").args(["/F", "/IM", "LegionSettingMenu.exe"]).status().is_ok_and(|s| s.success())
}

/// File name of the process that owns `hwnd`.
fn window_exe(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 { return None; }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut n).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..n as usize]).rsplit('\\').next().unwrap_or("").to_string())
    }
}

/// Cached per process id: the tray asks for every window shown and once a second.
pub fn is_legion_window(hwnd: HWND) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: Mutex<Option<HashMap<u32, bool>>> = Mutex::new(None);
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let mut c = CACHE.lock().unwrap();
    let map = c.get_or_insert_with(HashMap::new);
    if map.len() > 256 { map.clear(); }
    *map.entry(pid).or_insert_with(|| window_exe(hwnd).is_some_and(|e| EXES.iter().any(|x| x.eq_ignore_ascii_case(&e))))
}

/// Whether moving Legion Space's windows onto the built-in screen can work now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    /// The built-in screen is the main display (or the only one): Legion Space lands there itself.
    On,
    /// The built-in screen is off: nothing to move to.
    ScreenOff,
    /// Another display is the main one. Legion Space lays its windows out on the main display and
    /// puts them back when moved, so moving them would only make them flicker: left alone.
    ExternalMain,
}

/// `internal` = GDI name of the active built-in screen, `primary` = GDI name of the main display.
pub fn pin_state(internal: Option<&str>, primary: Option<&str>) -> PinState {
    match (internal, primary) {
        (None, _) => PinState::ScreenOff,
        (Some(i), Some(p)) if !i.eq_ignore_ascii_case(p) => PinState::ExternalMain,
        _ => PinState::On,
    }
}

/// A window Legion Space puts back right after it was moved (with the main display on an NVIDIA
/// eGPU its full-screen window snaps back to the main display) is not moved again for a while:
/// moving it every second made it flicker between the screens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Track { last_move: Option<std::time::Instant>, bounces: u32, gave_up: Option<std::time::Instant> }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin { Move, Skip, GiveUp }

/// Found off the built-in screen again within this long after a move = Legion Space moved it back.
const BOUNCE: std::time::Duration = std::time::Duration::from_secs(3);
/// After the second bounce the window is left where Legion Space wants it for this long.
const GIVE_UP_FOR: std::time::Duration = std::time::Duration::from_secs(600);

/// What to do with a window found off the built-in screen at `now` (pure; tested).
pub fn pin_step(t: Track, now: std::time::Instant) -> (Pin, Track) {
    if t.gave_up.is_some_and(|g| now.duration_since(g) < GIVE_UP_FOR) { return (Pin::Skip, t); }
    let bounced = t.last_move.is_some_and(|m| now.duration_since(m) < BOUNCE);
    let bounces = if bounced { t.bounces + 1 } else { 0 };
    if bounces >= 2 { return (Pin::GiveUp, Track { last_move: None, bounces: 0, gave_up: Some(now) }); }
    (Pin::Move, Track { last_move: Some(now), bounces, gave_up: None })
}

fn tracks() -> std::sync::MutexGuard<'static, std::collections::HashMap<isize, Track>> {
    static T: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<isize, Track>>> = std::sync::LazyLock::new(Default::default);
    T.lock().unwrap()
}

/// Moves a top-level Legion Space window onto the built-in panel if it is elsewhere. Large hidden
/// windows count too: the quick menu keeps its overlay prepared but hidden (alpha 0, not
/// WS_VISIBLE) on the main display and only fades it in, so it has to be moved beforehand.
/// Returns Move when it moved the window, GiveUp the one time it stops fighting Legion Space over it.
pub fn pin_window(hwnd: HWND, internal: &Monitor) -> Pin {
    unsafe {
        if GetAncestor(hwnd, GA_ROOT) != hwnd { return Pin::Skip; }
        let mut win = RECT::default();
        if GetWindowRect(hwnd, &mut win).is_err() { return Pin::Skip; }
        let big = win.right - win.left >= 400 && win.bottom - win.top >= 300;
        if !IsWindowVisible(hwnd).as_bool() && !big { return Pin::Skip; }
        let Some((_, from)) = display::monitor_info(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)) else { return Pin::Skip };
        if from.rect == internal.rect { return Pin::Skip; }
        let (step, t) = {
            let mut m = tracks();
            if m.len() > 64 { m.clear(); }
            let (step, t) = pin_step(m.get(&(hwnd.0 as isize)).copied().unwrap_or_default(), std::time::Instant::now());
            m.insert(hwnd.0 as isize, t);
            (step, t)
        };
        let _ = t;
        if step != Pin::Move { return step; }
        let to = display::place(win, &from, internal);
        if SetWindowPos(hwnd, None, to.left, to.top, to.right - to.left, to.bottom - to.top,
                        SWP_NOZORDER | SWP_NOACTIVATE).is_ok() { Pin::Move } else { Pin::Skip }
    }
}

/// True while a large Legion Space window is on screen: visible, or the quick menu's layered
/// overlay faded in (it never sets WS_VISIBLE).
pub fn in_use() -> bool {
    unsafe extern "system" fn each(hwnd: HWND, data: windows::Win32::Foundation::LPARAM) -> windows::core::BOOL {
        let found = unsafe { &mut *(data.0 as *mut bool) };
        let mut r = RECT::default();
        let big = unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() && r.right - r.left >= 400 && r.bottom - r.top >= 300;
        if big && is_legion_window(hwnd) {
            let mut alpha = 0u8;
            let faded_in = unsafe { GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), None) }.is_ok() && alpha > 0;
            if unsafe { IsWindowVisible(hwnd) }.as_bool() || faded_in { *found = true; return windows::core::BOOL(0); }
        }
        windows::core::BOOL(1)
    }
    let mut found = false;
    unsafe { let _ = EnumWindows(Some(each), windows::Win32::Foundation::LPARAM(&mut found as *mut _ as isize)); }
    found
}

/// Every Legion Space window now (pin_window decides which ones move). Returns (moved, given up).
pub fn pin_all(internal: &Monitor) -> (usize, usize) {
    unsafe extern "system" fn each(hwnd: HWND, data: windows::Win32::Foundation::LPARAM) -> windows::core::BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
        if is_legion_window(hwnd) { out.push(hwnd); }
        windows::core::BOOL(1)
    }
    let mut v: Vec<HWND> = Vec::new();
    unsafe { let _ = EnumWindows(Some(each), windows::Win32::Foundation::LPARAM(&mut v as *mut _ as isize)); }
    let r: Vec<Pin> = v.into_iter().map(|h| pin_window(h, internal)).collect();
    (r.iter().filter(|p| **p == Pin::Move).count(), r.iter().filter(|p| **p == Pin::GiveUp).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_only_when_the_built_in_screen_is_main() {
        let int = r"\\.\DISPLAY1";
        assert_eq!(pin_state(None, Some(r"\\.\DISPLAY11")), PinState::ScreenOff);
        assert_eq!(pin_state(Some(int), Some(r"\\.\DISPLAY11")), PinState::ExternalMain);
        assert_eq!(pin_state(Some(int), Some(r"\\.\display1")), PinState::On);
        assert_eq!(pin_state(Some(int), None), PinState::On);
    }

    #[test]
    fn stops_fighting_a_window_that_snaps_back() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let s = |sec: u64| t0 + Duration::from_secs(sec);
        let (p, t) = pin_step(Track::default(), s(0));
        assert_eq!(p, Pin::Move);
        // Back off the built-in screen 1 s later: moved once more (first bounce).
        let (p, t) = pin_step(t, s(1));
        assert_eq!(p, Pin::Move);
        // Again 1 s later: second bounce -> give up, then skip for 10 minutes.
        let (p, t) = pin_step(t, s(2));
        assert_eq!(p, Pin::GiveUp);
        assert_eq!(pin_step(t, s(300)).0, Pin::Skip);
        assert_eq!(pin_step(t, s(603)).0, Pin::Move);
        // A window that only shows up again much later is simply moved.
        let (_, t) = pin_step(Track::default(), s(0));
        assert_eq!(pin_step(t, s(60)), (Pin::Move, Track { last_move: Some(s(60)), bounces: 0, gave_up: None }));
    }

    #[test]
    fn gpu_pref_keeps_other_flags() {
        assert_eq!(with_gpu_pref("", Some(1)), "GpuPreference=1;");
        assert_eq!(with_gpu_pref("AutoHDREnable=48;GpuPreference=2;", Some(1)), "AutoHDREnable=48;GpuPreference=1;");
        assert_eq!(with_gpu_pref("AutoHDREnable=48;GpuPreference=1;", None), "AutoHDREnable=48;");
        assert_eq!(with_gpu_pref("GpuPreference=1;", None), "");
    }
}

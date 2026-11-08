//! Display helpers for the user session: the active displays and their GPU, the built-in panel,
//! where a window lands when moved there, and display modes. Changes (`set_mode`, `set_scale`,
//! `set_primary`, `fit_fullscreen`) are used for displays on the eGPU only (the built-in screen is
//! Legion Space's).
//! None of this works in session 0 (the service).
use windows::core::{BOOL, HSTRING};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Monitor { pub rect: RECT, pub work: RECT }

fn is_internal(t: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> bool {
    matches!(t, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS)
}

fn utf16(b: &[u16]) -> String { String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]) }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveDisplay {
    /// GDI device name such as DISPLAY32 with its usual prefix; the key for modes and monitors.
    pub gdi_name: String,
    /// Monitor name from EDID.
    pub monitor: String,
    /// LUID of the GPU driving it (same form as `gpu::Adapter::luid`).
    pub adapter_luid: u64,
    pub internal: bool,
}

/// Every active display path with its monitor name and GPU.
pub fn active_displays() -> Vec<ActiveDisplay> {
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() { return Vec::new(); }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
            return Vec::new();
        }
        paths.truncate(np as usize);
        paths.iter().filter_map(|p| {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 { return None; }
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            let monitor = if DisplayConfigGetDeviceInfo(&mut tgt.header) == 0 { utf16(&tgt.monitorFriendlyDeviceName) } else { String::new() };
            let l = p.sourceInfo.adapterId;
            Some(ActiveDisplay {
                gdi_name: utf16(&src.viewGdiDeviceName),
                monitor,
                adapter_luid: ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64,
                internal: is_internal(p.targetInfo.outputTechnology),
            })
        }).collect()
    }
}

/// GDI name of the active external display showing `monitor` (EDID name). Look it up again after a
/// vendor display-config change: NVIDIA can renumber the GDI names.
pub fn gdi_of(monitor: &str) -> Option<String> {
    active_displays().into_iter().find(|d| !d.internal && d.monitor == monitor).map(|d| d.gdi_name)
}

/// GDI name of the active built-in panel.
pub fn internal_display_name() -> Option<String> {
    active_displays().into_iter().find(|d| d.internal).map(|d| d.gdi_name)
}

pub fn monitor_info(h: HMONITOR) -> Option<(String, Monitor)> {
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    unsafe { GetMonitorInfoW(h, &mut mi as *mut _ as *mut MONITORINFO) }.as_bool()
        .then(|| (utf16(&mi.szDevice), Monitor { rect: mi.monitorInfo.rcMonitor, work: mi.monitorInfo.rcWork }))
}

/// The built-in panel's monitor rectangles (in the caller's DPI awareness).
pub fn internal_monitor() -> Option<Monitor> {
    monitor_by_name(&internal_display_name()?)
}

/// Rectangles of the monitor with this GDI name.
pub fn monitor_by_name(want: &str) -> Option<Monitor> {
    unsafe extern "system" fn each(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<(String, Monitor)>) };
        if let Some(m) = monitor_info(h) { out.push(m); }
        BOOL(1)
    }
    let mut all: Vec<(String, Monitor)> = Vec::new();
    unsafe { let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut all as *mut _ as isize)); }
    all.into_iter().find(|(n, _)| n.eq_ignore_ascii_case(want)).map(|(_, m)| m)
}

fn w(r: &RECT) -> i32 { r.right - r.left }
fn h(r: &RECT) -> i32 { r.bottom - r.top }

/// Where window `win` (now on monitor `from`) goes on monitor `to`. A window that covers its whole
/// monitor (an overlay such as Legion Space's quick menu) takes the whole target monitor; any
/// other window keeps its size and relative position, clamped into the target work area.
pub fn place(win: RECT, from: &Monitor, to: &Monitor) -> RECT {
    let fits = |m: &RECT| (w(&win) - w(m)).abs() <= 16 && (h(&win) - h(m)).abs() <= 16;
    // Full-screen overlays: sized for the source monitor, or already for the target one
    // (Legion Space sizes its quick menu for the built-in panel but puts it on the main display).
    if fits(&from.rect) || fits(&to.rect) {
        return to.rect;
    }
    let (ww, wh) = (w(&win).min(w(&to.work)), h(&win).min(h(&to.work)));
    let rel = |off: i32, span: i32, size: i32| if span > size { off as f64 / (span - size) as f64 } else { 0.0 };
    let fx = rel(win.left - from.work.left, w(&from.work), w(&win)).clamp(0.0, 1.0);
    let fy = rel(win.top - from.work.top, h(&from.work), h(&win)).clamp(0.0, 1.0);
    let left = to.work.left + ((w(&to.work) - ww) as f64 * fx).round() as i32;
    let top = to.work.top + ((h(&to.work) - wh) as f64 * fy).round() as i32;
    RECT { left, top, right: left + ww, bottom: top + wh }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Mode { pub w: u32, pub h: u32, pub hz: u32 }

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, "{} x {} @ {} Hz", self.w, self.h, self.hz) }
}

fn devmode_of(gdi: &str, i: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    unsafe { EnumDisplaySettingsW(&HSTRING::from(gdi), i, &mut dm) }.as_bool().then_some(dm)
}

fn mode_of(dm: &DEVMODEW) -> Mode { Mode { w: dm.dmPelsWidth, h: dm.dmPelsHeight, hz: dm.dmDisplayFrequency } }

pub fn current_mode(gdi: &str) -> Option<Mode> { devmode_of(gdi, ENUM_CURRENT_SETTINGS).map(|d| mode_of(&d)) }

/// Distinct modes the driver reports for this display (color depths folded together).
pub fn modes(gdi: &str) -> Vec<Mode> {
    let mut v: Vec<Mode> = (0..).map_while(|i| devmode_of(gdi, ENUM_DISPLAY_SETTINGS_MODE(i))).map(|d| mode_of(&d)).collect();
    menu_modes(&mut v);
    v
}

/// Largest first, then fastest; duplicates and modes below 640x480 or 50 Hz dropped.
pub fn menu_modes(v: &mut Vec<Mode>) {
    v.retain(|m| m.w >= 640 && m.h >= 480 && m.hz >= 50);
    v.sort_by(|a, b| (b.w * b.h, b.w, b.hz).cmp(&(a.w * a.h, a.w, a.hz)));
    v.dedup();
}

/// Switches one display to a reported mode (kept for the next logon too). The caller must
/// offer a confirm-or-revert step.
pub fn set_mode(gdi: &str, m: Mode) -> anyhow::Result<()> {
    let mut dm = devmode_of(gdi, ENUM_CURRENT_SETTINGS).ok_or_else(|| anyhow::anyhow!("{gdi}: no current mode"))?;
    dm.dmPelsWidth = m.w;
    dm.dmPelsHeight = m.h;
    dm.dmDisplayFrequency = m.hz;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    let r = unsafe { ChangeDisplaySettingsExW(&HSTRING::from(gdi), Some(&dm), None, CDS_UPDATEREGISTRY, None) };
    anyhow::ensure!(r == DISP_CHANGE_SUCCESSFUL, "{gdi}: ChangeDisplaySettingsEx returned {}", r.0);
    Ok(())
}

// --- Windows display scale (Settings > Display > Scale). There is no public API: Settings uses
// DisplayConfigGet/SetDeviceInfo with the private types -3 / -4 on the display's source. The value
// is stored *relative to the recommended scale* (which depends on the resolution), as steps in
// DPI_STEPS; the recommended step is at index -min. ---

pub const DPI_STEPS: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

/// Current scale in percent from the driver's (min, cur, max) relative steps.
pub fn dpi_percent(min_rel: i32, cur_rel: i32) -> Option<u32> {
    let i = min_rel.checked_neg()?.checked_add(cur_rel)?;
    DPI_STEPS.get(usize::try_from(i).ok()?).copied()
}

/// The relative step that gives `percent`, if it is within this display's range.
pub fn dpi_rel_for(percent: u32, min_rel: i32, max_rel: i32) -> Option<i32> {
    let i = DPI_STEPS.iter().position(|&p| p == percent)? as i32;
    let rel = i + min_rel;
    (min_rel..=max_rel).contains(&rel).then_some(rel)
}

#[repr(C)]
struct DpiGet { header: DISPLAYCONFIG_DEVICE_INFO_HEADER, min_rel: i32, cur_rel: i32, max_rel: i32 }
#[repr(C)]
struct DpiSet { header: DISPLAYCONFIG_DEVICE_INFO_HEADER, rel: i32 }

/// Source (adapter, id) of the active path whose GDI name is `gdi`.
fn source_of(gdi: &str) -> Option<(windows::Win32::Foundation::LUID, u32)> {
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).ok().ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).ok().ok()?;
        paths.truncate(np as usize);
        paths.iter().find_map(|p| {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            (DisplayConfigGetDeviceInfo(&mut src.header) == 0 && utf16(&src.viewGdiDeviceName).eq_ignore_ascii_case(gdi))
                .then_some((p.sourceInfo.adapterId, p.sourceInfo.id))
        })
    }
}

/// (min, cur, max) relative scale steps of a display.
fn dpi_raw(gdi: &str) -> Option<(i32, i32, i32)> {
    let (adapter, id) = source_of(gdi)?;
    let mut g = DpiGet { header: DISPLAYCONFIG_DEVICE_INFO_HEADER::default(), min_rel: 0, cur_rel: 0, max_rel: 0 };
    g.header.r#type = DISPLAYCONFIG_DEVICE_INFO_TYPE(-3);
    g.header.size = std::mem::size_of::<DpiGet>() as u32;
    g.header.adapterId = adapter;
    g.header.id = id;
    (unsafe { DisplayConfigGetDeviceInfo(&mut g.header) } == 0).then_some((g.min_rel, g.cur_rel, g.max_rel))
}

/// Current Windows scale of a display in percent.
pub fn scale(gdi: &str) -> Option<u32> {
    let (min, cur, _) = dpi_raw(gdi)?;
    dpi_percent(min, cur)
}

/// Sets the Windows scale of a display (kept for that monitor like a change in Settings).
pub fn set_scale(gdi: &str, percent: u32) -> anyhow::Result<()> {
    let (min, _, max) = dpi_raw(gdi).ok_or_else(|| anyhow::anyhow!("{gdi}: cannot read the scale"))?;
    let rel = dpi_rel_for(percent, min, max)
        .ok_or_else(|| anyhow::anyhow!("{gdi}: {percent}% is outside {:?}..{:?}", dpi_percent(min, min), dpi_percent(min, max)))?;
    let (adapter, id) = source_of(gdi).ok_or_else(|| anyhow::anyhow!("{gdi}: no active path"))?;
    let mut s = DpiSet { header: DISPLAYCONFIG_DEVICE_INFO_HEADER::default(), rel };
    s.header.r#type = DISPLAYCONFIG_DEVICE_INFO_TYPE(-4);
    s.header.size = std::mem::size_of::<DpiSet>() as u32;
    s.header.adapterId = adapter;
    s.header.id = id;
    let r = unsafe { DisplayConfigSetDeviceInfo(&s.header) };
    anyhow::ensure!(r == 0, "{gdi}: setting the scale returned {r}");
    Ok(())
}

/// Font smoothing type (1 = standard / grayscale, 2 = ClearType). It is one setting for the whole
/// session, not per monitor.
pub fn font_smoothing() -> Option<u32> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    let mut t = 0u32;
    unsafe { SystemParametersInfoW(SPI_GETFONTSMOOTHINGTYPE, 0, Some(&mut t as *mut u32 as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) }.ok()?;
    Some(t)
}

/// Sets it like Settings / the ClearType tuner do: saved and broadcast, so no logoff is needed
/// (windows that cache their text redraw it on their next repaint or restart).
pub fn set_font_smoothing(t: u32) -> anyhow::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    // SPI_SETFONTSMOOTHINGTYPE takes the value itself in pvParam.
    unsafe { SystemParametersInfoW(SPI_SETFONTSMOOTHINGTYPE, 0, Some(t as usize as *mut _), SPIF_UPDATEINIFILE | SPIF_SENDCHANGE) }?;
    Ok(())
}

/// Desktop position of each active display (GDI name, x, y). The primary is the one at (0, 0).
pub fn positions() -> Vec<(String, i32, i32)> {
    let mut out: Vec<(String, i32, i32)> = Vec::new();
    for d in active_displays() {
        if out.iter().any(|(n, _, _)| n == &d.gdi_name) { continue; } // clones share one source
        if let Some(dm) = devmode_of(&d.gdi_name, ENUM_CURRENT_SETTINGS) {
            let p = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
            out.push((d.gdi_name, p.x, p.y));
        }
    }
    out
}

pub fn primary_of(pos: &[(String, i32, i32)]) -> Option<&str> {
    pos.iter().find(|(_, x, y)| *x == 0 && *y == 0).map(|(n, _, _)| n.as_str())
}

/// The layout with `target` moved to (0, 0) and every other display shifted by the same offset,
/// so their arrangement relative to each other is unchanged.
pub fn shifted(pos: &[(String, i32, i32)], target: &str) -> Option<Vec<(String, i32, i32)>> {
    let (_, tx, ty) = pos.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(target))?;
    Some(pos.iter().map(|(n, x, y)| (n.clone(), x - tx, y - ty)).collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryStep {
    /// The eGPU has no active display yet: look again later.
    Wait,
    /// Nothing to do for this attach (already handled, or the eGPU display is primary).
    Done,
    /// Make this display the primary, then count the attach as handled.
    Set(String),
}

/// Once per attach: `handled` is true after the first decision, so a primary the user picks by
/// hand afterwards stays until the next attach. `egpu` = active displays on the eGPU.
pub fn primary_step(handled: bool, egpu: &[String], primary: Option<&str>) -> PrimaryStep {
    if handled { return PrimaryStep::Done; }
    let Some(first) = egpu.first() else { return PrimaryStep::Wait };
    if primary.is_some_and(|p| egpu.iter().any(|e| e.eq_ignore_ascii_case(p))) { return PrimaryStep::Done; }
    PrimaryStep::Set(first.clone())
}

/// Makes `target` the primary display in one topology change: every display's new position is
/// staged in the registry (CDS_NORESET), then applied together.
pub fn set_primary(target: &str) -> anyhow::Result<()> {
    let layout = shifted(&positions(), target).ok_or_else(|| anyhow::anyhow!("{target}: not an active display"))?;
    for (name, x, y) in &layout {
        let mut dm = devmode_of(name, ENUM_CURRENT_SETTINGS).ok_or_else(|| anyhow::anyhow!("{name}: no current mode"))?;
        dm.Anonymous1.Anonymous2.dmPosition = windows::Win32::Foundation::POINTL { x: *x, y: *y };
        dm.dmFields = DM_POSITION;
        let mut flags = CDS_UPDATEREGISTRY | CDS_NORESET;
        if name.eq_ignore_ascii_case(target) { flags |= CDS_SET_PRIMARY; }
        let r = unsafe { ChangeDisplaySettingsExW(&HSTRING::from(name.as_str()), Some(&dm), None, flags, None) };
        anyhow::ensure!(r == DISP_CHANGE_SUCCESSFUL, "{name}: staging position returned {}", r.0);
    }
    let r = unsafe { ChangeDisplaySettingsExW(None, None, None, CDS_TYPE(0), None) };
    anyhow::ensure!(r == DISP_CHANGE_SUCCESSFUL, "applying the layout returned {}", r.0);
    Ok(())
}

/// Which active paths stay on for "main display only": those whose desktop starts at (0, 0)
/// (the primary and its clones). `None` when no path is the primary or nothing would go off. Pure.
pub fn keep_primary(origins: &[Option<(i32, i32)>]) -> Option<Vec<bool>> {
    let keep: Vec<bool> = origins.iter().map(|o| *o == Some((0, 0))).collect();
    (keep.iter().any(|k| *k) && keep.iter().any(|k| !*k)).then_some(keep)
}

/// Turns every display but the main one off for now (game mode). Not saved in Windows' display
/// database, so `restore_layout` (or the next sign-in) brings the user's own layout back.
/// Returns the monitor names turned off (empty: one display already).
pub fn main_display_only() -> anyhow::Result<Vec<String>> {
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).ok()?;
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        let origins: Vec<Option<(i32, i32)>> = paths.iter().map(|p| {
            let m = modes.get(p.sourceInfo.Anonymous.modeInfoIdx as usize)?;
            (m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE).then(|| (m.Anonymous.sourceMode.position.x, m.Anonymous.sourceMode.position.y))
        }).collect();
        let Some(keep) = keep_primary(&origins) else { return Ok(Vec::new()) };
        let off: Vec<String> = paths.iter().zip(&keep).filter(|(_, k)| !**k).map(|(p, _)| {
            let mut t = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            t.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            t.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            t.header.adapterId = p.targetInfo.adapterId;
            t.header.id = p.targetInfo.id;
            let n = if DisplayConfigGetDeviceInfo(&mut t.header) == 0 { utf16(&t.monitorFriendlyDeviceName) } else { String::new() };
            if !n.is_empty() { n } else if is_internal(p.targetInfo.outputTechnology) { "built-in screen".into() } else { format!("display {}", p.targetInfo.id) }
        }).collect();
        let kept: Vec<DISPLAYCONFIG_PATH_INFO> = paths.iter().zip(&keep).filter(|(_, k)| **k).map(|(p, _)| *p).collect();
        let r = SetDisplayConfig(Some(&kept), Some(&modes), SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES);
        anyhow::ensure!(r == 0, "SetDisplayConfig returned {r}");
        Ok(off)
    }
}

/// Puts back the layout Windows has saved for the connected monitors (the user's own one).
pub fn restore_layout() -> anyhow::Result<()> {
    let r = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT) };
    anyhow::ensure!(r == 0, "SetDisplayConfig (saved layout) returned {r}");
    Ok(())
}

/// Full-screen window left at another resolution of this monitor: it sits at the monitor's
/// top-left and is exactly the size of one of the monitor's other modes (Steam Big Picture stayed
/// 2560x1440 after a game switched the desktop to 1440 and back to 1080). Returns the monitor's
/// rectangle to put it on. Normal windows are not exactly a display mode at 0,0, and maximized
/// ones are left to Windows (the caller skips them).
pub fn fit_target(win: RECT, mon: RECT, sizes: &[(i32, i32)]) -> Option<RECT> {
    let at_origin = (win.left - mon.left).abs() <= 2 && (win.top - mon.top).abs() <= 2;
    let size = (w(&win), h(&win));
    (at_origin && size != (w(&mon), h(&mon)) && sizes.contains(&size)).then_some(mon)
}

/// Resizes full-screen windows on display `gdi` that a mode change left at another mode's size
/// (`fit_target`) to the current monitor. Skips hidden, minimized, maximized, cloaked (suspended
/// store apps), tool and click-through windows, the desktop and this process. Log line per window.
pub fn fit_fullscreen(gdi: &str) -> Vec<String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::UI::WindowsAndMessaging::*;
    struct Ctx { mon: RECT, sizes: Vec<(i32, i32)>, pid: u32, out: Vec<String> }
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> BOOL {
        let c = unsafe { &mut *(data.0 as *mut Ctx) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool() { return BOOL(1); }
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex & (WS_EX_TOOLWINDOW.0 | WS_EX_TRANSPARENT.0) != 0 || GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CHILD.0 != 0 { return BOOL(1); }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == c.pid { return BOOL(1); }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok() && cloaked != 0 { return BOOL(1); }
            let mut cls = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut cls) as usize;
            let class = String::from_utf16_lossy(&cls[..n]);
            if ["Progman", "WorkerW", "Shell_TrayWnd", "ApplicationFrameWindow"].contains(&class.as_str()) { return BOOL(1); }
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() { return BOOL(1); }
            let Some(to) = fit_target(r, c.mon, &c.sizes) else { return BOOL(1) };
            let mut t = [0u16; 128];
            let n = GetWindowTextW(hwnd, &mut t) as usize;
            let title = String::from_utf16_lossy(&t[..n]);
            let res = SetWindowPos(hwnd, None, to.left, to.top, w(&to), h(&to), SWP_NOZORDER | SWP_NOACTIVATE);
            c.out.push(format!("'{title}' [{class}] {}x{} -> {}x{} ({res:?})", w(&r), h(&r), w(&to), h(&to)));
        }
        BOOL(1)
    }
    let Some(m) = monitor_by_name(gdi) else { return Vec::new() };
    let sizes = modes(gdi).into_iter().map(|x| (x.w as i32, x.h as i32)).collect();
    let mut c = Ctx { mon: m.rect, sizes, pid: std::process::id(), out: Vec::new() };
    unsafe { let _ = windows::Win32::UI::WindowsAndMessaging::EnumWindows(Some(each), LPARAM(&mut c as *mut _ as isize)); }
    c.out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_only_fullscreen_windows_left_at_another_mode() {
        let r = |l, t, r, b| RECT { left: l, top: t, right: r, bottom: b };
        let mon = r(0, 0, 1920, 1080);
        let sizes = [(3840, 2160), (2560, 1440), (1920, 1080), (1280, 720)];
        // Big Picture left at 1440 (and a 4K or 720p leftover): back to the monitor.
        assert_eq!(fit_target(r(0, 0, 2560, 1440), mon, &sizes), Some(mon));
        assert_eq!(fit_target(r(0, 0, 3840, 2160), mon, &sizes), Some(mon));
        assert_eq!(fit_target(r(0, 0, 1280, 720), mon, &sizes), Some(mon));
        // Already full-screen, a normal window, a window not at the origin: left alone.
        assert_eq!(fit_target(mon, mon, &sizes), None);
        assert_eq!(fit_target(r(1, 1, 1919, 1031), mon, &sizes), None);
        assert_eq!(fit_target(r(100, 50, 2660, 1490), mon, &sizes), None);
        // A second monitor to the right: its own origin counts.
        let right = r(1920, 0, 3840, 1080);
        assert_eq!(fit_target(r(1920, 0, 4480, 1440), right, &sizes), Some(right));
    }

    #[test]
    fn main_display_only_keeps_the_primary_and_its_clones() {
        assert_eq!(keep_primary(&[Some((0, 0)), Some((1920, 0))]), Some(vec![true, false]));
        assert_eq!(keep_primary(&[Some((-2560, 274)), Some((0, 0)), Some((0, 0))]), Some(vec![false, true, true]));
        assert_eq!(keep_primary(&[Some((0, 0))]), None, "one display: nothing to do");
        assert_eq!(keep_primary(&[Some((10, 0)), None]), None, "no primary found: leave it");
    }

    fn p(n: &str, x: i32, y: i32) -> (String, i32, i32) { (n.into(), x, y) }

    #[test]
    fn shift_keeps_relative_layout() {
        // Built-in panel primary, 4K monitor (scaled to 1920x1080) to its right.
        let pos = vec![p("D1", 0, 0), p("D6", 1280, -140)];
        assert_eq!(primary_of(&pos), Some("D1"));
        let s = shifted(&pos, "d6").unwrap();
        assert_eq!(s, vec![p("D1", -1280, 140), p("D6", 0, 0)]);
        assert_eq!(primary_of(&s), Some("D6"));
        assert!(shifted(&pos, "D9").is_none());
    }

    #[test]
    fn dpi_steps_are_relative_to_recommended() {
        // 4K: recommended 150% (index 2) -> min -2; 200% (index 4) is +2 steps from it.
        assert_eq!(dpi_percent(-2, 0), Some(150));
        assert_eq!(dpi_percent(-2, -2), Some(100));
        assert_eq!(dpi_rel_for(200, -2, 7), Some(2));
        assert_eq!(dpi_rel_for(100, -2, 7), Some(-2));
        // 1080: recommended 100% (min 0), max 175%.
        assert_eq!(dpi_rel_for(100, 0, 3), Some(0));
        assert_eq!(dpi_rel_for(200, 0, 3), None);
        assert_eq!(dpi_rel_for(110, 0, 3), None);
        assert_eq!(dpi_percent(1, 0), None);
    }

    #[test]
    fn primary_once_per_attach() {
        let e = vec!["D6".to_string()];
        assert_eq!(primary_step(false, &[], Some("D1")), PrimaryStep::Wait);
        assert_eq!(primary_step(false, &e, Some("D1")), PrimaryStep::Set("D6".into()));
        assert_eq!(primary_step(false, &e, Some("d6")), PrimaryStep::Done);
        // Handled: the user moved the primary back by hand; leave it until the next attach.
        assert_eq!(primary_step(true, &e, Some("D1")), PrimaryStep::Done);
        assert_eq!(primary_step(false, &e, None), PrimaryStep::Set("D6".into()));
    }

    fn r(l: i32, t: i32, rr: i32, b: i32) -> RECT { RECT { left: l, top: t, right: rr, bottom: b } }

    #[test]
    fn menu_modes_sorted_and_deduped() {
        let m = |w, h, hz| Mode { w, h, hz };
        let mut v = vec![m(1920, 1080, 60), m(3840, 2160, 60), m(1920, 1080, 60), m(1920, 1080, 120), m(640, 400, 60), m(3840, 2160, 30)];
        menu_modes(&mut v);
        assert_eq!(v, vec![m(3840, 2160, 60), m(1920, 1080, 120), m(1920, 1080, 60)]);
    }

    #[test]
    fn fullscreen_overlay_takes_the_whole_target() {
        let ext = Monitor { rect: r(0, 0, 1920, 1080), work: r(0, 0, 1920, 1032) };
        let int = Monitor { rect: r(-2560, 274, 0, 1874), work: r(-2560, 274, 0, 1826) };
        assert_eq!(place(r(4, 0, 1932, 1080), &ext, &int), int.rect);
        // Already sized for the target panel (2560x1600) but sitting on the external monitor.
        let ext4k = Monitor { rect: r(0, 0, 3840, 2160), work: r(0, 0, 3840, 2112) };
        let panel = Monitor { rect: r(-2560, 548, 0, 2148), work: r(-2560, 548, 0, 2052) };
        assert_eq!(place(r(0, 0, 2560, 1600), &ext4k, &panel), panel.rect);
    }

    #[test]
    fn window_keeps_relative_position_and_fits() {
        let ext = Monitor { rect: r(0, 0, 1920, 1080), work: r(0, 0, 1920, 1080) };
        let int = Monitor { rect: r(-1000, 0, 0, 600), work: r(-1000, 0, 0, 600) };
        // Right-aligned 400x300 window stays right-aligned.
        assert_eq!(place(r(1520, 0, 1920, 300), &ext, &int), r(-400, 0, 0, 300));
        // Too big for the target: shrunk to the work area.
        assert_eq!(place(r(100, 100, 1500, 900), &ext, &int), r(-1000, 0, 0, 600));
    }
}

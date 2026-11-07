//! The tray panel (`keel-gui --flyout L,T,R,B`, started by the tray on a left click with the
//! tray icon's rectangle): free memory, the eGPU, where internet goes, integer scaling per display.
//! Placed next to the tray icon inside the work area; closes on Escape or when another window
//! (not the taskbar) takes the focus. The tray ends it on a second click.
use crate::{displays, DisplayCard, Flyout};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::time::Duration;

thread_local! {
    /// Panel row -> card index in `displays::cards()` (other displays are left out).
    static ROWS: RefCell<Vec<usize>> = Default::default();
}

fn memory(f: &Flyout) {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_err() { return; }
    let gb = |b: u64| b as f64 / 1073741824.0;
    f.set_mem_free(format!("{:.1} GB free", gb(m.ullAvailPhys)).into());
    f.set_mem_sub(format!("of {:.1} GB · {}% used", gb(m.ullTotalPhys), m.dwMemoryLoad).into());
    f.set_mem_frac(m.dwMemoryLoad as f32 / 100.0);
}

fn egpu(f: &Flyout) {
    let st: Option<serde_json::Value> = std::fs::read_to_string(keel_egpu::util::status_path()).ok().and_then(|s| serde_json::from_str(&s).ok());
    let alive = st.as_ref().and_then(|s| s["utc"].as_str()).and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok())
        .is_some_and(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() < 20);
    let t: Option<keel_egpu::telemetry::GpuTelemetry> = st.as_ref().filter(|_| alive).and_then(|s| serde_json::from_value(s["telemetry"].clone()).ok());
    f.set_egpu_on(t.is_some());
    match (alive, t) {
        (false, _) => { f.set_egpu_value("Service not running".into()); f.set_egpu_sub("".into()); }
        (true, None) => { f.set_egpu_value("Not attached".into()); f.set_egpu_sub("".into()); }
        (true, Some(t)) => {
            f.set_egpu_value(t.name.clone().into());
            let game = st.as_ref().and_then(|s| s["game"].as_str()).map(|g| format!(" · {g}")).unwrap_or_default();
            let mut v = Vec::new();
            if let Some(w) = t.power_w { v.push(format!("{w:.0} W")); }
            if let Some(c) = t.hotspot_c.or(t.temp_c) { v.push(format!("{c:.0} °C")); }
            if let Some(u) = t.usage_pct { v.push(format!("{u:.0} %")); }
            if let (Some(u), Some(tot)) = (t.vram_used_mb, t.vram_total_mb) { v.push(format!("VRAM {:.1} / {:.0} GB", u as f64 / 1024.0, tot as f64 / 1024.0)); }
            f.set_egpu_sub(format!("{}{game}", v.join(" · ")).into());
        }
    }
}

fn display_rows(f: &Flyout) {
    let (cards, _) = displays::cards();
    let keep: Vec<usize> = (0..cards.len()).filter(|&i| cards[i].kind < 2).collect();
    let rows: Vec<DisplayCard> = keep.iter().map(|&i| cards[i].clone()).collect();
    f.set_displays(ModelRc::new(VecModel::from(rows)));
    ROWS.with_borrow_mut(|r| *r = keep);
}

fn network(f: &Flyout) {
    let w = f.as_weak();
    std::thread::spawn(move || {
        let r = keel_net::service::call(&keel_net::service::Request::Status);
        let _ = w.upgrade_in_event_loop(move |f| match r {
            Ok(r) => {
                let (head, sub) = crate::home::net_headline(&r);
                f.set_net_value(head.into());
                f.set_net_sub(if !r.problems.is_empty() { r.problems.join("; ") } else { sub }.into());
                f.set_net_state(if !r.problems.is_empty() { 2 } else if r.pending.is_empty() { 0 } else { 1 });
            }
            Err(_) => { f.set_net_value("Not watched".into()); f.set_net_sub("Service not running".into()); f.set_net_state(2); }
        });
    });
}

/// Top-left corner for a window of `size` next to the tray icon `icon` (L, T, R, B), inside the
/// work area `work` (pure; physical pixels).
pub fn place(icon: (i32, i32, i32, i32), size: (i32, i32), work: (i32, i32, i32, i32)) -> (i32, i32) {
    let (w, h) = size;
    let gap = 8;
    let cx = (icon.0 + icon.2) / 2;
    let x = (cx - w / 2).clamp(work.0 + gap, (work.2 - w - gap).max(work.0 + gap));
    // Taskbar at the bottom (the usual case): above the icon; at the top: below it.
    let y = if icon.1 >= (work.1 + work.3) / 2 { work.3 - h - gap } else { work.1 + gap };
    (x, y)
}

fn work_area(pt: (i32, i32)) -> (i32, i32, i32, i32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    unsafe {
        let m = MonitorFromPoint(POINT { x: pt.0, y: pt.1 }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(m, &mut mi).as_bool() { let r = mi.rcWork; return (r.left, r.top, r.right, r.bottom); }
    }
    (0, 0, 1920, 1080)
}

/// Whether the focus went to another window than ours or the taskbar / tray overflow.
fn lost_focus(own: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == own { return false; }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(fg, &mut buf) as usize;
        let class = String::from_utf16_lossy(&buf[..n]);
        !matches!(class.as_str(), "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "NotifyIconOverflowWindow" | "TopLevelWindowForOverflowXamlIsland")
    }
}

pub fn run(icon: (i32, i32, i32, i32)) -> Result<(), slint::PlatformError> {
    let Some(_one) = keel_core::single::acquire("keel-flyout") else { return Ok(()) };
    let f = Flyout::new()?;
    f.set_force_scheme(match std::env::var("KEEL_THEME").as_deref() { Ok("dark") => 1, Ok("light") => 2, _ => 0 });
    memory(&f);
    egpu(&f);
    network(&f);
    display_rows(&f);
    { let w = f.as_weak(); displays::read_igpu(move || if let Some(f) = w.upgrade() { display_rows(&f); }); }
    f.on_open(|tab| {
        let exe = std::env::current_exe().unwrap_or_default();
        let mut c = keel_core::cmd::windowed(exe);
        if !tab.is_empty() { c.args(["--tab", tab.as_str()]); }
        let _ = c.spawn();
        let _ = slint::quit_event_loop();
    });
    f.on_dismiss(|| { let _ = slint::quit_event_loop(); });
    {
        let w = f.as_weak();
        f.on_integer(move |row, on| {
            let Some(card) = ROWS.with_borrow(|r| r.get(row.max(0) as usize).copied()) else { return };
            let w = w.clone();
            displays::switch_integer(card as i32, on, move |text| if let Some(f) = w.upgrade() {
                f.set_result(text.into());
                display_rows(&f);
            });
        });
    }
    f.show()?;
    // Size is known once shown: place it by the tray icon.
    let size = f.window().size();
    let (x, y) = place(icon, (size.width as i32, size.height as i32), work_area((icon.0, icon.1)));
    f.window().set_position(slint::PhysicalPosition::new(x, y));
    // No taskbar button: a tool window, like the system's own flyouts.
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::*;
        if let Ok(h) = FindWindowW(windows::core::PCWSTR::null(), windows::core::w!("Keel panel")) {
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
            SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize));
            let _ = SetForegroundWindow(h);
        }
    }
    // Refresh while open; close when another window takes the focus.
    let tick = slint::Timer::default();
    {
        let w = f.as_weak();
        let mut n = 0u32;
        tick.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
            let Some(f) = w.upgrade() else { return };
            n += 1;
            let own = unsafe { windows::Win32::UI::WindowsAndMessaging::FindWindowW(windows::core::PCWSTR::null(), windows::core::w!("Keel panel")) };
            // Give the window a moment to become the foreground one before watching.
            if n > 4 && own.as_ref().is_ok_and(|h| lost_focus(*h)) { let _ = slint::quit_event_loop(); return; }
            if n % 8 == 0 { memory(&f); egpu(&f); }
            if n % 40 == 0 { network(&f); display_rows(&f); }
        });
    }
    slint::run_event_loop()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn sits_above_a_bottom_taskbar_inside_the_work_area() {
        let work = (0, 0, 1920, 1040);
        assert_eq!(super::place((1800, 1050, 1832, 1080), (330, 400), work), (1582, 632));
        assert_eq!(super::place((10, 1050, 42, 1080), (330, 400), work), (8, 632), "kept on screen at the left");
        assert_eq!(super::place((1800, 0, 1832, 30), (330, 400), (0, 40, 1920, 1080)), (1582, 48), "taskbar at the top");
    }
}

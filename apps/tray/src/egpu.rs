//! The eGPU part of the tray: reads the service's
//! `egpu-status.json` once a second, turns new alerts into notifications, keeps Legion Space on
//! the iGPU and the built-in screen (opt-in), owns the eGPU monitors' display modes and integer
//! scaling (desktop and game hand-over), fits full-screen windows after mode changes, prunes AMD's
//! duplicate tray-icon entries and restarts a stuck Adrenalin. Everything here needs the user's
//! session, which the service (session 0) does not have.
use crate::{notify, run_admin, run_console, service_job, tray_log};
use keel_core::traycfg::{Memory, Prefs};
use keel_egpu::apps::{restart_request_path, AdrenalinState, AppWatch};
use keel_egpu::config::Config;
use keel_egpu::gpu::VENDOR_AMD;
use keel_egpu::telemetry::GpuTelemetry;
use keel_egpu::util::{self, status_path, Alert};
use keel_egpu::{display, driver, intscale, legion, trayicons};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tray_icon::menu::{CheckMenuItem, IsMenuItem, MenuId, MenuItem, Submenu};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowRect, MessageBoxW, PostMessageW, SetDlgItemTextW, SetWindowPos, EVENT_OBJECT_SHOW,
    EVENT_SYSTEM_FOREGROUND, IDNO, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, OBJID_WINDOW,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_COMMAND,
};

#[derive(Deserialize, Default)]
#[serde(default)]
struct Adapter { name: String, vendor_id: u32, device_id: u32, luid: u64 }

#[derive(Deserialize, Default)]
#[serde(default)]
struct Status {
    utc: String,
    started_utc: String,
    adapters: Vec<Adapter>,
    telemetry: Option<GpuTelemetry>,
    game: Option<String>,
    game_exe: Option<String>,
    gpu_lock_held: bool,
    alerts: Vec<Alert>,
}

/// Whether the service sees a game running (GPU apps page: folders and programs; any GPU).
pub fn game_running() -> bool { read_status().is_some_and(|s| s.game.is_some()) }

fn read_status() -> Option<Status> {
    serde_json::from_str(&std::fs::read_to_string(status_path()).ok()?).ok()
}

/// The service rewrites the status every 1-2 s; older than 20 s means it is not running.
fn service_alive(s: &Status) -> bool {
    chrono::DateTime::parse_from_rfc3339(&s.utc)
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() < 20)
        .unwrap_or(false)
}

fn amd_hwid(cfg: &Config) -> Option<String> {
    cfg.gpus.iter().find(|g| g.vendor_id == VENDOR_AMD && !g.hardware_id.is_empty()).map(|g| g.hardware_id.clone())
}

fn prefer_gpu(exe: &str) -> anyhow::Result<()> {
    let (key, _) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .create_subkey(r"Software\Microsoft\DirectX\UserGpuPreferences")?;
    key.set_value(exe, &"GpuPreference=2;".to_string())?;
    Ok(())
}

// --- Legion Space window pin: a WinEvent hook moves its windows the moment they show up; the
// once-a-second refresh catches windows Legion Space repositions after showing them. ---

static PIN_ON: AtomicBool = AtomicBool::new(false);
/// Moving windows can work now: the built-in screen is on and is the main display (legion::pin_state).
static PIN_ALLOWED: AtomicBool = AtomicBool::new(false);
const GAVE_UP: &str = "legion space: a window went straight back to another display twice; left there for 10 minutes (no flicker)";

unsafe extern "system" fn on_win_event(_: HWINEVENTHOOK, _: u32, hwnd: HWND, id_object: i32, id_child: i32, _: u32, _: u32) {
    if !PIN_ON.load(Ordering::Relaxed) || !PIN_ALLOWED.load(Ordering::Relaxed) || id_object != OBJID_WINDOW.0 || id_child != 0 { return; }
    if legion::is_legion_window(hwnd) {
        if let Some(m) = display::internal_monitor() {
            match legion::pin_window(hwnd, &m) {
                legion::Pin::Move => tray_log("legion space: window moved to the built-in screen"),
                legion::Pin::GiveUp => tray_log(GAVE_UP),
                legion::Pin::Skip => {}
            }
        }
    }
}

fn install_hooks() {
    for ev in [EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND] {
        unsafe { SetWinEventHook(ev, ev, None, Some(on_win_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS); }
    }
}

fn apply_legion(on: bool) {
    PIN_ON.store(on, Ordering::Relaxed);
    let changed = legion::set_igpu_preference(on);
    for exe in &changed {
        tray_log(&format!("legion space: graphics preference {} for {exe}", if on { "power saving (iGPU)" } else { "cleared" }));
    }
    // The preference applies from the next start of each exe; the quick menu restarts on its own.
    if changed.iter().any(|e| e.ends_with("LegionSettingMenu.exe")) && legion::restart_quick_menu() {
        tray_log("legion space: quick menu ended so the new graphics preference applies on its next start");
    }
    if on && PIN_ALLOWED.load(Ordering::Relaxed) {
        if let Some(m) = display::internal_monitor() { legion::pin_all(&m); }
    }
}

// --- eGPU display modes: Keel owns the displays on the eGPU, Legion Space owns the
// built-in screen. A change from the menu asks to keep it, counting down
// from 15 s on the changed monitor, and goes back (closing the box) when nobody answers. ---

const CONFIRM_TITLE: &str = "Keel: display change";
const CONFIRM_SECS: u64 = 15;
/// Control id of a message box's text.
const MSGBOX_TEXT_ID: i32 = 0xFFFF;

struct PendingMode {
    gdi: String,
    old: display::Mode,
    deadline: Instant,
    /// 0 = no answer yet, 1 = keep, 2 = go back
    answer: Arc<AtomicU8>,
    /// Set when this change switched integer scaling on for that monitor: going back undoes it all.
    int_undo: Option<String>,
}

fn confirm_text(secs: u64) -> String {
    format!("Keep this display mode?\n\nGoing back to the previous mode in {secs} s.")
}

/// The message box runs in its own thread so the tray keeps counting and pumping messages.
fn ask_keep(answer: Arc<AtomicU8>) {
    std::thread::spawn(move || {
        let r = unsafe { MessageBoxW(None, &HSTRING::from(confirm_text(CONFIRM_SECS)), &HSTRING::from(CONFIRM_TITLE),
                                     MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND) };
        answer.store(if r == IDYES { 1 } else { 2 }, Ordering::SeqCst);
    });
}

fn confirm_box() -> Option<HWND> {
    unsafe { FindWindowW(w!("#32770"), &HSTRING::from(CONFIRM_TITLE)) }.ok()
}

/// Updates the countdown and keeps the box centered on the changed monitor (a mode change
/// otherwise leaves it in a corner).
fn update_confirm_box(gdi: &str, secs: u64) {
    let Some(h) = confirm_box() else { return };
    unsafe {
        let _ = SetDlgItemTextW(h, MSGBOX_TEXT_ID, &HSTRING::from(confirm_text(secs)));
        let (Some(m), mut r) = (display::monitor_by_name(gdi), RECT::default()) else { return };
        if GetWindowRect(h, &mut r).is_ok() {
            let x = m.work.left + ((m.work.right - m.work.left) - (r.right - r.left)) / 2;
            let y = m.work.top + ((m.work.bottom - m.work.top) - (r.bottom - r.top)) / 2;
            let _ = SetWindowPos(h, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

fn finish_pending(st: &mut State) {
    let Some(p) = &st.pending else { return };
    let answer = p.answer.load(Ordering::SeqCst);
    if answer == 1 {
        tray_log(&format!("display: kept the new mode on {}", p.gdi));
        st.pending = None;
    } else if answer == 2 || Instant::now() >= p.deadline {
        // Time out: answer "No" for the user. A Yes/No box ignores WM_CLOSE, so press the button.
        if answer == 0 {
            if let Some(h) = confirm_box() {
                unsafe { let _ = PostMessageW(Some(h), WM_COMMAND, WPARAM(IDNO.0 as usize), LPARAM(0)); }
            }
        }
        if let Some(mon) = p.int_undo.clone() {
            let (gdi, old) = (p.gdi.clone(), p.old);
            int_off(st, &mon, Some((gdi, old)));
            st.pending = None;
            st.display_key = None;
            return;
        }
        let r = display::set_mode(&p.gdi, p.old);
        tray_log(&format!("display: went back on {} to {} ({r:?})", p.gdi, p.old));
        st.own_change_until = Some(Instant::now() + Duration::from_secs(3));
        st.pending = None;
        st.display_key = None;
    } else {
        let left = p.deadline.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64;
        update_confirm_box(&p.gdi, left);
    }
}

/// (display, monitor name, current mode) of the displays on the eGPU; changes trigger a rebuild.
type DisplayKey = Vec<(String, String, Option<display::Mode>)>;

fn egpu_displays(egpu_luid: Option<u64>) -> DisplayKey {
    let Some(l) = egpu_luid else { return Vec::new() };
    display::active_displays().into_iter().filter(|d| d.adapter_luid == l && !d.internal)
        .map(|d| { let m = display::current_mode(&d.gdi_name); (d.gdi_name, d.monitor, m) }).collect()
}

fn rebuild_display_menu(ui: &Ui, st: &mut State, key: DisplayKey) {
    while ui.display_menu.remove_at(0).is_some() {}
    st.mode_items.clear();
    st.int_items.clear();
    if key.is_empty() {
        // The eGPU comes from the service's status: without it there is nothing to list.
        let text = if st.alive == Some(false) { "Keel service not running (eGPU unknown)" } else { "No display on the eGPU" };
        let _ = ui.display_menu.append(&MenuItem::new(text, false, None));
    }
    for (gdi, monitor, cur) in &key {
        let name = if monitor.is_empty() { gdi.rsplit('\\').next().unwrap_or(gdi).to_string() } else { monitor.clone() };
        let _ = ui.display_menu.append(&MenuItem::new(name, false, None));
        let modes = display::modes(gdi);
        let on = intscale::load().iter().any(|r| r.monitor == *monitor);
        let first = intscale::native_of(&modes)
            .and_then(|n| intscale::candidates(n, &modes, cur.map(|c| c.hz).unwrap_or(60)).into_iter().next());
        let label = match first {
            Some((k, m)) => format!("    Integer scaling (sharp {k}x: {} x {})", m.w, m.h),
            None => "    Integer scaling (no exact-divisor mode)".to_string(),
        };
        let item = CheckMenuItem::new(label, first.is_some() || on, on, None);
        st.int_items.insert(item.id().clone(), (gdi.clone(), monitor.clone()));
        let _ = ui.display_menu.append(&item);
        for m in modes.into_iter().take(24) {
            let item = CheckMenuItem::new(format!("    {m}"), true, Some(m) == *cur, None);
            st.mode_items.insert(item.id().clone(), (gdi.clone(), m));
            let _ = ui.display_menu.append(&item);
        }
    }
    st.display_key = Some(key);
}

/// Legion Space's resolution option applies to the Windows main display, which can be the eGPU
/// monitor. With the Legion Space pin on, an eGPU-monitor mode change made while Legion Space
/// was in use is undone there and applied to the built-in screen instead.
fn redirect_legion_mode(st: &mut State, gdi: &str, old: display::Mode, new: display::Mode) {
    let back = display::set_mode(gdi, old);
    let Some(int) = display::internal_display_name() else { return };
    let cur_hz = display::current_mode(&int).map(|m| m.hz);
    // Same size on the built-in screen, keeping its refresh rate when it has that mode.
    let pick = display::modes(&int).into_iter().filter(|m| m.w == new.w && m.h == new.h)
        .max_by_key(|m| (Some(m.hz) == cur_hz, m.hz));
    let applied = pick.map(|m| (m, display::set_mode(&int, m)));
    st.own_change_until = Some(Instant::now() + Duration::from_secs(3));
    tray_log(&format!("legion space: changed {gdi} {old} -> {new}; restored it ({back:?}), built-in screen -> {applied:?}"));
    match applied {
        Some((m, Ok(()))) => notify(&format!("Legion Space resolution applied to the built-in screen: {m}")),
        _ => notify(&format!("Legion Space tried to set the eGPU monitor to {new}; restored it. The built-in screen has no such mode.")),
    }
}

/// The eGPU menu items. All exist; the tray's menu shows those among the user's favorites
/// (`Ui::item`), so the logic below never needs to know which are visible.
pub struct Ui {
    pub display_menu: Submenu,
    pub info_egpu: MenuItem,
    pub info_tel: MenuItem,
    pub info_game: MenuItem,
    pub pin_game: MenuItem,
    pub legion_pin: CheckMenuItem,
    pub egpu_primary: CheckMenuItem,
    pub driver: Submenu,
    driver_update: MenuItem,
    wu_block: CheckMenuItem,
    driver_status: MenuItem,
    pub restart_adrenalin: MenuItem,
}

impl Ui {
    pub fn new(prefs: &Prefs) -> anyhow::Result<Ui> {
        let driver_update = MenuItem::new("Update...", true, None);
        let wu_block = CheckMenuItem::new("Block Windows Update", true, false, None);
        let driver_status = MenuItem::new("Status", true, None);
        let driver = Submenu::new("Driver", true);
        driver.append_items(&[&driver_update, &wu_block, &driver_status])?;
        Ok(Ui {
            display_menu: Submenu::new("eGPU display", true),
            info_egpu: MenuItem::new("eGPU: -", false, None),
            info_tel: MenuItem::new(" ", false, None),
            info_game: MenuItem::new("Game: none", false, None),
            pin_game: MenuItem::new("Game on eGPU", false, None),
            legion_pin: CheckMenuItem::new(LEGION_LABEL, true, prefs.legion_space_pin, None),
            egpu_primary: CheckMenuItem::new("eGPU as main display", true, prefs.egpu_primary_display, None),
            driver, driver_update, wu_block, driver_status,
            restart_adrenalin: MenuItem::new("Restart Adrenalin", true, None),
        })
    }

    /// The menu item for a favorite id (`keel_core::traycfg::ITEMS`), if it is an eGPU one.
    pub fn item(&self, id: &str) -> Option<&dyn IsMenuItem> {
        Some(match id {
            "status.egpu" => &self.info_egpu,
            "status.telemetry" => &self.info_tel,
            "status.game" => &self.info_game,
            "egpu.display" => &self.display_menu,
            "egpu.primary" => &self.egpu_primary,
            "egpu.game_gpu" => &self.pin_game,
            "egpu.legion" => &self.legion_pin,
            "egpu.driver" => &self.driver,
            "egpu.adrenalin" => &self.restart_adrenalin,
            _ => return None,
        })
    }
}

const LEGION_LABEL: &str = "Legion Space on iGPU";

pub struct State {
    /// Whether the built-in screen was active at the last refresh (None before the first).
    pin_state: Option<legion::PinState>,
    started: String,
    seen_alert: u64,
    alive: Option<bool>,
    game_exe: Option<String>,
    apps: Option<AppWatch>,
    last_adrenalin_restart: Option<Instant>,
    stuck_notified: bool,
    restart_request_seen: Option<SystemTime>,
    /// Time of the last main-window request taken (`TrayRequest`).
    gui_request_seen: i64,
    /// AMD eGPU attach (LUID) whose remembered Radeon settings were set again, and when it was seen.
    amd_tune: Option<(u64, Instant, bool)>,
    pub prefs: Prefs,
    pub mem: Memory,
    /// Tooltip lines: eGPU, telemetry (empty when none).
    pub tip: Vec<String>,
    display_key: Option<DisplayKey>,
    mode_items: HashMap<MenuId, (String, display::Mode)>,
    /// Integer scaling toggles: (GDI name, monitor name).
    int_items: HashMap<MenuId, (String, String)>,
    /// Profile of the attached eGPU (for the vendor scaling switches).
    egpu_profile: Option<keel_egpu::config::GpuProfile>,
    /// eGPU LUID and when it was first seen; integer scaling is re-applied once it has settled.
    int_seen: Option<(u64, Instant)>,
    /// When to end SearchHost.exe after an integer scaling change (once the display has settled).
    search_restart_at: Option<Instant>,
    /// eGPU display changes made by this tray until then are not Legion Space's.
    own_change_until: Option<Instant>,
    /// Last time a Legion Space window was on screen.
    legion_seen: Option<Instant>,
    pending: Option<PendingMode>,
    /// eGPU displays seen and since when; the primary switch waits until they are stable.
    primary_seen: Option<(Vec<String>, Instant)>,
    /// AMD eGPU present in the last status (None until the service answered).
    amd_egpu: Option<bool>,
    /// When to look at dgtrayicon's tray-icon entries next (it registers a new one per start).
    icon_prune_at: Option<Instant>,
    /// Last time the service reported a running game, and whether one counts as running (exit debounced).
    game_last_seen: Option<Instant>,
    game_active: bool,
    /// (eGPU LUID, game active, eGPU displays) the game hand-over last acted on, and a newer one
    /// waiting to settle.
    int_game_seen: Option<(u64, bool, DisplayKey)>,
    int_game_wait: Option<((u64, bool, DisplayKey), Instant)>,
    /// Game windows already moved to the eGPU monitor (each once; cleared when no game runs).
    game_moved: Vec<isize>,
    /// When the current game was first reported (Legion Space is sent back for a while after).
    game_started: Option<Instant>,
    /// eGPU displays the full-screen window fit last ran for (None while a game runs), and a
    /// newer state waiting to settle.
    fit_seen: Option<DisplayKey>,
    fit_wait: Option<(DisplayKey, Instant)>,
}

/// The eGPU's displays must look the same this long before the primary switch (attach settles).
const PRIMARY_SETTLE: Duration = Duration::from_secs(5);

/// Once per attach: make the eGPU monitor the main display. Each topology change costs dwm memory
/// it does not give back, so this never repeats within an attach, and a primary the user picks by
/// hand afterwards stays until the next attach.
fn egpu_primary(st: &mut State, egpu_luid: Option<u64>) {
    let Some(l) = egpu_luid.filter(|_| st.prefs.egpu_primary_display) else { st.primary_seen = None; return };
    if st.mem.primary_done_luid == l { return; }
    let names: Vec<String> = display::active_displays().into_iter().filter(|d| d.adapter_luid == l && !d.internal)
        .map(|d| d.gdi_name).collect();
    match &st.primary_seen {
        Some((n, t)) if *n == names => if t.elapsed() < PRIMARY_SETTLE { return },
        _ => { st.primary_seen = Some((names, Instant::now())); return; }
    }
    let pos = display::positions();
    let before = display::primary_of(&pos).map(str::to_string);
    match display::primary_step(false, &names, before.as_deref()) {
        display::PrimaryStep::Wait => return,
        display::PrimaryStep::Done => tray_log(&format!("primary display: {} is already on the eGPU", before.as_deref().unwrap_or("-"))),
        display::PrimaryStep::Set(g) => {
            st.own_change_until = Some(Instant::now() + Duration::from_secs(3));
            match display::set_primary(&g) {
                Ok(()) => {
                    tray_log(&format!("primary display: {} -> {g} (eGPU attached; layout {pos:?})", before.as_deref().unwrap_or("-")));
                    notify("The eGPU monitor is now the main display. Apps started from now on render on the eGPU.");
                }
                Err(e) => tray_log(&format!("primary display: switching to {g} failed: {e}")),
            }
        }
    }
    st.mem.primary_done_luid = l;
    st.save_mem();
}

// --- Integer scaling mode (intscale.rs): exact-divisor mode + vendor GPU / Integer Scaling. ---

fn int_log(lines: &[String]) { for l in lines { tray_log(&format!("integer scaling: {l}")); } }

/// Sets the session's font smoothing type if it differs (1 grayscale, 2 ClearType).
fn int_text(t: Option<u32>) {
    let Some(t) = t else { return };
    let now = display::font_smoothing();
    if now == Some(t) { return; }
    let name = |v: Option<u32>| match v { Some(1) => "grayscale", Some(2) => "ClearType", _ => "?" };
    int_log(&[format!("text smoothing {} -> {} ({:?})", name(now), name(Some(t)), display::set_font_smoothing(t))]);
}

/// Sets the Windows scale of an eGPU monitor if it differs (logged; failures leave it as it is).
fn int_scale(gdi: &str, pct: Option<u32>) {
    let Some(pct) = pct else { return };
    let now = display::scale(gdi);
    if now == Some(pct) { return; }
    int_log(&[format!("{gdi}: Windows scale {}% -> {pct}% ({:?})", now.map_or("?".into(), |s| s.to_string()), display::set_scale(gdi, pct))]);
}

/// Records everything first, then GPU / Integer Scaling, then the mode change with the usual
/// keep-or-go-back box (going back undoes the whole switch).
fn int_on(st: &mut State, gdi: &str, monitor: &str) {
    let Some(p) = st.egpu_profile.clone() else { return };
    let Some(scaler) = intscale::scaler_for(&p) else { return };
    let modes = display::modes(gdi);
    let (Some(native), Some(cur)) = (intscale::native_of(&modes), display::current_mode(gdi)) else { return };
    let Some(&(factor, target)) = intscale::candidates(native, &modes, cur.hz).first() else {
        notify("This monitor has no mode that divides its native resolution exactly.");
        return;
    };
    let scaling = match intscale::cli_record(monitor).map(Ok).unwrap_or_else(|| scaler.read(monitor)) {
        Ok(s) => s,
        Err(e) => { int_log(&[format!("{monitor}: {e}")]); notify(&format!("Integer scaling not switched on: {e}")); return; }
    };
    let scale_before = display::scale(gdi);
    let at_native = cur.w == native.w && cur.h == native.h;
    let scale_native = scale_before.map(|s| intscale::native_scale(at_native, s, factor));
    let mut recs = intscale::load();
    recs.retain(|r| r.monitor != monitor);
    recs.push(intscale::Record { gdi: gdi.into(), monitor: monitor.into(), vendor_id: p.vendor_id, native, before: cur, target, factor, scaling,
                                 scale_before, scale_native, smoothing_before: display::font_smoothing() });
    if let Err(e) = intscale::save(&recs) { notify(&format!("Integer scaling not switched on: cannot record the current settings ({e})")); return; }
    // The amd-scaling CLI record is merged into ours; a later `amd-scaling off` must not fight it.
    let _ = std::fs::remove_file(util::user_dir().join("amd-scaling.json"));
    int_log(&[format!("{monitor} on {gdi}: native {native}, before {cur}, target {target} ({factor}x)")]);
    match scaler.enable(monitor) {
        Ok(l) => int_log(&l),
        Err(e) => { int_log(&[format!("{monitor}: {e}")]); int_off(st, monitor, None); notify(&format!("Integer scaling not switched on: {e}")); return; }
    }
    // NVIDIA's display-config write can renumber the GDI names: look the monitor up again.
    let gdi = &display::gdi_of(monitor).unwrap_or_else(|| gdi.to_string());
    if let Some(l) = st.int_seen.map(|(l, _)| l) { st.mem.int_done_luid = l; st.save_mem(); }
    int_text(intscale::smoothing_while_on(recs.last().and_then(|r| r.smoothing_before)));
    st.search_restart_at = Some(Instant::now() + SEARCH_RESTART_DELAY);
    if cur == target {
        int_scale(gdi, scale_native.map(|s| intscale::scale_for_factor(s, factor)));
        notify(&format!("Integer scaling on: {} x {} shown at {factor}x.", target.w, target.h));
        return;
    }
    st.own_change_until = Some(Instant::now() + Duration::from_secs(3));
    match display::set_mode(gdi, target) {
        Ok(()) => {
            int_log(&[format!("{gdi}: {cur} -> {target}")]);
            // The scale is relative to the recommended one, which follows the mode: set it after.
            int_scale(gdi, scale_native.map(|s| intscale::scale_for_factor(s, factor)));
            let answer = Arc::new(AtomicU8::new(0));
            ask_keep(answer.clone());
            st.pending = Some(PendingMode { gdi: gdi.into(), old: cur, deadline: Instant::now() + Duration::from_secs(CONFIRM_SECS),
                                            answer, int_undo: Some(monitor.into()) });
        }
        Err(e) => { int_log(&[format!("{gdi}: mode change failed: {e}")]); int_off(st, monitor, None); }
    }
}

/// Windows Search is restarted this long after an integer scaling change, once displays settled.
const SEARCH_RESTART_DELAY: Duration = Duration::from_secs(3);

fn restart_search(st: &mut State) {
    if !st.search_restart_at.is_some_and(|t| Instant::now() >= t) { return; }
    st.search_restart_at = None;
    if !st.prefs.restart_search_after_scaling { return; }
    if let Some(a) = st.apps.as_mut() {
        let n = a.end_search_host();
        if n > 0 { tray_log(&format!("integer scaling: ended SearchHost.exe ({n}) so Windows Search starts fresh")); }
    }
}

/// Puts the mode back (`back`, or the native size at the old refresh rate) and the recorded
/// scaling settings, then forgets the monitor.
fn int_off(st: &mut State, monitor: &str, back: Option<(String, display::Mode)>) {
    let mut recs = intscale::load();
    let Some(r) = recs.iter().find(|r| r.monitor == monitor).cloned() else { return };
    let gdi = back.as_ref().map(|b| b.0.clone()).or_else(|| egpu_displays(st.int_seen.map(|(l, _)| l)).into_iter()
        .find(|(_, m, _)| *m == monitor).map(|(g, _, _)| g));
    if let Some(gdi) = gdi {
        let modes = display::modes(&gdi);
        // Going back from the confirm box: the exact mode and scale from before; else native.
        let (want, scale) = match &back {
            Some((_, m)) => (*m, r.scale_before),
            None => (intscale::restore_mode(r.native, &modes, r.before), r.scale_native),
        };
        if display::current_mode(&gdi) != Some(want) {
            st.own_change_until = Some(Instant::now() + Duration::from_secs(3));
            int_log(&[format!("{gdi}: back to {want} ({:?})", display::set_mode(&gdi, want))]);
        }
        int_scale(&gdi, scale);
    }
    int_text(r.smoothing_before);
    let restored = st.egpu_profile.as_ref().filter(|p| p.vendor_id == r.vendor_id).and_then(intscale::scaler_for)
        .map(|s| s.restore(&r.scaling));
    match restored {
        Some(Ok(l)) => int_log(&l),
        Some(Err(e)) => int_log(&[format!("{monitor}: restoring scaling failed: {e}")]),
        None => int_log(&[format!("{monitor}: eGPU not attached; scaling settings left as they are")]),
    }
    recs.retain(|x| x.monitor != monitor);
    let _ = intscale::save(&recs);
    if st.mem.int_game_handover.iter().any(|m| m == monitor) {
        st.mem.int_game_handover.retain(|m| m != monitor);
        st.save_mem();
    }
    st.search_restart_at = Some(Instant::now() + SEARCH_RESTART_DELAY);
    notify(&format!("Integer scaling off for {monitor}."));
}

/// Once per AMD eGPU attach, after it settled: the Radeon settings the user chose in the main window
/// (`keel egpu amd-tune apply`, user session; drivers can drop them on a reset or an update).
fn amd_reapply(st: &mut State, luid: Option<u64>) {
    let Some(l) = luid else { st.amd_tune = None; return };
    match st.amd_tune {
        Some((x, _, _)) if x != l => st.amd_tune = Some((l, Instant::now(), false)),
        None => st.amd_tune = Some((l, Instant::now(), false)),
        _ => {}
    }
    let Some((_, since, done)) = st.amd_tune else { return };
    if done || since.elapsed() < PRIMARY_SETTLE || st.game_active { return; }
    st.amd_tune = Some((l, since, true));
    if !util::user_dir().join("amd-tuning.json").exists() { return; }
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("keel.exe"))).unwrap_or_else(|| "keel.exe".into());
    std::thread::spawn(move || {
        match keel_core::cmd::hidden(exe).args(["egpu", "amd-tune", "apply"]).output() {
            Ok(o) => for l in String::from_utf8_lossy(&o.stdout).lines() { tray_log(&format!("amd settings after attach: {l}")); },
            Err(e) => tray_log(&format!("amd settings after attach: {e}")),
        }
    });
}

/// Once per attach: monitors with integer scaling on get their target mode and scaling back
/// if Windows or the driver reset them.
fn int_reapply(st: &mut State, egpu_luid: Option<u64>) {
    let Some(l) = egpu_luid else { st.int_seen = None; return };
    match st.int_seen { Some((x, _)) if x == l => {}, _ => { st.int_seen = Some((l, Instant::now())); return; } }
    if st.mem.int_done_luid == l || st.int_seen.is_some_and(|(_, t)| t.elapsed() < PRIMARY_SETTLE) { return; }
    // A running game owns the mode; the game hand-over (int_game) covers this attach then.
    if st.game_active { return; }
    let recs = intscale::load();
    let Some(p) = st.egpu_profile.clone() else { return };
    let shown = egpu_displays(Some(l));
    if shown.is_empty() { return; }
    for r in recs.iter().filter(|r| r.vendor_id == p.vendor_id) {
        let Some((gdi, _, cur)) = shown.iter().find(|(_, m, _)| *m == r.monitor) else { continue };
        let Some(scaler) = intscale::scaler_for(&p) else { continue };
        if scaler.read(&r.monitor).map(|s| s.integer_scaling != Some(true)).unwrap_or(false) {
            int_log(&scaler.enable(&r.monitor).unwrap_or_else(|e| vec![format!("{}: {e}", r.monitor)]));
        }
        let gdi = &display::gdi_of(&r.monitor).unwrap_or_else(|| gdi.clone());
        if *cur != Some(r.target) {
            st.own_change_until = Some(Instant::now() + Duration::from_secs(3));
            int_log(&[format!("{gdi}: re-applied {} after attach ({:?})", r.target, display::set_mode(gdi, r.target))]);
            int_scale(gdi, r.scale_native.map(|s| intscale::scale_for_factor(s, r.factor)));
            st.search_restart_at = Some(Instant::now() + SEARCH_RESTART_DELAY);
            st.display_key = None;
        }
        st.mem.int_game_handover.retain(|m| *m != r.monitor);
    }
    st.mem.int_done_luid = l;
    st.save_mem();
}

/// A game counts as exited only after the service has not reported one for this long (games
/// restart themselves, e.g. Monster Hunter World ended and started again within 8 s).
const GAME_EXIT_SETTLE: Duration = Duration::from_secs(15);

/// The game's display modes must look the same this long before Integer Scaling follows them
/// (games switch modes a few times while starting).
const GAME_MODE_SETTLE: Duration = Duration::from_secs(2);

/// Game hand-over (intscale::game_step). While a game runs, Integer Scaling follows the game's
/// mode: on for exact divisors of the native mode (sharp), off otherwise (GPU Scaling stays, so
/// 1440p fills the panel instead of sitting 1x in black borders). The game's mode itself is never
/// changed. Once it has exited (debounced), the integer mode's desktop mode and Integer Scaling
/// come back. Acts once per settled change of (attach, game, modes); the record is untouched.
fn int_game(st: &mut State, egpu_luid: Option<u64>, game: Option<&str>, key: &DisplayKey) {
    let Some(l) = egpu_luid else { st.int_game_seen = None; st.int_game_wait = None; return };
    let sig = (l, st.game_active, key.clone());
    if st.int_game_seen.as_ref() == Some(&sig) { return; }
    // After an attach, wait until the displays settled (int_reapply's clock).
    if !st.int_seen.is_some_and(|(x, t)| x == l && t.elapsed() >= PRIMARY_SETTLE) { return; }
    match &st.int_game_wait {
        Some((w, t)) if *w == sig => if t.elapsed() < GAME_MODE_SETTLE { return },
        _ => { st.int_game_wait = Some((sig, Instant::now())); return; }
    }
    let Some(p) = st.egpu_profile.clone() else { return };
    st.int_game_wait = None;
    let active = st.game_active;
    st.int_game_seen = Some(sig);
    // AMD applies a scaling change only at the next mode change: switched under a running game it
    // did nothing visible (1440p stayed small in black borders) and the display re-init upset the
    // game's mouse. Leave AMD scaling as it is while a game runs (run games at an exact divisor,
    // e.g. 1080p on 4K, for a sharp full screen).
    if active && p.vendor_id == keel_egpu::gpu::VENDOR_AMD && st.mem.int_game_handover.is_empty() {
        int_log(&[format!("game running ({}): AMD integer scaling left as it is until it exits", game.unwrap_or("?"))]);
        return;
    }
    let recs = intscale::load();
    let before = st.mem.int_game_handover.clone();
    st.mem.int_game_handover.retain(|m| recs.iter().any(|r| r.monitor == *m));
    for r in recs.iter().filter(|r| r.vendor_id == p.vendor_id) {
        let Some((gdi, _, cur)) = key.iter().find(|(_, m, _)| *m == r.monitor) else { continue };
        let in_game = st.mem.int_game_handover.contains(&r.monitor);
        if !active && !in_game { continue; }
        let Some(scaler) = intscale::scaler_for(&p) else { continue };
        let integer = match scaler.read(&r.monitor) {
            Ok(s) => s.integer_scaling,
            Err(e) => { int_log(&[format!("{}: game hand-over: {e}", r.monitor)]); continue; }
        };
        let (acts, now) = intscale::game_step(true, active, in_game, *cur, r.native, r.target, integer);
        st.mem.int_game_handover.retain(|m| *m != r.monitor);
        if now { st.mem.int_game_handover.push(r.monitor.clone()); }
        let cur_text = cur.map_or("?".into(), |m| m.to_string());
        if active && !in_game { int_log(&[format!("{}: game started ({}) at {cur_text}", r.monitor, game.unwrap_or("?"))]); }
        if acts.is_empty() {
            if !active { int_log(&[format!("{}: game exited ({} s settled); {cur_text} with Integer Scaling already as it should be", r.monitor, GAME_EXIT_SETTLE.as_secs())]); }
            continue;
        }
        int_log(&[if active {
            let fits = cur.is_some_and(|c| intscale::exact_divisor(r.native, c));
            format!("{}: game at {cur_text} {}", r.monitor,
                    if fits { "divides the native mode: Integer Scaling on" } else { "does not divide the native mode: Integer Scaling off, GPU Scaling fills the panel" })
        } else {
            format!("{}: game exited ({} s settled) at {cur_text}: back to {} with Integer Scaling", r.monitor, GAME_EXIT_SETTLE.as_secs(), r.target)
        }]);
        for a in acts {
            match a {
                intscale::GameAction::IntegerOff => int_log(&scaler.integer_off(&r.monitor).unwrap_or_else(|e| vec![format!("{}: {e}", r.monitor)])),
                intscale::GameAction::IntegerOn => int_log(&scaler.enable(&r.monitor).unwrap_or_else(|e| vec![format!("{}: {e}", r.monitor)])),
                intscale::GameAction::SetMode(m) => {
                    let gdi = &display::gdi_of(&r.monitor).unwrap_or_else(|| gdi.clone());
                    st.own_change_until = Some(Instant::now() + Duration::from_secs(3));
                    int_log(&[format!("{gdi}: {cur_text} -> {m} ({:?})", display::set_mode(gdi, m))]);
                    int_scale(gdi, r.scale_native.map(|s| intscale::scale_for_factor(s, r.factor)));
                    st.search_restart_at = Some(Instant::now() + SEARCH_RESTART_DELAY);
                    st.display_key = None;
                }
            }
        }
    }
    if before != st.mem.int_game_handover { st.save_mem(); }
}

/// eGPU display modes must look the same this long (and no game run) before full-screen windows
/// are fitted: a game's last mode change, the game hand-over's mode change and Windows' own
/// re-layout come first.
const FIT_SETTLE: Duration = Duration::from_secs(3);

/// Full-screen windows a mode change left at another mode's size (Steam Big Picture stayed
/// 2560x1440 after a game switched the desktop to 1440 and back) are put back to the eGPU
/// monitor's current size (`display::fit_target`). Runs once per settled change of the eGPU
/// displays and after each game, never while a game runs (its own window is the game's).
/// A game set to the eGPU (Windows' per-app GPU choice) renders there but opens on the main
/// display; when that is another screen (the built-in one), each of its windows is moved once to
/// the first eGPU monitor. Once: a game that moves itself back keeps its choice (no fight).
fn game_to_egpu(st: &mut State, key: &DisplayKey) {
    let Some(exe) = st.game_exe.clone().filter(|_| st.prefs.game_to_egpu_screen) else { st.game_moved.clear(); return };
    let Some((target, monitor, _)) = key.first() else { return };
    if !keel_egpu::gpupref::is_high_performance(&exe) { return; }
    for (hwnd, gdi) in display::exe_windows(&exe) {
        if st.game_moved.contains(&hwnd) || key.iter().any(|(g, _, _)| g.eq_ignore_ascii_case(&gdi)) { continue; }
        st.game_moved.push(hwnd);
        let ok = display::move_window(hwnd, &gdi, target);
        tray_log(&format!("game: window of {} moved from {gdi} to {monitor} (eGPU){}", exe.rsplit('\\').next().unwrap_or(&exe), if ok { "" } else { ": failed" }));
    }
}

/// Legion Space opens its own screen when a game starts; for this long after a game was first
/// reported, a Legion Space window in front is minimized so the game keeps the screen. Later ones
/// (the Legion button pressed in the game) are left alone.
const LEGION_HIDE_FOR: Duration = Duration::from_secs(20);

fn game_hide_legion(st: &mut State) {
    if !st.prefs.game_hide_legion || !st.game_started.is_some_and(|t| t.elapsed() < LEGION_HIDE_FOR) { return; }
    if legion::minimize_if_foreground() { tray_log("game: Legion Space came to the front as the game started; minimized"); }
}

fn fit_windows(st: &mut State, key: &DisplayKey) {
    if st.game_active { st.fit_seen = None; st.fit_wait = None; return; }
    if key.is_empty() || st.fit_seen.as_ref() == Some(key) { return; }
    match &st.fit_wait {
        Some((k, t)) if k == key => if t.elapsed() < FIT_SETTLE { return },
        _ => { st.fit_wait = Some((key.clone(), Instant::now())); return; }
    }
    st.fit_wait = None;
    st.fit_seen = Some(key.clone());
    for (gdi, _, _) in key {
        for l in display::fit_fullscreen(gdi) { tray_log(&format!("full-screen window on {gdi}: {l}")); }
    }
}

/// dgtrayicon restarts on eGPU attach / detach; give its new icon time to register first.
const ICON_PRUNE_DELAY: Duration = Duration::from_secs(45);

fn prune_tray_icons(st: &mut State) {
    if !st.icon_prune_at.is_some_and(|t| Instant::now() >= t) { return; }
    st.icon_prune_at = None;
    if !st.prefs.prune_amd_tray_icons { return; }
    for line in trayicons::prune(&util::user_dir().join("tray-icons-backup")) { tray_log(&line); }
}

fn request_mtime() -> Option<SystemTime> {
    std::fs::metadata(restart_request_path()).and_then(|m| m.modified()).ok()
}

fn restart_adrenalin(st: &mut State, why: &str) {
    let Some(apps) = st.apps.as_mut() else { return };
    let msg = apps.restart_adrenalin();
    tray_log(&format!("{why}: {msg}"));
    notify(&format!("{why} - {msg}"));
    st.last_adrenalin_restart = Some(Instant::now());
    st.stuck_notified = false;
}

/// Every few seconds: log other tuning apps starting/stopping, restart a stuck Adrenalin
/// (auto, at most every 5 min, if enabled) and honour driver-update's restart request.
fn watch_apps(st: &mut State, cfg: &Config) {
    if let Some(m) = request_mtime() {
        if st.restart_request_seen.is_some_and(|seen| m > seen) {
            st.restart_request_seen = Some(m);
            restart_adrenalin(st, "Restarting Adrenalin after the driver update");
            return;
        }
        st.restart_request_seen.get_or_insert(m);
    }
    let Some(apps) = st.apps.as_mut() else { return };
    let (state, changes) = apps.check();
    for c in &changes { tray_log(c); }
    if state != AdrenalinState::Stuck { st.stuck_notified = false; return; }
    let cooled = st.last_adrenalin_restart.is_none_or(|t| t.elapsed() >= Duration::from_secs(300));
    if cfg.adrenalin_autorestart && cooled {
        restart_adrenalin(st, "Adrenalin was stuck, restarting it");
    } else if !st.stuck_notified {
        notify("Adrenalin looks stuck. Use Restart Adrenalin in the tray or the main window.");
        tray_log("adrenalin: looks stuck (no window after launch)");
        st.stuck_notified = true;
    }
}

fn telemetry_text(s: &Status) -> String { s.telemetry.as_ref().map(|t| t.summary()).unwrap_or_default() }

/// Once a second (and after menu actions): status, alerts, menu texts, the display work.
pub fn refresh(ui: &Ui, st: &mut State, cfg: &Config) {
    let status = read_status();
    let alive = status.as_ref().is_some_and(service_alive);
    if st.alive != Some(alive) {
        if st.alive.is_some() {
            if st.prefs.notifications { notify(if alive { "Service is running again" } else { "Service is not responding" }); }
        }
        st.alive = Some(alive);
    }
    let s = status.unwrap_or_default();

    // New alerts -> notifications. Alert ids restart with the service.
    if s.started_utc != st.started {
        let first = st.started.is_empty();
        st.started = s.started_utc.clone();
        st.seen_alert = if first { s.alerts.last().map(|a| a.id).unwrap_or(0) } else { 0 };
    }
    let seen = st.seen_alert;
    for a in s.alerts.iter().filter(|a| a.id > seen) {
        if st.prefs.notifications { notify(&a.msg); }
        st.seen_alert = a.id;
    }

    let egpu = s.adapters.iter().find(|a| cfg.profile_for(a.vendor_id, a.device_id).is_some());
    if alive && !s.adapters.is_empty() {
        let amd = egpu.is_some_and(|a| a.vendor_id == VENDOR_AMD);
        if st.amd_egpu.is_some_and(|was| was != amd) { st.icon_prune_at = Some(Instant::now() + ICON_PRUNE_DELAY); }
        st.amd_egpu = Some(amd);
    }
    let egpu_text = match (alive, egpu) {
        (false, _) => "eGPU: service not running".to_string(),
        (true, None) => "eGPU: not connected".to_string(),
        (true, Some(a)) => format!("eGPU: {}", a.name),
    };
    let tel_text = if alive && egpu.is_some() { telemetry_text(&s) } else { String::new() };
    let game_text = match &s.game {
        Some(g) => format!("Game: {g}{}", if s.gpu_lock_held { " (holds GPU lock)" } else { "" }),
        None => "Game: none".to_string(),
    };
    ui.info_egpu.set_text(&egpu_text);
    ui.info_tel.set_text(if tel_text.is_empty() { " " } else { &tel_text });
    ui.info_game.set_text(&game_text);
    if s.game_exe.is_some() != st.game_exe.is_some() { st.game_started = s.game_exe.is_some().then(Instant::now); }
    st.game_exe = s.game_exe.clone();
    ui.pin_game.set_enabled(st.game_exe.is_some());

    let hwid = amd_hwid(cfg);
    ui.wu_block.set_checked(hwid.as_deref().is_some_and(driver::is_blocked));
    ui.wu_block.set_enabled(hwid.is_some());

    st.tip = vec![egpu_text, tel_text];

    // Windows are moved only while the built-in screen is on and is the main display; with an
    // external main display Legion Space puts its windows back (flicker), so they are left alone
    // and only its iGPU graphics preference stays (legion::pin_state).
    let internal = display::internal_display_name();
    let internal_on = internal.is_some();
    let pos = display::positions();
    let state = legion::pin_state(internal.as_deref(), display::primary_of(&pos));
    PIN_ALLOWED.store(state == legion::PinState::On, Ordering::Relaxed);
    if st.pin_state != Some(state) {
        if st.pin_state.is_some() { tray_log(&format!("legion space: window pin {state:?}")); }
        st.pin_state = Some(state);
        // Works only while the built-in screen is the main display: greyed out otherwise (the
        // menu cannot show a tooltip; the main window shows the reason).
        ui.legion_pin.set_enabled(state == legion::PinState::On);
    }

    if st.prefs.legion_space_pin && internal_on {
        if state == legion::PinState::On {
            if let Some(m) = display::internal_monitor() {
                if legion::pin_all(&m).1 > 0 { tray_log(GAVE_UP); }
            }
        }
        if legion::in_use() { st.legion_seen = Some(Instant::now()); }
    }

    finish_pending(st);
    if st.pending.is_some() { return; }
    gui_request(st);
    if st.pending.is_some() { return; }
    egpu_primary(st, egpu.filter(|_| alive).map(|a| a.luid).filter(|l| *l != 0));
    st.egpu_profile = egpu.filter(|_| alive).and_then(|a| cfg.profile_for(a.vendor_id, a.device_id)).cloned();
    if alive && s.game.is_some() { st.game_last_seen = Some(Instant::now()); }
    st.game_active = st.game_last_seen.is_some_and(|t| t.elapsed() < GAME_EXIT_SETTLE);
    int_reapply(st, egpu.filter(|_| alive).map(|a| a.luid).filter(|l| *l != 0));
    amd_reapply(st, egpu.filter(|a| alive && a.vendor_id == VENDOR_AMD).map(|a| a.luid).filter(|l| *l != 0));
    let key = egpu_displays(egpu.filter(|_| alive).map(|a| a.luid).filter(|l| *l != 0));
    int_game(st, egpu.filter(|_| alive).map(|a| a.luid).filter(|l| *l != 0), s.game.as_deref(), &key);
    fit_windows(st, &key);
    game_to_egpu(st, &key);
    game_hide_legion(st);
    if st.display_key.as_ref() != Some(&key) {
        let ours = st.own_change_until.is_some_and(|t| Instant::now() < t);
        // A running game changes the eGPU monitor's mode itself: never take that for Legion Space's.
        let legion = st.prefs.legion_space_pin && !st.game_active && st.legion_seen.is_some_and(|t| t.elapsed() < Duration::from_secs(10));
        if let (Some(prev), false, true) = (st.display_key.clone(), ours, legion) {
            for (gdi, _, now) in &key {
                let before = prev.iter().find(|(g, _, _)| g == gdi).and_then(|(_, _, m)| *m);
                if let (Some(old), Some(new)) = (before, *now) {
                    if old != new { redirect_legion_mode(st, gdi, old, new); }
                }
            }
            // Rebuild from the restored modes on the next tick.
            st.display_key = None;
            return;
        }
        rebuild_display_menu(ui, st, key);
    }
}

/// Sets a display mode with the keep-or-go-back box (menu and main window).
fn set_mode_asking(st: &mut State, gdi: String, m: display::Mode) {
    let old = display::current_mode(&gdi);
    st.own_change_until = Some(Instant::now() + Duration::from_secs(3));
    match (old, display::set_mode(&gdi, m)) {
        (Some(old), Ok(())) if old != m => {
            tray_log(&format!("display: {gdi} {old} -> {m}"));
            let answer = Arc::new(AtomicU8::new(0));
            ask_keep(answer.clone());
            st.pending = Some(PendingMode { gdi, old, deadline: Instant::now() + Duration::from_secs(CONFIRM_SECS), answer, int_undo: None });
        }
        (_, Err(e)) => notify(&format!("Display mode not changed: {e}")),
        _ => {}
    }
    st.display_key = None; // rebuild check marks after the change settles
}

/// A display job from the main window's Displays page (`TrayRequest`), done like the menu's.
fn gui_request(st: &mut State) {
    let Some(r) = keel_core::traycfg::TrayRequest::take(&mut st.gui_request_seen) else { return };
    let Some(gdi) = display::gdi_of(&r.monitor) else { tray_log(&format!("main window: {} is not shown now", r.monitor)); return };
    tray_log(&format!("main window: {} {} {}", r.cmd, r.monitor, if r.cmd == "integer" { r.on.to_string() } else { format!("{:?}", r.mode) }));
    match r.cmd.as_str() {
        "mode" => if let Some((w, h, hz)) = r.mode { set_mode_asking(st, gdi, display::Mode { w, h, hz }); },
        "integer" => {
            let on_now = intscale::load().iter().any(|x| x.monitor == r.monitor);
            if r.on == on_now { return; }
            // Turning it off puts the desktop mode back, under the running game (its mouse then
            // maps to the wrong size); the game hand-over already follows the game's mode.
            if st.game_active { notify("A game is running: integer scaling follows the game now; switch it after the game exits."); }
            else if !r.on { int_off(st, &r.monitor, None); }
            else if st.egpu_profile.is_none() || !egpu_displays(st.int_seen.map(|(l, _)| l)).iter().any(|(_, m, _)| *m == r.monitor) {
                notify("Integer scaling here works on eGPU monitors; the built-in screen has its own switch in the main window.");
            }
            else { int_on(st, &gdi, &r.monitor); }
            st.display_key = None;
        }
        other => tray_log(&format!("main window: unknown request {other}")),
    }
}

/// A click on one of the eGPU items; false if the id is not one of them.
pub fn handle(ui: &Ui, st: &mut State, cfg: &Config, id: &MenuId) -> bool {
    if let Some((gdi, monitor)) = st.int_items.get(id).cloned() {
        if st.pending.is_none() {
            // On: it would record the game's mode as the one to go back to and fight its mode
            // changes. Off: the desktop mode would change under the game (mouse mapped wrong).
            if st.game_active { notify("A game is running: integer scaling follows the game now; switch it after the game exits."); }
            else if intscale::load().iter().any(|r| r.monitor == monitor) { int_off(st, &monitor, None); }
            else { int_on(st, &gdi, &monitor); }
        }
        st.display_key = None;
        return true;
    }
    if let Some((gdi, m)) = st.mode_items.get(id).cloned() {
        if st.pending.is_none() { set_mode_asking(st, gdi, m); }
        return true;
    }
    if id == ui.pin_game.id() {
        if let Some(exe) = &st.game_exe {
            match prefer_gpu(exe) {
                Ok(()) => notify(&format!("Runs on the eGPU from its next start: {exe}")),
                Err(e) => notify(&format!("Could not set the GPU: {e}")),
            }
        }
    } else if id == ui.legion_pin.id() {
        let on = ui.legion_pin.is_checked();
        st.prefs.legion_space_pin = on;
        st.save_prefs();
        apply_legion(on);
        // The ADL redirect lives in Program Files, so it goes through UAC once.
        if on != cfg.legion_space_fix {
            service_job(serde_json::json!({"cmd": "legion-fix", "on": on}), if on { "egpu legion-fix on" } else { "egpu legion-fix off" });
        }
        if on {
            notify("Legion Space now stays on the iGPU and the built-in screen. Its own settings are not changed. Restart Legion Space once.");
        }
        tray_log(&format!("legion space pin: {}", if on { "on" } else { "off" }));
    } else if id == ui.egpu_primary.id() {
        let on = ui.egpu_primary.is_checked();
        st.prefs.egpu_primary_display = on;
        st.mem.primary_done_luid = 0; // switching it on acts on the current attach too
        st.save_prefs();
        st.save_mem();
        tray_log(&format!("eGPU main display: {}", if on { "on" } else { "off" }));
    } else if id == ui.driver_update.id() {
        let picked = rfd::FileDialog::new()
            .set_title("Select the AMD driver installer (.exe) or .inf")
            .add_filter("AMD driver", &["exe", "inf"])
            .pick_file();
        if let Some(p) = picked {
            run_admin(&format!("egpu driver-update \"{}\"", p.display()), true);
        }
    } else if id == ui.wu_block.id() {
        // The click already flipped the check mark; act on the real current state.
        if let Some(h) = amd_hwid(cfg) {
            let block = !driver::is_blocked(&h);
            service_job(serde_json::json!({"cmd": "driver-block", "block": block}), if block { "egpu driver-block" } else { "egpu driver-unblock" });
        }
    } else if id == ui.driver_status.id() {
        run_console("egpu driver-status");
    } else if id == ui.restart_adrenalin.id() {
        restart_adrenalin(st, "Restarting Adrenalin from the menu");
    } else {
        return false;
    }
    true
}


impl State {
    pub fn new(prefs: Prefs) -> Self {
        State { pin_state: None, started: String::new(), seen_alert: 0, alive: None, game_exe: None,
                apps: AppWatch::new(), last_adrenalin_restart: None, stuck_notified: false,
                restart_request_seen: request_mtime(), gui_request_seen: 0, amd_tune: None, prefs, mem: Memory::load(), tip: Vec::new(), display_key: None,
                mode_items: HashMap::new(), int_items: HashMap::new(), egpu_profile: None, int_seen: None, search_restart_at: None,
                own_change_until: None, legion_seen: None, pending: None, primary_seen: None, amd_egpu: None,
                icon_prune_at: Some(Instant::now() + ICON_PRUNE_DELAY), game_last_seen: None, game_active: false,
                int_game_seen: None, int_game_wait: None, game_moved: Vec::new(), game_started: None, fit_seen: None, fit_wait: None }
    }

    fn save_mem(&self) { if let Err(e) = self.mem.save() { tray_log(&format!("tray-state.toml: {e:#}")); } }
    fn save_prefs(&self) { if let Err(e) = self.prefs.save() { tray_log(&format!("tray.toml: {e:#}")); } }

    /// The main window changed `tray.toml`: take its options (Legion Space pin applied at once).
    pub fn set_prefs(&mut self, ui: &Ui, p: Prefs) {
        if p.legion_space_pin != self.prefs.legion_space_pin { apply_legion(p.legion_space_pin); }
        if p.egpu_primary_display && !self.prefs.egpu_primary_display { self.mem.primary_done_luid = 0; }
        ui.legion_pin.set_checked(p.legion_space_pin);
        ui.egpu_primary.set_checked(p.egpu_primary_display);
        self.prefs = p;
    }
}

/// At tray start: hooks for the Legion Space pin, and the pin itself if it is on.
pub fn start(prefs: &Prefs) {
    install_hooks();
    if prefs.legion_space_pin { apply_legion(true); }
}

/// The slower jobs of the loop: tuning apps / Adrenalin every 5 s, tray-icon pruning, Windows
/// Search restart, the Legion Space preference once a minute (new versions use new folders).
pub fn tick(st: &mut State, cfg: &Config, ticks: u32) {
    if ticks % 60 == 0 && st.prefs.legion_space_pin { apply_legion(true); }
    if ticks % 5 == 0 { watch_apps(st, cfg); }
    prune_tray_icons(st);
    restart_search(st);
}

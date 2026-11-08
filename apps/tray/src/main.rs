//! keel-tray: Keel's tray icon, one per user session. The menu holds only the user's favorites
//! (picked in the main window's Settings, `keel_core::traycfg`) plus "Open Keel" and "Quit"; the
//! tooltip shows free memory, the eGPU and the network. It also hosts the user-session work the
//! service cannot do: the eGPU display / Legion Space / Adrenalin logic (`egpu.rs`) and the memory
//! observer (5-minute samples, display / power / TDR events). One loop, woken by window messages or
//! once a second.
#![windows_subsystem = "windows"]

mod egpu;

use keel_core::traycfg::{self, Prefs};
use keel_net::model::Side;
use keel_ram::ondemand::{Request, State as OdState};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tauri_winrt_notification::Toast;
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, SHOW_WINDOW_CMD,
    SW_HIDE, SW_SHOWNORMAL, WM_QUIT,
};

// ------------------------------------------------------------------ helpers (also used by egpu.rs)

pub fn tray_log(msg: &str) { keel_core::log::user(msg); }

/// Shown as "Keel" once the installer's Start-menu shortcut registers the AppUserModelID.
pub fn notify(msg: &str) {
    let _ = Toast::new(keel_egpu::util::APP_ID).title("Keel").text1(msg).show();
    keel_core::trim::working_set();
}

/// After each memory sample: warn once when dwm holds too much system RAM.
fn dwm_ram_check(s: &keel_ram::observe::Sample) {
    static WARNED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
    if let Some(msg) = keel_ram::dwmwatch::check(s, &mut WARNED.lock().unwrap()) {
        tray_log(&format!("dwm system RAM: {} MB private", keel_ram::dwmwatch::private_bytes(s).unwrap_or(0) >> 20));
        notify(&msg);
    }
}

fn exe_beside(name: &str) -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(name))).unwrap_or_else(|| PathBuf::from(name))
}

fn shell(verb: PCWSTR, file: &str, params: &str, show: SHOW_WINDOW_CMD) {
    unsafe { ShellExecuteW(None, verb, &HSTRING::from(file), &HSTRING::from(params), PCWSTR::null(), show); }
    keel_core::trim::working_set();
}

/// Runs a keel command elevated (UAC prompt). `visible` keeps a console open for its output.
pub fn run_admin(args: &str, visible: bool) {
    let exe = exe_beside("keel.exe").display().to_string();
    if visible { shell(w!("runas"), "cmd.exe", &format!("/k \"\"{exe}\" {args}\""), SW_SHOWNORMAL); }
    else { shell(w!("runas"), &exe, args, SW_HIDE); }
}

/// An admin job done by the service (no UAC prompt): `body` is a keel-service request such as
/// `{"cmd":"legion-fix","on":true}`. Only when the service cannot be reached does it fall back to
/// `keel <fallback>` elevated (UAC).
pub fn service_job(body: serde_json::Value, fallback: &str) {
    match keel_core::pipe::call("service", &body) {
        Ok(r) => {
            let ok = r["ok"].as_bool().unwrap_or(false);
            let msg = r["message"].as_str().unwrap_or("").to_string();
            tray_log(&format!("service job {body}: {}", if ok { msg.clone() } else { format!("failed: {msg}") }));
            if !ok { notify(&format!("Keel service: {msg}")); }
        }
        Err(e) => {
            tray_log(&format!("service job {body}: service not reachable ({e:#}); asking for admin rights"));
            run_admin(fallback, false);
        }
    }
}

/// Opens a console with the output of a (non-admin) keel command.
pub fn run_console(args: &str) {
    let exe = exe_beside("keel.exe").display().to_string();
    shell(w!("open"), "cmd.exe", &format!("/k \"\"{exe}\" {args}\""), SW_SHOWNORMAL);
}

fn open_path(p: &Path) { shell(w!("open"), &p.display().to_string(), "", SW_SHOWNORMAL); }

fn open_gui(tab: Option<&str>) {
    let mut c = keel_core::cmd::windowed(exe_beside("keel-gui.exe"));
    if let Some(t) = tab { c.args(["--tab", t]); }
    // The menu click gave the tray the foreground: pass it on, or the window opens behind others.
    if let Ok(child) = c.spawn() { unsafe { let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id()); } }
}

/// Left click on the tray icon: opens the panel next to it, or closes it when it is open.
fn toggle_panel(panel: &mut Option<std::process::Child>, rect: &tray_icon::Rect) {
    if let Some(c) = panel.as_mut() {
        if matches!(c.try_wait(), Ok(None)) { let _ = c.kill(); let _ = c.wait(); *panel = None; return; }
    }
    let (l, t) = (rect.position.x as i32, rect.position.y as i32);
    let (r, b) = (l + rect.size.width as i32, t + rect.size.height as i32);
    *panel = keel_core::cmd::windowed(exe_beside("keel-gui.exe")).args(["--flyout", &format!("{l},{t},{r},{b}")]).spawn().ok();
}

fn gb(b: u64) -> String { format!("{:.1}", b as f64 / 1073741824.0) }

fn memory_line() -> String {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_err() { return "Memory: ?".into(); }
    format!("{} GB free of {} GB ({}%)", gb(m.ullAvailPhys), gb(m.ullTotalPhys), m.dwMemoryLoad)
}

/// The network line from the service's `net-status.json` (users may read ProgramData\keel).
fn net_line() -> String {
    let v: Option<serde_json::Value> = std::fs::read_to_string(keel_net::status_path()).ok().and_then(|s| serde_json::from_str(&s).ok());
    match v.as_ref().and_then(|v| v.get("summary")).and_then(|s| s.as_str()) {
        Some(s) => format!("Net: {s}"),
        None => "Net: no status yet".into(),
    }
}

/// Icon resource for the taskbar's theme (assets/app.rc): the boat alone, 3 = dark (light taskbar),
/// 4 = white (dark taskbar).
fn icon_ordinal() -> u16 {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let light: u32 = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value("SystemUsesLightTheme")).unwrap_or(0);
    if light == 1 { 3 } else { 4 }
}

/// What the tray icon shows: the taskbar's dark / white boat, or the boat in the user's app
/// colour (Settings > App icon).
type IconKey = (u16, Option<(u8, u8, u8)>);

fn icon_key(p: &Prefs) -> IconKey {
    (icon_ordinal(), p.tray_icon_color.then(|| keel_core::appicon::parse_hex(&p.icon_color)).flatten())
}

fn make_icon(k: IconKey) -> Option<Icon> {
    match k.1 {
        Some(rgb) => Icon::from_rgba(keel_core::appicon::render(keel_core::appicon::Style::Boat, rgb, 32), 32, 32).ok(),
        None => Icon::from_resource(k.0, Some((32, 32))).ok(),
    }
}

fn mtime(p: &Path) -> Option<SystemTime> { std::fs::metadata(p).and_then(|m| m.modified()).ok() }

// ------------------------------------------------------------------ menu

/// Results of pipe calls made on worker threads (they can take seconds).
enum Done { OnDemand(Option<OdState>), Message(String), Ai(bool, bool, Vec<keel_ram::ondemand::Entry>), AiLog(String) }

struct Ui {
    tray: TrayIcon,
    egpu: egpu::Ui,
    ram_line: MenuItem,
    net_line: MenuItem,
    ondemand: Submenu,
    od_items: Vec<(MenuId, String, bool)>,
    leaks: MenuItem,
    internet: Submenu,
    via_wifi: CheckMenuItem,
    via_lan: CheckMenuItem,
    net_apply: MenuItem,
    logs: Submenu,
    service_log: MenuItem,
    tray_log: MenuItem,
    data_folder: MenuItem,
    restart: MenuItem,
    open: MenuItem,
    quit: MenuItem,
}

impl Ui {
    fn item(&self, id: &str) -> Option<&dyn IsMenuItem> {
        if let Some(i) = self.egpu.item(id) { return Some(i); }
        Some(match id {
            "status.ram" => &self.ram_line,
            "status.net" => &self.net_line,
            "ram.ondemand" => &self.ondemand,
            "ram.leaks" => &self.leaks,
            "net.internet" => &self.internet,
            "net.apply" => &self.net_apply,
            "logs" => &self.logs,
            "service.restart" => &self.restart,
            _ => return None,
        })
    }

    /// Favorites in their order: status lines first, a separator, then the rest; Open and Quit last.
    fn build_menu(&self, prefs: &Prefs) {
        let menu = Menu::new();
        let (lines, rest): (Vec<&String>, Vec<&String>) = prefs.favorites.iter().partition(|f| f.starts_with("status."));
        for f in &lines { if let Some(i) = self.item(f) { let _ = menu.append(i); } }
        if !lines.is_empty() { let _ = menu.append(&PredefinedMenuItem::separator()); }
        for f in &rest { if let Some(i) = self.item(f) { let _ = menu.append(i); } }
        if !rest.is_empty() { let _ = menu.append(&PredefinedMenuItem::separator()); }
        let _ = menu.append_items(&[&self.open, &self.quit]);
        self.tray.set_menu(Some(Box::new(menu)));
    }

    fn set_ondemand(&mut self, st: &Option<OdState>) {
        while self.ondemand.remove_at(0).is_some() {}
        self.od_items.clear();
        match st {
            None => { let _ = self.ondemand.append(&MenuItem::new("Service not running", false, None)); }
            Some(s) if s.entries.is_empty() => { let _ = self.ondemand.append(&MenuItem::new("None (set them in Keel)", false, None)); }
            Some(s) => for e in &s.entries {
                let item = CheckMenuItem::new(&e.family, true, e.on, None);
                self.od_items.push((item.id().clone(), e.key.clone(), !e.on));
                let _ = self.ondemand.append(&item);
            },
        }
    }

    fn set_internet(&self) {
        let side = keel_net::load_config().map(|c| c.internet).unwrap_or(Side::Lan);
        self.via_wifi.set_checked(side == Side::Wifi);
        self.via_lan.set_checked(side == Side::Lan);
    }
}

fn build_ui(prefs: &Prefs) -> anyhow::Result<Ui> {
    // Left click: the panel (keel-gui --flyout); right click: the menu.
    let mut b = TrayIconBuilder::new().with_tooltip("Keel").with_menu_on_left_click(false);
    if let Some(i) = make_icon(icon_key(prefs)) { b = b.with_icon(i); }
    let tray = b.build()?;
    let internet = Submenu::new("Internet", true);
    let (via_wifi, via_lan) = (CheckMenuItem::new("Wi-Fi", true, false, None), CheckMenuItem::new("LAN", true, false, None));
    internet.append_items(&[&via_wifi, &via_lan])?;
    let logs = Submenu::new("Logs", true);
    let (service_log, tray_log, data_folder) = (MenuItem::new("Service log", true, None), MenuItem::new("Tray log", true, None), MenuItem::new("Data folder", true, None));
    logs.append_items(&[&service_log, &tray_log, &data_folder])?;
    let ui = Ui {
        tray, egpu: egpu::Ui::new(prefs)?,
        ram_line: MenuItem::new("Memory", false, None),
        net_line: MenuItem::new("Net", false, None),
        ondemand: Submenu::new("On-demand apps", true),
        od_items: Vec::new(),
        leaks: MenuItem::new("Memory history", true, None),
        internet, via_wifi, via_lan,
        net_apply: MenuItem::new("Re-apply network", true, None),
        logs, service_log, tray_log, data_folder,
        restart: MenuItem::new("Restart service", true, None),
        open: MenuItem::new("Open Keel", true, None),
        quit: MenuItem::new("Quit", true, None),
    };
    ui.set_internet();
    ui.build_menu(prefs);
    Ok(ui)
}

fn ask_ondemand(tx: &Sender<Done>, req: Option<Request>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        if let Some(r) = req {
            if let Err(e) = keel_ram::pipe::call(&r) { let _ = tx.send(Done::Message(format!("{e:#}"))); }
        }
        let st = keel_ram::pipe::call(&Request::Status).ok().map(|r| r.state);
        let _ = tx.send(Done::OnDemand(st));
    });
}

/// AI mode check (worker thread): is a model loaded (Ollama only, and only when it runs and AI
/// mode is on), and the service's on-demand entries.
fn ask_ai(tx: &Sender<Done>, ai_on: bool, game_on: bool) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let loaded = ai_on && process_running("ollama.exe")
            && keel_egpu::coord::ollama_loaded(&keel_egpu::config::Config::load().ollama_url).unwrap_or(0) > 0;
        let game = game_on && egpu::game_running();
        let Ok(r) = keel_ram::pipe::call(&Request::Status) else { return };
        let _ = tx.send(Done::Ai(loaded, game, r.state.entries));
    });
}

/// Families AI mode and game mode want off now (keys, each once). Pure; tested.
fn modes_chosen(ai: Option<&[traycfg::AiApp]>, game: Option<&[traycfg::AiApp]>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for a in ai.into_iter().flatten().chain(game.into_iter().flatten()) {
        if !v.contains(&a.key) { v.push(a.key.clone()); }
    }
    v
}

/// AI mode changes on one worker thread: one request to the service for all families to switch
/// off (it scans once), then the tray closes, with the user's rights, the processes of families
/// the service cannot switch (installed per user, or nothing machine-wide); then one request to
/// put families back.
fn ask_ai_switch(tx: &Sender<Done>, busy: &Arc<AtomicBool>, off: Vec<String>, on: Vec<String>, all_back: bool) {
    if off.is_empty() && on.is_empty() { return; }
    let (tx, busy) = (tx.clone(), busy.clone());
    busy.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        let log = |m: String| { let _ = tx.send(Done::AiLog(m)); };
        if !off.is_empty() {
            let before = startup_apps_running();
            match keel_ram::pipe::call_wait(&Request::Ai { keys: off, off: true }, Duration::from_secs(120)) {
                Ok(r) => {
                    if !r.ok { log(format!("ai mode: {}", r.message)); }
                    if !r.failed.is_empty() { for m in close_user_apps(&r.failed) { log(m); } }
                }
                Err(e) => log(format!("ai mode: {e:#}")),
            }
            // Startup apps AI mode closed: started again when the model unloads.
            let now = running_exes();
            let closed: Vec<(String, String)> = before.into_iter().filter(|(exe, _)| !now.contains(exe)).collect();
            if !closed.is_empty() {
                let mut all = relaunch_load();
                for c in closed { if !all.contains(&c) { all.push(c); } }
                if let Err(e) = relaunch_save(&all) { log(format!("ai mode: {}: {e:#}", relaunch_path().display())); }
            }
        }
        if !on.is_empty() {
            match keel_ram::pipe::call_wait(&Request::Ai { keys: on, off: false }, Duration::from_secs(120)) {
                Ok(r) if !r.ok => log(format!("ai mode: {}", r.message)),
                // Startup apps start again once neither mode wants anything off (which app a
                // closed process belonged to is not kept).
                Ok(_) if all_back => { for m in relaunch_closed() { log(m); } }
                Ok(_) => {}
                Err(e) => log(format!("ai mode: {e:#}")),
            }
        }
        busy.store(false, Ordering::SeqCst);
    });
}

/// Closes the running processes of these families (vendor folders as this user sees them;
/// protected families never). Nothing to put back later: the user opens the app again.
fn close_user_apps(keys: &[String]) -> Vec<String> {
    use keel_ram::{ondemand, procs, scan, switch};
    let report = match scan::run(0, scan::SortBy::Private, false) {
        Ok(r) => r,
        Err(e) => return vec![format!("ai mode: scan: {e:#}")],
    };
    let Ok(ps) = procs::snapshot() else { return vec!["ai mode: no process list".into()] };
    let devmap = procs::device_map();
    let list: Vec<(u32, String, Option<String>)> = ps.iter().map(|p| (p.pid, p.name.clone(), procs::image_path(p.pid, &devmap).0)).collect();
    let mut out = Vec::new();
    for k in keys {
        let Some(f) = report.families.iter().find(|f| &f.key == k) else { out.push(format!("ai mode: {k}: not running")); continue };
        if f.protected.is_some() { out.push(format!("ai mode: {}: protected, left alone", f.name)); continue; }
        let pids = ondemand::leftovers(&ondemand::family_dirs(f), &list, std::process::id());
        if pids.is_empty() { out.push(format!("ai mode: {}: nothing running to close", f.name)); }
        for pid in pids {
            let name = list.iter().find(|x| x.0 == pid).map(|x| x.1.clone()).unwrap_or_default();
            out.push(match switch::terminate(pid) {
                Ok(()) => format!("ai mode: {}: closed {name} ({pid})", f.name),
                Err(e) => format!("ai mode: {}: close {name} ({pid}): {e:#}", f.name),
            });
        }
    }
    out
}

/// Exe paths (lowercase) of the running processes this user can see.
fn running_exes() -> Vec<String> {
    use keel_ram::procs;
    let devmap = procs::device_map();
    procs::snapshot().unwrap_or_default().iter().filter_map(|p| procs::image_path(p.pid, &devmap).0).map(|p| p.to_lowercase()).collect()
}

/// Run-key and Startup-folder apps that run now: (exe, command line). Disabled items count too:
/// On-demand apps switches a followed app's startup item off while it runs.
fn startup_apps_running() -> Vec<(String, String)> {
    use keel_ram::sources;
    let now = running_exes();
    let mut out: Vec<(String, String)> = sources::run_entries().into_iter().chain(sources::startup_entries())
        .filter(|l| l.kind != "runonce")
        .filter_map(|l| l.exe.map(|e| (e.to_lowercase(), l.command)))
        .filter(|(e, _)| now.contains(e)).collect();
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// `%LOCALAPPDATA%\keel\ai-relaunch.json`: startup apps AI mode closed (kept on disk, so a
/// tray restart while a model is loaded still starts them again).
fn relaunch_path() -> PathBuf { keel_core::paths::user_data_dir().join("ai-relaunch.json") }
fn relaunch_load() -> Vec<(String, String)> {
    std::fs::read_to_string(relaunch_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}
fn relaunch_save(v: &[(String, String)]) -> anyhow::Result<()> {
    let p = relaunch_path();
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    std::fs::write(p, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

/// Starts the recorded startup apps again with their own command lines (not when already running).
fn relaunch_closed() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    let list = relaunch_load();
    if list.is_empty() { return Vec::new(); }
    let now = running_exes();
    let mut out = Vec::new();
    for (exe, cmd) in &list {
        if now.contains(exe) { continue; }
        let path = PathBuf::from(exe);
        let args = keel_ram::paths::command_args(cmd, &keel_ram::sources::env_lookup);
        let mut c = keel_core::cmd::windowed(&path);
        if !args.is_empty() { c.raw_arg(&args); }
        if let Some(d) = path.parent() { c.current_dir(d); }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(match c.spawn() {
            Ok(_) => format!("ai mode: started {name} again"),
            Err(e) => format!("ai mode: start {name}: {e}"),
        });
    }
    let _ = std::fs::remove_file(relaunch_path());
    out
}

/// Whether a process with this exe name runs (cheap toolhelp snapshot).
fn process_running(exe: &str) -> bool {
    keel_ram::procs::snapshot().map(|v| v.iter().any(|p| p.name.eq_ignore_ascii_case(exe))).unwrap_or(false)
}

fn ask_net(tx: &Sender<Done>, req: keel_net::service::Request) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        let m = match keel_net::service::call(&req) { Ok(r) => r.message, Err(e) => format!("{e:#}") };
        let _ = tx.send(Done::Message(format!("Network: {m}")));
    });
}

/// A click outside the eGPU items; false = quit.
fn handle(ui: &Ui, id: &MenuId, tx: &Sender<Done>) -> bool {
    if id == ui.quit.id() { return false; }
    if id == ui.open.id() { open_gui(None); }
    else if id == ui.leaks.id() { open_gui(Some("memory-history")); }
    else if id == ui.via_wifi.id() || id == ui.via_lan.id() {
        let side = if id == ui.via_wifi.id() { Side::Wifi } else { Side::Lan };
        ui.via_wifi.set_checked(side == Side::Wifi);
        ui.via_lan.set_checked(side == Side::Lan);
        ask_net(tx, keel_net::service::Request::SetInternet { side });
    }
    else if id == ui.net_apply.id() { ask_net(tx, keel_net::service::Request::Apply); }
    else if id == ui.service_log.id() { open_path(&keel_core::log::service_log_path()); }
    else if id == ui.tray_log.id() { open_path(&keel_core::log::user_log_path()); }
    else if id == ui.data_folder.id() { open_path(&keel_core::paths::user_data_dir()); }
    else if id == ui.restart.id() { service_job(serde_json::json!({"cmd": "restart"}), "service-restart"); }
    else if let Some((_, key, on)) = ui.od_items.iter().find(|(i, _, _)| i == id) {
        ask_ondemand(tx, Some(Request::Turn { key: key.clone(), on: *on }));
    }
    true
}

fn tooltip(ram: &str, egpu: &[String], net: &str) -> String {
    let mut lines = vec!["Keel".to_string(), ram.to_string()];
    lines.extend(egpu.iter().filter(|l| !l.is_empty()).cloned());
    lines.push(net.to_string());
    let s = lines.join("\n");
    s.chars().take(127).collect()
}

fn main() -> anyhow::Result<()> {
    std::panic::set_hook(Box::new(|info| keel_core::log::user(format!("tray panic: {info}"))));
    let Some(_one) = keel_core::single::acquire("keel-tray") else { return Ok(()) };
    // Start-up steps are logged, so a tray that stops during start-up shows where.
    tray_log(&format!("tray starting {} (pid {})", env!("CARGO_PKG_VERSION"), std::process::id()));
    // Real pixel coordinates on every monitor, needed to move windows between them.
    unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
    let prefs = Prefs::load();
    let mut ui = build_ui(&prefs)?;
    tray_log("tray starting: icon and menu ready");
    let mut prefs_seen = mtime(&traycfg::prefs_path());
    egpu::start(&prefs);
    let mut st = egpu::State::new(prefs);
    tray_log("tray starting: eGPU part ready");
    // One memory observer per session, shared with the console `keel ram watch`.
    let _watch = keel_core::single::acquire("keel-watch");
    if _watch.is_some() {
        keel_ram::events::set_after_sample(dwm_ram_check);
        if let Err(e) = keel_ram::events::start() { tray_log(&format!("memory observer: {e:#}")); }
    }
    let (tx, rx): (Sender<Done>, Receiver<Done>) = channel();
    if st.prefs.has("ram.ondemand") { ask_ondemand(&tx, None); }
    tray_log("tray started");
    let mut cfg = keel_egpu::config::Config::load();
    let mut ticks = 0u32;
    let (mut ram, mut net) = (memory_line(), net_line());
    let mut icon = icon_key(&st.prefs);
    let mut panel: Option<std::process::Child> = None;
    let mut last_tick = Instant::now();
    // AI mode: families switched off (or tried) in this model load, so each is tried once; and
    // whether the service holds families AI mode switched off (checked once at start).
    let mut ai_failed: Vec<String> = Vec::new();
    let mut ai_pending = true;
    let ai_busy = Arc::new(AtomicBool::new(false));
    egpu::refresh(&ui.egpu, &mut st, &cfg);
    keel_core::trim::working_set();
    loop {
        // Pump window messages (tray, menu, WinEvent hook, observer window); wake once a second.
        let r = unsafe { MsgWaitForMultipleObjects(None, false, 1000, QS_ALLINPUT) };
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT { return Ok(()); }
            unsafe { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } = ev {
                toggle_panel(&mut panel, &rect);
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            let id = ev.id();
            if !egpu::handle(&ui.egpu, &mut st, &cfg, id) && !handle(&ui, id, &tx) { return Ok(()); }
            egpu::refresh(&ui.egpu, &mut st, &cfg);
        }
        while let Ok(d) = rx.try_recv() {
            match d {
                Done::OnDemand(s) => ui.set_ondemand(&s),
                Done::Message(m) => { tray_log(&m); notify(&m); ui.set_internet(); }
                // Plans made while earlier changes still run would repeat them.
                Done::Ai(..) if ai_busy.load(Ordering::SeqCst) => {}
                Done::Ai(loaded, game, entries) => {
                    let chosen = modes_chosen(loaded.then_some(&st.prefs.ai_mode_apps[..]), game.then_some(&st.prefs.game_mode_apps[..]));
                    let active = !chosen.is_empty();
                    if !active { ai_failed.clear(); }
                    let (off, back) = keel_ram::ondemand::ai_plan(active, &chosen, &entries, &ai_failed);
                    ai_pending = entries.iter().any(|e| e.ai_off) || !off.is_empty();
                    let why = match (loaded, game) { (true, true) => "model loaded, game running", (true, false) => "model loaded", _ => "game running" };
                    if !off.is_empty() { tray_log(&format!("ai mode: {why}, switching off {}", off.join(", "))); }
                    if !back.is_empty() { tray_log(&format!("ai mode: putting back {}", back.join(", "))); }
                    ai_failed.extend(off.iter().cloned());
                    ask_ai_switch(&tx, &ai_busy, off, back, !active);
                }
                Done::AiLog(m) => tray_log(&m),
            }
        }
        // The once-a-second work runs by the clock. (It used to count wake-ups: with steady window
        // messages, e.g. the Legion Space hooks, only odd counts reached here, so the every-2-ticks
        // settings reload never ran and the tray icon colour needed a restart.)
        let _ = r;
        if last_tick.elapsed() < Duration::from_millis(950) { continue; }
        last_tick = Instant::now();
        ticks += 1;
        if ticks % 30 == 0 { cfg = keel_egpu::config::Config::load(); }
        egpu::refresh(&ui.egpu, &mut st, &cfg);
        egpu::tick(&mut st, &cfg, ticks);
        if ticks % 5 == 0 { ram = memory_line(); ui.ram_line.set_text(&ram); }
        if ticks % 10 == 0 { net = net_line(); ui.net_line.set_text(&net); }
        // AI mode on, or families it switched off still to put back (also after a reboot).
        if ticks % 10 == 5 && (st.prefs.ai_mode || st.prefs.game_mode || ai_pending) { ask_ai(&tx, st.prefs.ai_mode, st.prefs.game_mode); }
        if ticks % 30 == 0 {
            if st.prefs.has("ram.ondemand") { ask_ondemand(&tx, None); }
            if st.prefs.has("net.internet") { ui.set_internet(); }
            let now = icon_key(&st.prefs);
            if now != icon { icon = now; if let Some(i) = make_icon(icon) { let _ = ui.tray.set_icon(Some(i)); } }
        }
        if ticks % 2 == 0 {
            let m = mtime(&traycfg::prefs_path());
            if m != prefs_seen {
                prefs_seen = m;
                let p = Prefs::load();
                if p.favorites != st.prefs.favorites { ui.build_menu(&p); }
                if p.has("ram.ondemand") && !st.prefs.has("ram.ondemand") { ask_ondemand(&tx, None); }
                st.set_prefs(&ui.egpu, p);
                let now = icon_key(&st.prefs);
                if now != icon { icon = now; if let Some(i) = make_icon(icon) { let _ = ui.tray.set_icon(Some(i)); } }
            }
        }
        let _ = ui.tray.set_tooltip(Some(tooltip(&ram, &st.tip, &net)));
        if ticks % 600 == 0 { keel_core::trim::working_set(); }
    }
}

#[cfg(test)]
mod tests {
    use keel_core::traycfg::AiApp;

    #[test]
    fn ai_and_game_mode_lists_join() {
        let a = |k: &str| AiApp { key: k.into(), name: k.into() };
        let ai = [a("adobe"), a("onedrive")];
        let game = [a("onedrive"), a("zoom")];
        assert_eq!(super::modes_chosen(Some(&ai), Some(&game)), ["adobe", "onedrive", "zoom"]);
        assert_eq!(super::modes_chosen(None, Some(&game)), ["onedrive", "zoom"]);
        assert!(super::modes_chosen(None, None).is_empty());
    }
}

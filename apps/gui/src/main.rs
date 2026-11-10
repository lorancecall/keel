//! keel-gui: Keel's main window (Slint, software renderer, Fluent style following the system's dark
//! / light setting). Only runs while open. Side menu (ui/nav.slint): Home; Memory: Apps,
//! On-demand apps, Memory history; Graphics: External GPU, Displays, GPU apps (games and Local AI tabs); Network; Settings
//! (appearance, service features, tray menu). Data comes from the libraries (scan, families), the service's pipe
//! (on-demand, network, features) and the status files the service writes; slow work runs on
//! worker threads.
#![windows_subsystem = "windows"]

use keel_core::traycfg::{self, Prefs};
use keel_net::model::{Config as NetConfig, RouteRule, Side, Vpn, VpnKind};
use keel_ram::family::Family;
use keel_ram::ondemand::{self, Mode, Request, State};
use keel_ram::{pipe, scan, userside};
use slint::{Model, ModelRc, SharedString, StandardListViewItem, VecModel};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

slint::include_modules!();

mod amdtune;
mod nv3d;
mod egpu_page;
mod flyout;
mod games;
mod displays;
mod home;
mod localai;
mod memhistory;
mod redraw;
mod stress;

thread_local! {
    /// External GPU page state (telemetry history, stress test); UI thread only.
    static EGPU: std::cell::RefCell<egpu_page::Page> = Default::default();
    static TICK: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Widget ids in `Prefs::hidden_widgets`.
const WIDGETS: [&str; 5] = ["history", "gauges", "bars", "tuning", "stress"];

fn mb(b: u64) -> String { format!("{:.0}", b as f64 / 1048576.0) }
fn gb(b: u64) -> String { format!("{:.1}", b as f64 / 1073741824.0) }

fn rows(v: Vec<Vec<String>>) -> ModelRc<ModelRc<StandardListViewItem>> {
    let outer: Vec<ModelRc<StandardListViewItem>> = v.into_iter()
        .map(|r| ModelRc::new(VecModel::from(r.into_iter().map(|c| StandardListViewItem::from(SharedString::from(c))).collect::<Vec<_>>())))
        .collect();
    ModelRc::new(VecModel::from(outer))
}

fn count(f: &Family, k: &str) -> usize { f.counts.get(k).copied().unwrap_or(0) }

fn family_row(f: &Family) -> Vec<String> {
    vec![format!("{}{}", if f.protected.is_some() { "! " } else { "" }, f.name), f.processes.to_string(), mb(f.memory),
        f.auto_start.to_string(), count(f, "service").to_string(), count(f, "task").to_string()]
}

/// Members of a family as text for the details pane.
fn family_text(f: &Family) -> String {
    let mut s = format!("{}\n{} processes, {} MB memory (as Task Manager; {} MB committed), {} things start by itself\n",
        f.name, f.processes, mb(f.memory), mb(f.private_bytes), f.auto_start);
    if let Some(p) = f.protected { s += &format!("Protected ({p}): Keel never switches it.\n"); }
    if !f.uninstallers.is_empty() { s += &format!("Uninstaller: {}\n", f.uninstallers[0]); }
    let mut last = "";
    for m in &f.members {
        if m.kind == "product" && m.hidden { continue; }
        if m.kind != last { s += &format!("\n{}s:\n", m.kind); last = m.kind; }
        let mut tags = Vec::new();
        if let Some(st) = &m.start { tags.push(st.clone()); }
        if m.enabled == Some(false) && m.start.as_deref() != Some("disabled") { tags.push("off".into()); }
        if m.running && m.kind != "process" { tags.push("running".into()); }
        if m.kind == "process" { tags.push(format!("{} MB committed", mb(m.bytes))); }
        s += &format!("  {}  {}\n", m.name, if tags.is_empty() { String::new() } else { format!("({})", tags.join(", ")) });
    }
    s
}

fn side_index(s: Side) -> i32 { if s == Side::Wifi { 0 } else { 1 } }
fn index_side(i: i32) -> Side { if i == 0 { Side::Wifi } else { Side::Lan } }

fn vpn_model(c: &NetConfig) -> ModelRc<VpnRow> {
    ModelRc::new(VecModel::from(c.vpns.iter().map(|v| {
        let mut kind = vec![match v.kind { VpnKind::DefaultRoute => "follows the default route".to_string(), VpnKind::Server => "own server routes".into() }];
        if !v.adapter.trim().is_empty() { kind.push(format!("tunnel {}", v.adapter.trim())); }
        if !v.ports.is_empty() { kind.push(format!("ports {}", v.ports.join(", "))); }
        if !v.servers.is_empty() { kind.push(format!("{} server(s)", v.servers.len())); }
        let progs = v.all_programs();
        VpnRow {
            name: v.name.clone().into(),
            kind: kind.join(" · ").into(),
            via: side_index(v.via), kill: v.kill_switch,
            exe: if progs.is_empty() { "no program".into() } else { progs.join("; ").into() },
        }
    }).collect::<Vec<_>>()))
}

fn route_model(c: &NetConfig) -> ModelRc<RouteRow> {
    ModelRc::new(VecModel::from(c.routes.iter().map(|r| RouteRow { prefix: r.prefix.clone().into(), via: side_index(r.via) }).collect::<Vec<_>>()))
}

fn fill_form(ui: &MainWindow, v: &Vpn) {
    ui.set_new_name(v.name.clone().into());
    ui.set_new_kind(if v.kind == VpnKind::Server { 1 } else { 0 });
    ui.set_new_via(side_index(v.via));
    ui.set_new_kill(v.kill_switch);
    ui.set_new_exe(v.all_programs().join("\n").into());
    ui.set_new_adapter(v.adapter.clone().into());
    ui.set_new_ports(v.ports.join(", ").into());
    ui.set_new_servers(v.servers.join(", ").into());
}

/// The VPN in the form, checked.
fn form_vpn(ui: &MainWindow) -> Result<Vpn, String> {
    let name = ui.get_new_name().trim().to_string();
    if name.is_empty() { return Err("A VPN needs a name".into()); }
    let programs: Vec<String> = ui.get_new_exe().lines().map(|l| l.trim().trim_matches('"').to_string()).filter(|l| !l.is_empty()).collect();
    let ports: Vec<String> = ui.get_new_ports().split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    if let Some(bad) = ports.iter().find(|p| keel_net::model::parse_ports(p).is_none()) {
        return Err(format!("Ports: \"{bad}\" is not like \"udp 51820\""));
    }
    let servers: Vec<String> = ui.get_new_servers().split([',', ' ', ';']).filter(|s| !s.is_empty()).map(String::from).collect();
    if let Some(bad) = servers.iter().find(|s| keel_net::model::parse_prefix(s).is_none()) { return Err(format!("Servers: {bad} is not an address")); }
    let kill = ui.get_new_kill();
    if kill && programs.is_empty() && ports.is_empty() { return Err("The kill switch needs a program or a port to block".into()); }
    let mut v = Vpn { name, kind: if ui.get_new_kind() == 1 { VpnKind::Server } else { VpnKind::DefaultRoute },
                      adapter: ui.get_new_adapter().trim().to_string(), via: index_side(ui.get_new_via()), kill_switch: kill,
                      servers, ports, ..Default::default() };
    v.set_programs(programs);
    Ok(v)
}

/// Running programs with a known path, once each: (file name, path, path).
fn running_programs() -> Vec<(String, String, String)> {
    let map = keel_ram::procs::device_map();
    let mut out: Vec<(String, String, String)> = Vec::new();
    for p in keel_ram::procs::snapshot().unwrap_or_default() {
        let Some(path) = keel_ram::procs::image_path(p.pid, &map).0 else { continue };
        if out.iter().any(|x| x.2.eq_ignore_ascii_case(&path)) { continue; }
        out.push((p.name.clone(), path.clone(), path));
    }
    out.sort_by_key(|x| x.0.to_lowercase());
    out
}

/// Adapters that are no Wi-Fi or LAN, tunnels before WAN miniports: (description, name and state, description).
fn adapter_picks() -> Vec<(String, String, String)> {
    use keel_net::model::Kind;
    let mut v: Vec<_> = keel_net::sys::adapters().unwrap_or_default().into_iter()
        .filter(|a| a.kind == Kind::Other && !a.description.to_lowercase().contains("loopback"))
        .map(|a| (a.description.clone(), format!("{} · {}", a.alias, if a.up { "up" } else { "down" }), a.description)).collect();
    v.sort_by_key(|x| (x.0.to_lowercase().contains("miniport"), x.0.to_lowercase()));
    v
}

fn show_picks(ui: &MainWindow, all: &[(String, String, String)]) {
    let f = ui.get_pick_filter().to_lowercase();
    let rows: Vec<PickRow> = all.iter().filter(|x| f.is_empty() || x.0.to_lowercase().contains(&f) || x.1.to_lowercase().contains(&f))
        .map(|x| PickRow { title: x.0.clone().into(), sub: x.1.clone().into(), value: x.2.clone().into() }).collect();
    ui.set_pick_rows(ModelRc::new(VecModel::from(rows)));
}

fn top_row(f: &Family) -> Vec<String> {
    vec![f.name.clone(), f.processes.to_string(), mb(f.memory), f.auto_start.to_string(),
         if f.protected.is_some() { "yes".into() } else { String::new() }]
}

/// Table cell order: numbers as numbers (blank first), everything else case-insensitive.
/// The "! " mark of protected families is ignored.
fn cmp_cell(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (a.trim_start_matches("! "), b.trim_start_matches("! "));
    let num = |s: &str| s.split_whitespace().next().and_then(|x| x.parse::<f64>().ok());
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (None, Some(_)) if a.is_empty() => std::cmp::Ordering::Less,
        (Some(_), None) if b.is_empty() => std::cmp::Ordering::Greater,
        _ => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

/// Stable sort of `v` by column `col` of its table row.
fn sort_rows<T>(v: &mut [T], row: impl Fn(&T) -> Vec<String>, (col, asc): (usize, bool)) {
    v.sort_by(|a, b| {
        let (ra, rb) = (row(a), row(b));
        let o = cmp_cell(ra.get(col).map_or("", |s| s.as_str()), rb.get(col).map_or("", |s| s.as_str()));
        if asc { o } else { o.reverse() }
    });
}

fn managed_row(e: &keel_ram::ondemand::Entry) -> Vec<String> {
    vec![e.family.clone(), match e.mode { Mode::Off => "keep off".into(), Mode::Follow => "follow".into() },
         if e.on { "on".into() } else { "off".into() }, e.apps.join(", "), e.services.len().to_string(),
         e.tasks.iter().filter(|t| t.enabled).count().to_string()]
}

#[derive(Default)]
struct Data {
    families: Vec<Family>,
    /// The overview's largest families (its own order).
    top: Vec<Family>,
    od: Option<State>,
    net: Option<NetConfig>,
    /// What each LAN choice on the Network page saves as `lan_descriptions`.
    net_lans: Vec<Vec<String>>,
    /// The Network page's picker: (title, detail, value), unfiltered.
    picks: Vec<(String, String, String)>,
    /// Column and direction each table is sorted by (overview, families, on-demand); kept across reloads.
    sorts: [Option<(usize, bool)>; 3],
}

impl Data {
    /// Applies the remembered sorts and puts the three tables on screen.
    fn show_tables(&mut self, ui: &MainWindow) {
        if let Some(s) = self.sorts[0] { sort_rows(&mut self.top, top_row, s); }
        if let Some(s) = self.sorts[1] { sort_rows(&mut self.families, family_row, s); }
        if let (Some(s), Some(od)) = (self.sorts[2], self.od.as_mut()) { sort_rows(&mut od.entries, managed_row, s); }
        ui.set_top_families(rows(self.top.iter().map(top_row).collect()));
        ui.set_families(rows(self.families.iter().map(family_row).collect()));
        ui.set_managed(rows(self.od.as_ref().map(|s| s.entries.iter().map(managed_row).collect()).unwrap_or_default()));
    }
}

/// Runs `work` on a thread with the window marked busy, then `done` on the UI thread.
fn background<T: Send + 'static>(ui: &MainWindow, work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(&MainWindow, T) + Send + 'static) {
    ui.set_busy(true);
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let r = work();
        let _ = weak.upgrade_in_event_loop(move |ui| { ui.set_busy(false); done(&ui, r); });
    });
}

// ------------------------------------------------------------------ loading

fn read_egpu_status() -> Option<serde_json::Value> {
    std::fs::read_to_string(keel_egpu::util::status_path()).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// External GPU page and Home's eGPU line from `egpu-status.json` (cheap; every 2 s while open).
fn show_egpu(ui: &MainWindow) {
    let st = read_egpu_status();
    let alive = st.as_ref().and_then(|s| s["utc"].as_str()).and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok())
        .is_some_and(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() < 20);
    let t: Option<keel_egpu::telemetry::GpuTelemetry> = st.as_ref().and_then(|s| serde_json::from_value(s["telemetry"].clone()).ok());
    ui.set_egpu_line(match (alive, &t) {
        (false, _) => "eGPU: the service is not running".to_string(),
        (true, None) => "eGPU: none attached".to_string(),
        (true, Some(t)) => format!("eGPU: {}  {}", t.name, t.summary()),
    }.into());
    // Vendor of the attached eGPU (the service lists eGPUs by adapter name).
    let vendor = st.as_ref().filter(|_| alive).and_then(|s| {
        let egpus: Vec<&str> = s["egpus"].as_array()?.iter().filter_map(|v| v.as_str()).collect();
        s["adapters"].as_array()?.iter().find(|a| a["name"].as_str().is_some_and(|n| egpus.contains(&n)))?["vendor_id"].as_u64()
    });
    let name = t.as_ref().map(|t| t.name.clone()).unwrap_or_default();

    EGPU.with_borrow_mut(|pg| {
        egpu_page::show(ui, pg, alive, &name, vendor, t.as_ref());
        home::egpu(ui, pg, alive, &name, t.as_ref(), st.as_ref());
        egpu_page::stress_tick(ui, pg);
    });
    // The Legion Space option works only while the built-in screen is the main display.
    let pos = keel_egpu::display::positions();
    ui.global::<EgpuData>().set_legion_ok(keel_egpu::legion::pin_state(keel_egpu::display::internal_display_name().as_deref(),
        keel_egpu::display::primary_of(&pos)) == keel_egpu::legion::PinState::On);
}

fn show_prefs(ui: &MainWindow, p: &Prefs) {
    let d = ui.global::<EgpuData>();
    d.set_opt_legion(p.legion_space_pin);
    d.set_opt_legion_screen(p.legion_screen());
    d.set_opt_primary(p.egpu_primary_display);
    d.set_opt_prune(p.prune_amd_tray_icons);
    d.set_opt_search(p.restart_search_after_scaling);
    d.set_opt_notify(p.notifications);
    let shown = |w: &str| !p.hidden_widgets.iter().any(|h| h == w);
    d.set_w_history(shown(WIDGETS[0]));
    d.set_w_gauges(shown(WIDGETS[1]));
    d.set_w_bars(shown(WIDGETS[2]));
    d.set_w_tuning(shown(WIDGETS[3]));
    d.set_w_stress(shown(WIDGETS[4]));
    let h = ui.global::<HomeData>();
    h.set_w_mem(shown("home.memory"));
    h.set_w_egpu(shown("home.egpu"));
    h.set_w_net(shown("home.network"));
    h.set_order(ModelRc::new(VecModel::from(traycfg::home_order(&p.home_order).into_iter().map(SharedString::from).collect::<Vec<_>>())));
    d.set_tuning_first(p.egpu_tuning_first);
    ui.set_side_menu(p.side_menu.min(2) as i32);
    show_icon_prefs(ui, p);
    ui.global::<MemHistData>().set_keep(p.keep_memory_history);
    ui.global::<MemHistData>().set_fast(p.fast_samples);
    // Grouped by the id's prefix (ITEMS keeps each group together).
    let group = |id: &str| match id.split('.').next() {
        Some("status") => "Status lines", Some("egpu") => "External GPU", Some("ram") => "Memory",
        Some("net") => "Network", _ => "Keel",
    };
    let favs: Vec<Fav> = traycfg::ITEMS.iter().map(|(id, label, desc)| Fav {
        id: (*id).into(), label: (*label).into(), desc: (*desc).into(), on: p.has(id), group: group(id).into(),
    }).collect();
    ui.set_favorites(ModelRc::new(VecModel::from(favs)));
}

fn show_net(ui: &MainWindow, r: &Result<keel_net::service::Reply, String>, data: &Arc<Mutex<Data>>) {
    home::net(ui, r);
    match r {
        Ok(r) => {
            ui.set_net_summary(r.summary.clone().into());
            let mut d = Vec::new();
            d.extend(r.problems.iter().map(|p| format!("! {p}")));
            if r.pending.is_empty() { d.push("Routes and rules match the config.".into()); } else { d.push(format!("{} change(s) pending: {}", r.pending.len(), r.pending.join("; "))); }
            ui.set_net_detail(d.join("   ").into());
            ui.set_net_line(format!("Network: {}", r.summary).into());
            if let Some(c) = &r.config { set_net_config(ui, c, data); data.lock().unwrap().net = Some(c.clone()); }
        }
        Err(e) => {
            // No service: show the config as read (ProgramData\keel is readable) so it can be looked at.
            ui.set_net_summary("The Keel service is not running".into());
            ui.set_net_detail(if e.contains("not running") { "Showing net.toml as saved; Save and apply needs the service.".into() } else { e.clone().into() });
            ui.set_net_line("Network: the service is not running".into());
            if let Ok(c) = keel_net::load_config() { set_net_config(ui, &c, data); data.lock().unwrap().net = Some(c); }
        }
    }
}

fn set_net_config(ui: &MainWindow, c: &NetConfig, data: &Arc<Mutex<Data>>) {
    use keel_net::model::{Kind, lan_choices};
    let wired: Vec<String> = keel_net::sys::adapters().unwrap_or_default().into_iter()
        .filter(|a| a.kind == Kind::Wired).map(|a| a.description).collect();
    let (labels, values, at) = lan_choices(&wired, &c.lan_descriptions);
    ui.set_net_lans(ModelRc::new(VecModel::from(labels.into_iter().map(SharedString::from).collect::<Vec<_>>())));
    ui.set_net_lan(at as i32);
    data.lock().unwrap().net_lans = values;
    ui.set_net_internet(side_index(c.internet));
    ui.set_net_enabled(c.enabled);
    ui.set_net_keep_wifi(c.keep_wifi_with_lan);
    ui.set_vpn_list(vpn_model(c));
    ui.set_route_list(route_model(c));
    ui.set_net_ts_wifi(c.tailscale_wifi_only);
    let presets: Vec<SharedString> = std::iter::once("Start from...".to_string()).chain(Vpn::presets().into_iter().map(|v| v.name))
        .map(SharedString::from).collect();
    ui.set_vpn_presets(ModelRc::new(VecModel::from(presets)));
}

fn show_service(ui: &MainWindow, r: &Option<keel_service::Reply>) {
    home::lights(ui, r);
    let labels = [("ram", "Memory"), ("egpu", "eGPU"), ("net", "Network")];
    let feats: Vec<Feat> = labels.iter().map(|(n, l)| {
        let f = r.as_ref().and_then(|r| r.features.iter().find(|f| f.name == *n));
        let state = match f {
            None => "service not running".to_string(),
            Some(f) if !f.enabled => "off".into(),
            Some(f) if f.running => if f.restarts > 0 { format!("running ({} restarts; last: {})", f.restarts, f.last_error.as_deref().unwrap_or("-")) } else { "running".into() },
            Some(f) => format!("restarting: {}", f.last_error.as_deref().unwrap_or("-")),
        };
        Feat { name: (*n).into(), label: (*l).into(), on: f.is_some_and(|f| f.enabled), state: state.into() }
    }).collect();
    ui.set_features(ModelRc::new(VecModel::from(feats)));
    let me = env!("CARGO_PKG_VERSION");
    ui.set_service_version(match r {
        Some(r) if r.version == me => format!("Service {} · pid {}", r.version, r.pid),
        Some(r) => format!("Service {} (this window {me}): reinstall to match", r.version),
        None => "Service not running".into(),
    }.into());
    ui.set_service_line(match r {
        Some(r) => format!("Service {}: {}", r.version, r.features.iter().map(|f| format!("{} {}", f.name,
            if !f.enabled { "off" } else if f.running { "on" } else { "restarting" })).collect::<Vec<_>>().join(", ")),
        None => "Service: not running (install Keel to start it)".into(),
    }.into());
}

fn load(ui: &MainWindow, data: Arc<Mutex<Data>>) {
    home::memory_tick(ui);
    show_egpu(ui);
    show_prefs(ui, &Prefs::load());
    background(ui, || {
        let report = scan::run(0, scan::SortBy::Private, false).ok();
        let od = pipe::call(&Request::Status).ok().map(|r| r.state);
        let net = keel_net::service::call(&keel_net::service::Request::Status).map_err(|e| format!("{e:#}"));
        let svc = keel_service::call(&keel_service::Request::Status).ok();
        (report, od, net, svc)
    }, move |ui, (report, od, net, svc)| {
        if let Some(r) = report {
            if let Some(m) = &r.memory {
                ui.set_memory_line(format!("{} GB free of {} GB  ({}% used)   commit {} / {} GB", gb(m.available), gb(m.total), m.load_pct, gb(m.commit_used), gb(m.commit_limit)).into());
            }
            let mut d = data.lock().unwrap();
            d.families = r.families.into_iter().filter(scan::notable).collect();
            d.top = d.families.iter().take(25).cloned().collect();
            home::top(ui, &d.families, r.memory.as_ref().map(|m| m.total).unwrap_or(0));
        }
        {
            let mut d = data.lock().unwrap();
            d.od = od;
            home::background(ui, d.od.as_ref());
            d.show_tables(ui);
        }
        show_net(ui, &net, &data);
        show_service(ui, &svc);
        // Keep the message of the action that triggered this reload (Keep off, Follow app, ...).
        let st = ui.get_status();
        if svc.is_none() { ui.set_status("Service not running".into()); }
        else if st.is_empty() || st == "Service not running" || st.ends_with("...") { ui.set_status("Service running".into()); }
    });
}

/// Gives the window the app icon (resource 1, embedded by build.rs) for title bar and taskbar.
/// The window icon in the user's colour and style (Settings > App icon); None: the built-in one.
fn app_icon(p: &Prefs, size: i32) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};
    let rgb = keel_core::appicon::parse_hex(&p.icon_color)?;
    let px = keel_core::appicon::render(keel_core::appicon::Style::from_index(p.icon_style), rgb, size as usize);
    unsafe {
        let bmi = BITMAPV5HEADER {
            bV5Size: size_of::<BITMAPV5HEADER>() as u32, bV5Width: size, bV5Height: -size, bV5Planes: 1, bV5BitCount: 32,
            bV5Compression: BI_BITFIELDS, bV5RedMask: 0x00FF_0000, bV5GreenMask: 0x0000_FF00, bV5BlueMask: 0x0000_00FF,
            bV5AlphaMask: 0xFF00_0000, ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi as *const _ as *const BITMAPINFO, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, px.len());
        for (d, s) in dst.chunks_mut(4).zip(px.chunks(4)) { d.copy_from_slice(&[s[2], s[1], s[0], s[3]]); }
        let mask = CreateBitmap(size, size, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color }).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// Settings > App icon: the swatches, the current choice and its preview.
fn show_icon_prefs(ui: &MainWindow, p: &Prefs) {
    let sw: Vec<Swatch> = keel_core::appicon::COLORS.iter().filter_map(|(_, h)| keel_core::appicon::parse_hex(h)
        .map(|(r, g, b)| Swatch { hex: (*h).into(), color: slint::Color::from_rgb_u8(r, g, b) })).collect();
    ui.set_icon_colors(ModelRc::new(VecModel::from(sw)));
    ui.set_icon_color(if p.icon_color.is_empty() { keel_core::appicon::COLORS[0].1.into() } else { p.icon_color.clone().into() });
    ui.set_icon_style(p.icon_style.min(1) as i32);
    ui.set_tray_icon_color(p.tray_icon_color);
    let rgb = keel_core::appicon::parse_hex(&ui.get_icon_color()).unwrap_or((31, 31, 31));
    let px = keel_core::appicon::render(keel_core::appicon::Style::from_index(p.icon_style), rgb, 64);
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&px, 64, 64);
    ui.set_icon_preview(slint::Image::from_rgba8(buf));
}

/// The Start menu and desktop shortcuts follow the app icon (the service writes them; an upgrade
/// puts the installer's icon back, so the window sends it again at start).
fn shortcut_icon(p: &Prefs) {
    let req = keel_service::Request::AppIcon { color: (!p.icon_color.is_empty()).then(|| p.icon_color.clone()), style: p.icon_style };
    std::thread::spawn(move || { let _ = keel_service::call(&req); });
}

/// This window's handle: the "Keel" top-level window of this thread (another Keel window may be open).
fn own_window() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{EnumThreadWindows, GetWindowTextW};
    use windows::core::BOOL;
    unsafe extern "system" fn each(h: HWND, out: LPARAM) -> BOOL {
        let mut buf = [0u16; 16];
        let n = unsafe { GetWindowTextW(h, &mut buf) } as usize;
        if String::from_utf16_lossy(&buf[..n]) == "Keel" { unsafe { *(out.0 as *mut isize) = h.0 as isize; } return false.into(); }
        true.into()
    }
    let mut found: isize = 0;
    unsafe { let _ = EnumThreadWindows(GetCurrentThreadId(), Some(each), LPARAM(&mut found as *mut isize as isize)); }
    (found != 0).then(|| HWND(found as _))
}

/// Brings the new window to the front. The tray or panel that started it allowed this
/// (AllowSetForegroundWindow); without that Windows opened it behind the active window.
fn bring_to_front() {
    use windows::Win32::UI::WindowsAndMessaging::{BringWindowToTop, SetForegroundWindow};
    let Some(h) = own_window() else { return };
    unsafe { let _ = BringWindowToTop(h); let _ = SetForegroundWindow(h); }
}

/// Redraws the whole window when Windows may have dropped its content. The software renderer
/// only draws what changed, so after a display mode or monitor change (integer scaling, eGPU
/// attach) parts of the window stayed see-through or showed an old page until they changed.
/// Checked with the 2 s timer: monitor, desktop size, monitor count, becoming active, and every
/// 10 s anyway (a full redraw takes a few milliseconds).
fn repaint_check(ui: &MainWindow) {
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::WindowsAndMessaging::*;
    thread_local! { static LAST: std::cell::Cell<(isize, i32, i32, i32, bool, u32)> = const { std::cell::Cell::new((0, 0, 0, 0, false, 0)) }; }
    let Some(h) = own_window() else { return };
    let sig = unsafe { (MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST).0 as isize, GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN), GetSystemMetrics(SM_CMONITORS), GetForegroundWindow() == h) };
    let (m, x, y, c, fg, n) = LAST.get();
    let changed = (m, x, y, c) != (sig.0, sig.1, sig.2, sig.3) || (sig.4 && !fg);
    let n = if changed || n >= 4 { ui.set_repaint(ui.get_repaint().wrapping_add(1)); 0 } else { n + 1 };
    LAST.set((sig.0, sig.1, sig.2, sig.3, sig.4, n));
}

fn set_window_icon() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::PCWSTR;
    unsafe {
        let Some(hwnd) = own_window() else { return };
        let Ok(inst) = GetModuleHandleW(None) else { return };
        let p = Prefs::load();
        // The taskbar takes the big icon at the system's icon size (48 px at 150 %).
        let big = GetSystemMetrics(SM_CXICON).max(32);
        for (which, size) in [(ICON_BIG, big), (ICON_SMALL, GetSystemMetrics(SM_CXSMICON).max(16))] {
            let h = match app_icon(&p, size) {
                Some(h) => Some(h),
                None => LoadImageW(Some(inst.into()), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR).ok().map(|h| HICON(h.0)),
            };
            if let Some(h) = h { SendMessageW(hwnd, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(h.0 as isize))); }
        }
    }
}

fn exe_beside(name: &str) -> std::path::PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(name))).unwrap_or_else(|| name.into())
}

fn net_call(ui: &MainWindow, data: Arc<Mutex<Data>>, req: keel_net::service::Request) {
    background(ui, move || keel_net::service::call(&req).map_err(|e| format!("{e:#}")), move |ui, r| {
        ui.set_status(match &r { Ok(r) => r.message.clone(), Err(e) => e.clone() }.into());
        show_net(ui, &r, &data);
    });
}

/// Page number for a tab name (`--tab`; old names from the tray and shortcuts keep working).
fn page_of(ui: &MainWindow, t: &str) -> i32 {
    let pages = ui.global::<Pages>();
    match t {
        "apps" | "families" => pages.get_apps(),
        "background" | "ondemand" | "on-demand" => pages.get_background(),
        "memory-history" | "memory-history+log" | "leaks" => pages.get_memory_history(),
        "egpu" | "external-gpu" => pages.get_external_gpu(),
        "games" | "gpu-apps" | "local-ai" => pages.get_games(),
        "displays" => pages.get_displays(),
        "network" => pages.get_network(),
        "settings" => pages.get_settings(),
        _ => pages.get_home(),
    }
}

/// Shows a page and loads its data, like a click in the side menu.
fn open_page(ui: &MainWindow, t: &str) {
    let p = page_of(ui, t);
    ui.set_page(p);
    if p == ui.global::<Pages>().get_displays() { ui.global::<DisplaysData>().invoke_refresh(); }
    if p == ui.global::<Pages>().get_external_gpu() { amdtune::load(ui); nv3d::load(ui); }
    if p == ui.global::<Pages>().get_games() {
        // Local AI is the second tab of GPU apps ("local-ai" opens it there).
        ui.global::<GamesData>().set_tab(if t == "local-ai" { 1 } else { 0 });
        ui.global::<GamesData>().invoke_refresh();
        ui.global::<LocalAiData>().invoke_refresh();
    }
    if p == ui.global::<Pages>().get_memory_history() {
        ui.global::<MemHistData>().invoke_reload();
        // "memory-history+log": with the log drawer open.
        if t.ends_with("+log") { ui.global::<MemHistData>().set_log_open(true); ui.global::<MemHistData>().invoke_log_refresh(); }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // The tray panel: `--flyout L,T,R,B` (the tray icon's rectangle, physical pixels).
    let argv: Vec<String> = std::env::args().collect();
    if let Some(r) = argv.windows(2).find(|w| w[0] == "--flyout").map(|w| w[1].clone()) {
        let v: Vec<i32> = r.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        return flyout::run(if let [l, t, r, b] = v[..] { (l, t, r, b) } else { (0, 0, 0, 0) });
    }
    // Its own taskbar identity: the taskbar then shows this window's icon (the user's colour,
    // Settings > App icon) instead of the Start menu shortcut's.
    unsafe { let _ = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::w!("Keel.Main")); }
    // The tray belongs with the window: start it when it is not running (it is one per session).
    if !keel_core::single::running("keel-tray") {
        let _ = keel_core::cmd::windowed(exe_beside("keel-tray.exe")).spawn();
    }
    let ui = MainWindow::new()?;
    { let p = Prefs::load(); if !p.icon_color.is_empty() { shortcut_icon(&p); } }
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_about_paths(format!("Program: {}   ·   Your data and tray log: {}   ·   Service data and log: {}",
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.display().to_string())).unwrap_or_default(),
        keel_core::paths::user_data_dir().display(), keel_core::paths::data_dir().display()).into());
    ui.on_open_folder(|which| {
        let dir = match which {
            0 => std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_default(),
            1 => keel_core::paths::user_data_dir(),
            _ => keel_core::paths::data_dir(),
        };
        let _ = keel_core::cmd::windowed("explorer.exe").arg(dir).spawn();
    });
    ui.set_force_scheme(match std::env::var("KEEL_THEME").as_deref() { Ok("dark") => 1, Ok("light") => 2, _ => 0 });
    let data = Arc::new(Mutex::new(Data::default()));
    let args: Vec<String> = std::env::args().collect();
    // --tab: that page at once, its data once the event loop runs (like a menu click).
    if let Some(t) = args.windows(2).find(|w| w[0] == "--tab").map(|w| w[1].clone()) {
        ui.set_page(page_of(&ui, &t));
        let w = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || if let Some(ui) = w.upgrade() { open_page(&ui, &t); });
    }
    // KEEL_GOTO=<tab> (checks only): switch to that page 3 s after start, like a menu click.
    if let Ok(t) = std::env::var("KEEL_GOTO") {
        let w = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_secs(3), move || if let Some(ui) = w.upgrade() {
            eprintln!("KEEL_GOTO {t} -> page {}", page_of(&ui, &t));
            open_page(&ui, &t);
        });
    }

    { let d = data.clone(); let w = ui.as_weak(); ui.on_refresh(move || if let Some(ui) = w.upgrade() {
        ui.set_status("Refreshing...".into());
        load(&ui, d.clone());
        if ui.get_page() == ui.global::<Pages>().get_memory_history() { ui.global::<MemHistData>().invoke_reload(); }
        if ui.get_page() == ui.global::<Pages>().get_displays() { ui.global::<DisplaysData>().invoke_refresh(); }
    }); }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_sort_table(move |table, col, asc| {
            let Some(ui) = w.upgrade() else { return };
            let mut g = d.lock().unwrap();
            let Some(slot) = g.sorts.get_mut(table as usize) else { return };
            *slot = Some((col.max(0) as usize, asc));
            g.show_tables(&ui);
            drop(g);
            // The selected row index now points at another entry: refresh what depends on it.
            if table == 1 && ui.get_fam_row() >= 0 { ui.invoke_family_selected(ui.get_fam_row()); }
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_family_selected(move |r| {
            let Some(ui) = w.upgrade() else { return };
            let d = d.lock().unwrap();
            if let Some(f) = d.families.get(r as usize) {
                ui.set_details(family_text(f).into());
                let hints = ondemand::parts(f).map(|p| p.app_hints.join(", ")).unwrap_or_default();
                ui.set_app_hint(if hints.is_empty() { SharedString::new() } else { format!("app exe (startup items run: {hints})").into() });
                let current = d.od.as_ref().and_then(|s| s.entries.iter().find(|e| e.key == f.key)).map(|e| e.apps.join(" ")).unwrap_or_default();
                ui.set_app_exe(current.into());
                let p = keel_core::traycfg::Prefs::load();
                ui.set_fam_ai(p.ai_mode_apps.iter().any(|a| a.key == f.key));
                ui.set_fam_game(p.game_mode_apps.iter().any(|a| a.key == f.key));
            }
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_set_mode(move |mode| {
            let Some(ui) = w.upgrade() else { return };
            let Some(f) = d.lock().unwrap().families.get(ui.get_fam_row() as usize).cloned() else { return };
            let apps: Vec<String> = ui.get_app_exe().split([' ', ',', ';']).filter(|s| !s.is_empty()).map(String::from).collect();
            let mode = mode.to_string();
            if mode == "follow" && apps.is_empty() { ui.set_status("Follow needs the app exe (e.g. OneDrive.exe)".into()); return; }
            ui.set_status(format!("{}: {} ...", f.name, match mode.as_str() { "off" => "keeping off", "follow" => "following its app", _ => "back to normal" }).into());
            let d2 = d.clone();
            background(&ui, move || {
                let parts = ondemand::parts(&f).map_err(anyhow::Error::msg)?;
                let m = match mode.as_str() { "off" => Some(Mode::Off), "follow" => Some(Mode::Follow), _ => None };
                let r = pipe::call(&Request::Set { family: f.name.clone(), key: f.key.clone(), mode: m, apps })?;
                if r.ok || m.is_none() {
                    if m.is_some() { if !parts.hkcu.is_empty() { userside::user_off(&f.key, &parts.hkcu)?; } } else { userside::user_restore(&f.key)?; }
                }
                Ok::<_, anyhow::Error>(r.message)
            }, move |ui, r| {
                ui.set_status(match r { Ok(m) => m.into(), Err(e) => format!("{e:#}").into() });
                load(ui, d2);
            });
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_turn(move |on| {
            let Some(ui) = w.upgrade() else { return };
            let Some(key) = d.lock().unwrap().od.as_ref().and_then(|s| s.entries.get(ui.get_managed_row() as usize)).map(|e| e.key.clone()) else { return };
            let d2 = d.clone();
            background(&ui, move || pipe::call(&Request::Turn { key, on }), move |ui, r| {
                ui.set_status(match r { Ok(r) => r.message.into(), Err(e) => format!("{e:#}").into() });
                load(ui, d2);
            });
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_release(move || {
            let Some(ui) = w.upgrade() else { return };
            let Some(e) = d.lock().unwrap().od.as_ref().and_then(|s| s.entries.get(ui.get_managed_row() as usize)).cloned() else { return };
            let d2 = d.clone();
            background(&ui, move || {
                let r = pipe::call(&Request::Set { family: e.family.clone(), key: e.key.clone(), mode: None, apps: vec![] })?;
                userside::user_restore(&e.key)?;
                Ok::<_, anyhow::Error>(r.message)
            }, move |ui, r| {
                ui.set_status(match r { Ok(m) => m.into(), Err(e) => format!("{e:#}").into() });
                load(ui, d2);
            });
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_fam_ai_toggled(move |on| {
            let Some(ui) = w.upgrade() else { return };
            let Some(f) = d.lock().unwrap().families.get(ui.get_fam_row() as usize).cloned() else { return };
            if on && f.protected.is_some() {
                ui.set_fam_ai(false);
                ui.set_status(format!("{} is protected: Keel does not switch it off", f.name).into());
                return;
            }
            let mut p = keel_core::traycfg::Prefs::load();
            p.ai_mode_apps.retain(|a| a.key != f.key);
            if on { p.ai_mode_apps.push(keel_core::traycfg::AiApp { key: f.key.clone(), name: f.name.clone() }); }
            ui.set_status(match p.save() {
                Ok(()) if on => format!("{}: switched off in AI mode{}", f.name, if p.ai_mode { "" } else { " (AI mode is off: GPU apps > Local AI)" }).into(),
                Ok(()) => format!("{}: not in AI mode", f.name).into(),
                Err(e) => format!("{e:#}").into(),
            });
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_fam_game_toggled(move |on| {
            let Some(ui) = w.upgrade() else { return };
            let Some(f) = d.lock().unwrap().families.get(ui.get_fam_row() as usize).cloned() else { return };
            if on && f.protected.is_some() {
                ui.set_fam_game(false);
                ui.set_status(format!("{} is protected: Keel does not switch it off", f.name).into());
                return;
            }
            let mut p = keel_core::traycfg::Prefs::load();
            p.game_mode_apps.retain(|a| a.key != f.key);
            if on { p.game_mode_apps.push(keel_core::traycfg::AiApp { key: f.key.clone(), name: f.name.clone() }); }
            ui.set_status(match p.save() {
                Ok(()) if on => format!("{}: switched off in game mode{}", f.name, if p.game_mode { "" } else { " (game mode is off: GPU apps)" }).into(),
                Ok(()) => format!("{}: not in game mode", f.name).into(),
                Err(e) => format!("{e:#}").into(),
            });
            games::refresh(&ui);
        });
    }
    games::wire(&ui);
    amdtune::wire(&ui);
    localai::wire(&ui);
    nv3d::wire(&ui);
    {
        let w = ui.as_weak();
        ui.global::<LocalAiData>().on_refresh(move || { if let Some(ui) = w.upgrade() { localai::refresh(&ui); } });
        let w = ui.as_weak();
        ui.global::<LocalAiData>().on_unload(move |m| { if let Some(ui) = w.upgrade() { localai::unload_one(&ui, m.to_string()); } });
        let w = ui.as_weak();
        ui.global::<LocalAiData>().on_stop_ollama(move || { if let Some(ui) = w.upgrade() { localai::stop_ollama(&ui); } });
    }
    {
        let w = ui.as_weak();
        ui.global::<DisplaysData>().on_refresh(move || { if let Some(ui) = w.upgrade() { displays::show(&ui); displays::load_igpu(&ui); } });
        let w = ui.as_weak();
        ui.global::<DisplaysData>().on_set_mode(move |c, i| { if let Some(ui) = w.upgrade() { displays::set_mode(&ui, c, i); } });
        let w = ui.as_weak();
        ui.global::<DisplaysData>().on_integer(move |c, on| { if let Some(ui) = w.upgrade() { displays::integer(&ui, c, on); } });
        let w = ui.as_weak();
        ui.global::<DisplaysData>().on_make_main(move |c| { if let Some(ui) = w.upgrade() { displays::make_main(&ui, c); } });
    }
    {
        // Memory history: samples load on a worker thread; the rest works on what is loaded.
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_reload(move || {
            let Some(ui) = w.upgrade() else { return };
            ui.set_history_loaded(true);
            let range = ui.global::<MemHistData>().get_range().clamp(0, memhistory::RANGES - 1);
            ui.global::<MemHistData>().set_note("Loading samples...".into());
            background(&ui, move || memhistory::load(range), memhistory::show);
        });
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_select(move |r| {
            let Some(ui) = w.upgrade() else { return };
            ui.global::<MemHistData>().set_selected(r);
            memhistory::detail(&ui);
        });
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_watch(move |r| {
            let Some(ui) = w.upgrade() else { return };
            if memhistory::toggle_watch(r) { ui.global::<MemHistData>().invoke_reload(); }
        });
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_watch_name(move |name| {
            let Some(ui) = w.upgrade() else { return };
            let mut p = Prefs::load();
            if !p.watched.iter().any(|x| x.eq_ignore_ascii_case(&name)) { p.watched.push(name.to_string()); }
            ui.set_status(match p.save() { Ok(()) => format!("Watching {name}").into(), Err(e) => format!("{e:#}").into() });
            ui.global::<MemHistData>().invoke_reload();
        });
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_fast_changed(move || {
            let Some(ui) = w.upgrade() else { return };
            let mut p = Prefs::load();
            p.fast_samples = ui.global::<MemHistData>().get_fast();
            ui.set_status(match p.save() { Ok(()) => "Saved; the tray samples at the new pace from its next minute".into(), Err(e) => format!("{e:#}").into() });
        });
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_ignore(move |r| { if let Some(ui) = w.upgrade() { memhistory::ignore(&ui, r); } });
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_clear_ignored(move || { if let Some(ui) = w.upgrade() { memhistory::clear_ignored(&ui); } });
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_log_refresh(move || { if let Some(ui) = w.upgrade() { memhistory::show_log(&ui); } });
        ui.global::<MemHistData>().on_log_folder(memhistory::open_log_folder);
        let w = ui.as_weak();
        ui.global::<MemHistData>().on_keep_changed(move || {
            let Some(ui) = w.upgrade() else { return };
            let mut p = Prefs::load();
            p.keep_memory_history = ui.global::<MemHistData>().get_keep();
            ui.set_status(match p.save() { Ok(()) => "Saved; the tray uses it from its next sample".into(), Err(e) => format!("{e:#}").into() });
        });
        // Show in Apps: the family whose processes include this one.
        let d = data.clone(); let w = ui.as_weak();
        ui.global::<MemHistData>().on_show_app(move |r| {
            let Some(ui) = w.upgrade() else { return };
            let Some(name) = memhistory::process_name(r) else { return };
            let row = d.lock().unwrap().families.iter().position(|f| f.members.iter().any(|m| m.kind == "process" && m.name.eq_ignore_ascii_case(&name)));
            ui.set_page(ui.global::<Pages>().get_apps());
            match row {
                Some(i) => { ui.set_fam_row(i as i32); ui.invoke_family_selected(i as i32); }
                None => ui.set_status(format!("{name} is not in a listed family (small or already closed)").into()),
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_icon_changed(move || {
            let Some(ui) = w.upgrade() else { return };
            let hex = ui.get_icon_color().trim().to_lowercase();
            if keel_core::appicon::parse_hex(&hex).is_none() { ui.set_status("Colour: use #rrggbb".into()); return; }
            let mut p = Prefs::load();
            p.icon_color = hex;
            p.icon_style = ui.get_icon_style().clamp(0, 1) as u8;
            p.tray_icon_color = ui.get_tray_icon_color();
            ui.set_status(match p.save() { Ok(()) => "Icon saved".into(), Err(e) => format!("{e:#}").into() });
            show_icon_prefs(&ui, &p);
            set_window_icon();
            shortcut_icon(&p);
        });
    }
    {
        ui.on_side_menu_changed(|mode| {
            let mut p = Prefs::load();
            p.side_menu = mode.clamp(0, 2) as u8;
            let _ = p.save();
        });
    }
    {
        let w = ui.as_weak();
        ui.global::<EgpuData>().on_options_changed(move || {
            let Some(ui) = w.upgrade() else { return };
            let d = ui.global::<EgpuData>();
            let mut p = Prefs::load();
            p.legion_space_pin = d.get_opt_legion();
            p.legion_space_screen = Some(d.get_opt_legion_screen());
            p.egpu_primary_display = d.get_opt_primary();
            p.prune_amd_tray_icons = d.get_opt_prune();
            p.restart_search_after_scaling = d.get_opt_search();
            p.notifications = d.get_opt_notify();
            ui.set_status(match p.save() { Ok(()) => "Saved; the tray applies it within seconds".into(), Err(e) => format!("{e:#}").into() });
        });
    }
    {
        let w = ui.as_weak();
        ui.global::<EgpuData>().on_widgets_changed(move || {
            let Some(ui) = w.upgrade() else { return };
            let d = ui.global::<EgpuData>();
            let on = [d.get_w_history(), d.get_w_gauges(), d.get_w_bars(), d.get_w_tuning(), d.get_w_stress()];
            let mut p = Prefs::load();
            p.hidden_widgets.retain(|x| x.starts_with("home."));
            p.hidden_widgets.extend(WIDGETS.iter().zip(on).filter(|(_, on)| !on).map(|(w, _)| w.to_string()));
            p.egpu_tuning_first = d.get_tuning_first();
            let _ = p.save();
        });
    }
    {
        let w = ui.as_weak();
        ui.global::<HomeData>().on_move_widget(move |i, delta| {
            let Some(ui) = w.upgrade() else { return };
            let mut p = Prefs::load();
            p.home_order = traycfg::moved(traycfg::home_order(&p.home_order), i.max(0) as usize, delta);
            let _ = p.save();
            ui.global::<HomeData>().set_order(ModelRc::new(VecModel::from(p.home_order.iter().cloned().map(SharedString::from).collect::<Vec<_>>())));
        });
    }
    {
        let w = ui.as_weak();
        ui.global::<HomeData>().on_widgets_changed(move || {
            let Some(ui) = w.upgrade() else { return };
            let h = ui.global::<HomeData>();
            let mut p = Prefs::load();
            p.hidden_widgets.retain(|x| !x.starts_with("home."));
            for (id, on) in [("home.memory", h.get_w_mem()), ("home.egpu", h.get_w_egpu()), ("home.network", h.get_w_net())] {
                if !on { p.hidden_widgets.push(id.to_string()); }
            }
            let _ = p.save();
        });
    }
    {
        // Read-only `keel` commands run hidden; their output shows on the page (no console window).
        let w = ui.as_weak();
        ui.global::<EgpuData>().on_run(move |args| {
            let Some(ui) = w.upgrade() else { return };
            let args: Vec<String> = args.split_whitespace().map(String::from).collect();
            ui.global::<EgpuData>().set_output(format!("keel {} ...", args.join(" ")).into());
            background(&ui, move || {
                match keel_core::cmd::hidden(exe_beside("keel.exe")).args(&args).output() {
                    Ok(o) => {
                        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                        let e = String::from_utf8_lossy(&o.stderr);
                        if !e.trim().is_empty() { s += &format!("\n{e}"); }
                        format!("> keel {}\n\n{}", args.join(" "), s.trim_end())
                    }
                    Err(e) => format!("keel {}: {e}", args.join(" ")),
                }
            }, |ui, text| ui.global::<EgpuData>().set_output(text.into()));
        });
    }
    {
        // Tuning: the service saves the limits in the eGPU's profile and re-attaches.
        fn send(ui: &MainWindow, power: Option<u32>, clock: Option<u32>, mem: Option<u32>) {
            let req = keel_service::Request::EgpuLimits { power_limit_w: power, clock_max_mhz: clock, mem_clock_max_mhz: mem };
            background(ui, move || keel_service::call(&req).map(|r| (r.ok, r.message)).map_err(|e| format!("{e:#}")),
                |ui, r| {
                    if matches!(r, Ok((true, _))) { EGPU.with_borrow_mut(egpu_page::reset_peaks); }
                    ui.set_status(match r {
                        Ok((true, m)) => m.into(),
                        Ok((false, m)) | Err(m) => format!("Not applied: {m}").into(),
                    });
                    EGPU.with_borrow_mut(|pg| pg.seeded = false);
                });
        }
        let w = ui.as_weak();
        ui.global::<EgpuData>().on_apply_limits(move || {
            let Some(ui) = w.upgrade() else { return };
            let (p, c, m) = egpu_page::limits(&ui);
            send(&ui, p, c, m);
        });
        let w = ui.as_weak();
        ui.global::<EgpuData>().on_default_limits(move || {
            let Some(ui) = w.upgrade() else { return };
            send(&ui, None, None, None);
        });
    }
    {
        let w = ui.as_weak();
        ui.global::<EgpuData>().on_start_stress(move || {
            let Some(ui) = w.upgrade() else { return };
            let d = ui.global::<EgpuData>();
            let secs = stress::LENGTHS[d.get_stress_length().clamp(0, 2) as usize];
            match stress::Run::start(&exe_beside("keel.exe"), secs) {
                Ok(r) => {
                    EGPU.with_borrow_mut(|pg| pg.stress = Some(r));
                    d.set_stress_results(egpu_page::rows(Vec::new()));
                    d.set_stress_running(true);
                    d.set_stress_status("Starting...".into());
                }
                Err(e) => d.set_stress_status(format!("Could not start: {e}").into()),
            }
        });
        let w = ui.as_weak();
        ui.global::<EgpuData>().on_stop_stress(move || {
            let Some(ui) = w.upgrade() else { return };
            EGPU.with_borrow_mut(|pg| if let Some(r) = pg.stress.as_mut() { r.stop(); });
            ui.global::<EgpuData>().set_stress_status("Stopping...".into());
        });
    }
    {
        let w = ui.as_weak();
        ui.on_fav_toggled(move |i, on| {
            let Some(ui) = w.upgrade() else { return };
            let mut p = Prefs::load();
            let Some((id, _, _)) = traycfg::ITEMS.get(i as usize) else { return };
            p.favorites.retain(|f| f != id);
            if on {
                // Keep the menu in ITEMS order.
                p.favorites.push(id.to_string());
                p.favorites.sort_by_key(|f| traycfg::ITEMS.iter().position(|(x, _, _)| x == f).unwrap_or(usize::MAX));
            }
            ui.set_status(match p.save() { Ok(()) => "Tray menu saved".into(), Err(e) => format!("{e:#}").into() });
            show_prefs(&ui, &p);
        });
    }
    {
        let w = ui.as_weak();
        ui.on_feature_toggled(move |name, on| {
            let Some(ui) = w.upgrade() else { return };
            let name = name.to_string();
            background(&ui, move || keel_service::call(&keel_service::Request::SetFeature { name, on }).map_err(|e| format!("{e:#}")), |ui, r| {
                match r {
                    Ok(r) => { ui.set_status(r.message.clone().into()); show_service(ui, &Some(r)); }
                    Err(e) => { ui.set_status(e.into()); show_service(ui, &None); }
                }
            });
        });
    }
    // Network page: edits go to the in-memory config; "Save and apply" sends it to the service.
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_vpn_set(move |r, via, kill| {
            let Some(ui) = w.upgrade() else { return };
            let mut g = d.lock().unwrap();
            let Some(c) = g.net.as_mut() else { return };
            if let Some(v) = c.vpns.get_mut(r as usize) {
                v.via = index_side(via);
                v.kill_switch = kill;
            }
            ui.set_vpn_list(vpn_model(c));
            ui.set_net_dirty(true);
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_vpn_remove_at(move |r| {
            let Some(ui) = w.upgrade() else { return };
            let mut g = d.lock().unwrap();
            let Some(c) = g.net.as_mut() else { return };
            if (r as usize) < c.vpns.len() { c.vpns.remove(r as usize); }
            ui.set_vpn_list(vpn_model(c));
            ui.set_net_dirty(true);
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_vpn_add(move || {
            let Some(ui) = w.upgrade() else { return };
            // A mistake stays in the open form, in red, next to the fields.
            let v = match form_vpn(&ui) { Ok(v) => v, Err(e) => { ui.set_form_error(e.into()); return; } };
            let mut g = d.lock().unwrap();
            let c = g.net.get_or_insert_with(NetConfig::default);
            let at = ui.get_edit_index();
            if c.vpns.iter().enumerate().any(|(i, x)| i as i32 != at && x.name.eq_ignore_ascii_case(&v.name)) {
                ui.set_form_error(format!("A VPN named {} exists: use its Edit button, or pick another name", v.name).into());
                return;
            }
            let note = format!("{} {}. Not saved yet: Save and apply.", v.name, if at < 0 { "added" } else { "changed" });
            match usize::try_from(at).ok().and_then(|i| c.vpns.get_mut(i)) { Some(x) => *x = v, None => c.vpns.push(v) }
            ui.set_vpn_list(vpn_model(c));
            // Done: the form closes and empties; the note under the list says what changed.
            fill_form(&ui, &Vpn { kill_switch: true, ..Default::default() });
            ui.set_form_error(SharedString::new());
            ui.set_edit_index(-1);
            ui.set_add_open(false);
            ui.set_pick_open(false);
            ui.set_net_dirty(true);
            ui.set_vpn_note(note.clone().into());
            ui.set_status(note.into());
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_vpn_edit(move |i| {
            let Some(ui) = w.upgrade() else { return };
            let v = usize::try_from(i).ok().and_then(|i| d.lock().unwrap().net.as_ref().and_then(|c| c.vpns.get(i).cloned()));
            fill_form(&ui, &v.unwrap_or(Vpn { kill_switch: true, ..Default::default() }));
            ui.set_edit_index(i);
            ui.set_form_error(SharedString::new());
            ui.set_pick_open(false);
            ui.set_add_open(true);
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_vpn_preset(move |i| {
            let Some(ui) = w.upgrade() else { return };
            let Some(v) = usize::try_from(i - 1).ok().and_then(|i| Vpn::presets().get(i).cloned()) else { return };
            // A new VPN from a preset whose name is taken: edit that one instead of failing at Add.
            let at = d.lock().unwrap().net.as_ref().and_then(|c| c.vpns.iter().position(|x| x.name.eq_ignore_ascii_case(&v.name)));
            if let Some(at) = at.filter(|&at| at as i32 != ui.get_edit_index()) {
                ui.set_edit_index(at as i32);
                ui.set_form_error(format!("{} already exists: this form now changes it (Done to keep)", v.name).into());
            } else {
                ui.set_form_error(SharedString::new());
            }
            fill_form(&ui, &v);
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_pick_load(move |kind| {
            let Some(ui) = w.upgrade() else { return };
            let picks = if kind == 0 { running_programs() } else { adapter_picks() };
            ui.set_pick_kind(kind);
            ui.set_pick_filter(SharedString::new());
            show_picks(&ui, &picks);
            d.lock().unwrap().picks = picks;
            ui.set_pick_open(true);
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_pick_filter_changed(move || { if let Some(ui) = w.upgrade() { show_picks(&ui, &d.lock().unwrap().picks); } });
    }
    {
        let w = ui.as_weak();
        ui.on_pick_choose(move |value| {
            let Some(ui) = w.upgrade() else { return };
            let programs = ui.get_pick_kind() == 0;
            let text = if programs { ui.get_new_exe() } else { ui.get_new_adapter() };
            let mut list: Vec<String> = text.split(if programs { '\n' } else { ',' }).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            if !list.iter().any(|x| x.eq_ignore_ascii_case(&value)) { list.push(value.to_string()); }
            if programs { ui.set_new_exe(list.join("\n").into()); } else { ui.set_new_adapter(list.join(", ").into()); }
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_route_add(move || {
            let Some(ui) = w.upgrade() else { return };
            let prefix = ui.get_new_route().trim().to_string();
            if keel_net::model::parse_prefix(&prefix).is_none() { ui.set_status("A route is a.b.c.d or a.b.c.d/n".into()); return; }
            let mut g = d.lock().unwrap();
            let c = g.net.get_or_insert_with(NetConfig::default);
            c.routes.push(RouteRule { prefix, via: index_side(ui.get_new_route_via()) });
            ui.set_route_list(route_model(c));
            ui.set_new_route(SharedString::new());
            ui.set_net_dirty(true);
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_route_remove_at(move |i| {
            let Some(ui) = w.upgrade() else { return };
            let mut g = d.lock().unwrap();
            let Some(c) = g.net.as_mut() else { return };
            if (i as usize) < c.routes.len() { c.routes.remove(i as usize); }
            ui.set_route_list(route_model(c));
            ui.set_net_dirty(true);
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_route_set(move |i, via| {
            let Some(ui) = w.upgrade() else { return };
            let mut g = d.lock().unwrap();
            let Some(c) = g.net.as_mut() else { return };
            if let Some(r) = c.routes.get_mut(i as usize) { r.via = index_side(via); }
            ui.set_route_list(route_model(c));
            ui.set_net_dirty(true);
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_net_save(move || {
            let Some(ui) = w.upgrade() else { return };
            // An open VPN form is part of what is saved; a form with a mistake stops the save.
            if ui.get_add_open() {
                ui.invoke_vpn_add();
                if ui.get_add_open() { ui.set_net_dirty(true); ui.set_status("Not saved: fix the VPN form first".into()); return; }
            }
            ui.set_net_dirty(false);
            ui.set_vpn_note(SharedString::new());
            let mut c = d.lock().unwrap().net.clone().unwrap_or_default();
            if let Some(v) = d.lock().unwrap().net_lans.get(ui.get_net_lan() as usize) { c.lan_descriptions = v.clone(); }
            c.internet = index_side(ui.get_net_internet());
            c.enabled = ui.get_net_enabled();
            c.keep_wifi_with_lan = ui.get_net_keep_wifi();
            c.tailscale_wifi_only = ui.get_net_ts_wifi();
            net_call(&ui, d.clone(), keel_net::service::Request::SetConfig { config: c });
        });
    }
    {
        let d = data.clone(); let w = ui.as_weak();
        ui.on_net_reapply(move || { if let Some(ui) = w.upgrade() { net_call(&ui, d.clone(), keel_net::service::Request::Apply); } });
    }

    load(&ui, data.clone());
    let timer = slint::Timer::default();
    {
        let w = ui.as_weak();
        timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(2), move || {
            // Home and External GPU show it; a running stress test is followed on any page.
            if let Some(ui) = w.upgrade() {
                repaint_check(&ui);
                if matches!(ui.get_page(), 0 | 4) || EGPU.with_borrow(|pg| pg.stress.is_some()) { show_egpu(&ui); }
                if ui.get_page() == 0 { home::memory_tick(&ui); }
                // GPU apps (eGPU memory, Local AI tab): every other tick (4 s), on a worker thread.
                if ui.get_page() == ui.global::<Pages>().get_games() && TICK.with(|t| { t.set(t.get() + 1); t.get() % 2 == 0 }) {
                    localai::refresh(&ui);
                }
            }
        });
    }
    {
        let w = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
            set_window_icon();
            if let Some(h) = own_window() {
                redraw::watch(h, move || if let Some(ui) = w.upgrade() { ui.set_repaint(ui.get_repaint().wrapping_add(1)); });
            }
            bring_to_front();
        });
    }
    if ui.get_page() == ui.global::<Pages>().get_memory_history() { ui.global::<MemHistData>().invoke_reload(); }
    if ui.get_page() == ui.global::<Pages>().get_displays() { ui.global::<DisplaysData>().invoke_refresh(); }
    if ui.get_page() == ui.global::<Pages>().get_games() { ui.global::<GamesData>().invoke_refresh(); ui.global::<LocalAiData>().invoke_refresh(); }
    let _ = (Rc::new(()), ui.get_favorites().row_count()); // keep imports used in all builds
    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn cells_sort_as_numbers_or_text() {
        assert_eq!(cmp_cell("9", "10"), Less);
        assert_eq!(cmp_cell("1200.5", "300"), Greater);
        assert_eq!(cmp_cell("", "0"), Less);
        assert_eq!(cmp_cell("! Adobe", "microsoft"), Less);
        assert_eq!(cmp_cell("servers (2)", "default route"), Greater);
        let mut v = vec![vec!["b".to_string(), "5".into()], vec!["a".into(), "40".into()], vec!["c".into(), "5".into()]];
        sort_rows(&mut v, |r| r.clone(), (1, false));
        assert_eq!(v.iter().map(|r| r[0].as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
    }
}

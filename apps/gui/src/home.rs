//! Home page widgets (ui/home.slint): Memory (free / used / commit, a short history, largest app
//! families, on-demand apps), External GPU (name, power / temperature / usage, power history),
//! Network (where traffic goes, each VPN, problems) and the service's status lights.
use crate::{HomeData, Line, Light, MainWindow};
use keel_ram::family::Family;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Free memory over the last 3 hours: this session's 5-minute samples (the tray records them),
/// then one point per refresh while the window is open.
const MEM_SPAN: Duration = Duration::from_secs(3 * 3600);

thread_local! {
    static MEM: RefCell<VecDeque<(Instant, f64)>> = Default::default();
    static SEEDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Puts this session's recorded free memory in front of the live points (once per window).
fn seed(h: &mut VecDeque<(Instant, f64)>, now: Instant) {
    if SEEDED.with(|s| s.replace(true)) { return; }
    let unix = chrono::Utc::now().timestamp();
    for (t, free) in keel_ram::observe::session_free() {
        let Some(at) = now.checked_sub(Duration::from_secs((unix - t).max(0) as u64)) else { continue };
        h.push_back((at, free as f64 / 1073741824.0));
    }
}

fn lines(v: Vec<Line>) -> ModelRc<Line> { ModelRc::new(VecModel::from(v)) }

fn line(label: &str, text: String, frac: f32, warn: bool) -> Line {
    Line { label: label.into(), text: text.into(), frac, warn }
}

/// Free / used / commit from Windows (cheap; every refresh while Home is open).
pub fn memory_tick(ui: &MainWindow) {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_err() { return; }
    let gb = |b: u64| b as f64 / 1073741824.0;
    let d = ui.global::<HomeData>();
    let used = 1.0 - m.ullAvailPhys as f64 / m.ullTotalPhys.max(1) as f64;
    d.set_mem_free(format!("{:.1} GB free", gb(m.ullAvailPhys)).into());
    d.set_mem_sub(format!("of {:.1} GB in this machine", gb(m.ullTotalPhys)).into());
    d.set_mem_frac(used as f32);
    let commit = 1.0 - m.ullAvailPageFile as f64 / m.ullTotalPageFile.max(1) as f64;
    d.set_commit_frac(commit as f32);
    d.set_commit_text(format!("{:.1} / {:.1} GB", gb(m.ullTotalPageFile - m.ullAvailPageFile), gb(m.ullTotalPageFile)).into());
    let now = Instant::now();
    let (path, since) = MEM.with_borrow_mut(|h| {
        seed(h, now);
        h.push_back((now, gb(m.ullAvailPhys)));
        while h.front().is_some_and(|(t, _)| now.duration_since(*t) > MEM_SPAN) { h.pop_front(); }
        let pts: Vec<(Instant, Option<f64>)> = h.iter().map(|(t, v)| (*t, Some(*v))).collect();
        (crate::egpu_page::path(&pts, now, MEM_SPAN, gb(m.ullTotalPhys)), h.front().map_or(0, |(t, _)| now.duration_since(*t).as_secs()))
    });
    d.set_mem_path(path.into());
    d.set_mem_note(if since >= 3600 { format!("Free memory, last {:.1} h", since as f64 / 3600.0) }
                   else { format!("Free memory, last {} min", (since / 60).max(1)) }.into());
}

/// The five largest app families, as a share of physical memory.
pub fn top(ui: &MainWindow, families: &[Family], total: u64) {
    let v: Vec<Line> = families.iter().take(5).map(|f| {
        let mb = f.memory as f64 / 1048576.0;
        let text = if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{mb:.0} MB") };
        line(&f.name, text, if total > 0 { f.memory as f32 / total as f32 } else { -1.0 }, false)
    }).collect();
    ui.global::<HomeData>().set_top(lines(v));
}

/// On-demand apps (families Keel keeps off until used) in one line.
pub fn background(ui: &MainWindow, od: Option<&keel_ram::ondemand::State>) {
    let text = match od {
        None => "On-demand apps: the service is not running.".to_string(),
        Some(s) if s.entries.is_empty() => "On-demand apps: none.".into(),
        Some(s) => {
            let off = s.entries.iter().filter(|e| !e.on).count();
            format!("On-demand apps off: {} ({}){}.", off,
                s.entries.iter().map(|e| e.family.as_str()).take(4).collect::<Vec<_>>().join(", "),
                if s.entries.len() > off { format!(", {} on for now", s.entries.len() - off) } else { String::new() })
        }
    };
    ui.global::<HomeData>().set_bg_note(text.into());
}

/// External GPU widget from the eGPU page's state (same telemetry and power history).
pub fn egpu(ui: &MainWindow, pg: &crate::egpu_page::Page, alive: bool, name: &str,
            t: Option<&keel_egpu::telemetry::GpuTelemetry>, status: Option<&serde_json::Value>) {
    let d = ui.global::<HomeData>();
    let Some(t) = t.filter(|_| alive) else {
        d.set_egpu_on(false);
        d.set_egpu_name(if alive { "No eGPU attached".into() } else { "Service not running".into() });
        d.set_egpu_lines(lines(Vec::new()));
        d.set_egpu_facts(lines(Vec::new()));
        d.set_egpu_note(if alive { "Not connected".into() }
                        else { "eGPU values come from the Keel service.".into() });
        return;
    };
    d.set_egpu_on(true);
    d.set_egpu_name(name.into());
    let limit = t.power_limit_w.filter(|l| *l > 0.0);
    let temp = t.hotspot_c.or(t.temp_c);
    let mut v = Vec::new();
    if let Some(w) = t.power_w {
        v.push(line("Power", limit.map_or(format!("{w:.0} W"), |l| format!("{w:.0} / {l:.0} W")),
                    limit.map_or(-1.0, |l| w / l), limit.is_some_and(|l| w >= l * 0.95)));
    }
    if let Some(c) = temp {
        v.push(line(if t.hotspot_c.is_some() { "Hotspot" } else { "Temperature" }, format!("{c:.0} °C"), c / 100.0,
                    c >= if t.hotspot_c.is_some() { 95.0 } else { 83.0 }));
    }
    if let Some(u) = t.usage_pct { v.push(line("Usage", format!("{u:.0} %"), u / 100.0, false)); }
    if let (Some(u), Some(tot)) = (t.vram_used_mb, t.vram_total_mb) {
        let f = u as f32 / tot.max(1) as f32;
        v.push(line("VRAM", format!("{:.1} / {:.0} GB", u as f64 / 1024.0, tot as f64 / 1024.0), f, f >= 0.9));
    }
    d.set_egpu_lines(lines(v));
    // Power over the last 5 minutes from the eGPU page's history.
    let now = Instant::now();
    let top = t.power_max_w.or(limit.map(|l| l * 1.25)).unwrap_or(300.0) as f64;
    let pts: Vec<(Instant, Option<f64>)> = pg.history.iter().map(|s| (s.t, s.power.map(f64::from))).collect();
    d.set_egpu_path(crate::egpu_page::path(&pts, now, Duration::from_secs(300), top).into());
    let held: Vec<&str> = t.held_back.iter().map(String::as_str).filter(|h| *h != "idle").collect();
    let mut facts = vec![line("Game running", status.and_then(|s| s["game"].as_str()).unwrap_or("none").to_string(), -1.0, false)];
    facts.push(line("Clock held back by", if held.is_empty() { "nothing".into() } else { held.join(", ") }, -1.0, !held.is_empty()));
    d.set_egpu_facts(lines(facts));
    d.set_egpu_note(SharedString::new());
}

/// Network widget from the service's reply.
pub fn net(ui: &MainWindow, r: &Result<keel_net::service::Reply, String>) {
    let d = ui.global::<HomeData>();
    match r {
        Ok(r) => {
            d.set_net_on(true);
            let (head, sub) = net_headline(r);
            d.set_net_summary(head.into());
            d.set_net_sub(sub.into());
            let up = |u: Option<bool>| match u { Some(true) => "connected", Some(false) => "not connected", None => "?" };
            let mut v = Vec::new();
            if let Some(c) = r.config.as_ref().filter(|c| c.enabled) {
                v.push(line("Internet you chose", c.internet.to_string(), -1.0, false));
                if r.lan_up.is_some() {
                    v.push(line("LAN", up(r.lan_up).into(), -1.0, c.internet == keel_net::model::Side::Lan && r.lan_up == Some(false)));
                    v.push(line("Wi-Fi", up(r.wifi_up).into(), -1.0, c.internet == keel_net::model::Side::Wifi && r.wifi_up == Some(false)));
                }
                for vpn in &c.vpns {
                    v.push(line(&format!("VPN {}", vpn.name), format!("over {}{}", vpn.via, if vpn.kill_switch { " · kill switch" } else { "" }), -1.0, false));
                }
                v.push(line("Wi-Fi stays on with LAN", if c.keep_wifi_with_lan { "yes" } else { "no" }.into(), -1.0, false));
            }
            d.set_net_lines(lines(v));
            let warn = !r.problems.is_empty();
            d.set_net_warn(warn);
            d.set_net_state(if warn { 2 } else if r.pending.is_empty() { 0 } else { 1 });
            d.set_net_note(if warn { r.problems.join("; ") }
                           else if r.pending.is_empty() { "Routes and kill switches as set.".into() }
                           else { format!("Applying {} change(s)...", r.pending.len()) }.into());
        }
        Err(_) => {
            d.set_net_on(false);
            d.set_net_state(2);
            d.set_net_summary("Not watched".into());
            d.set_net_sub("The Keel service is not running.".into());
            d.set_net_lines(lines(Vec::new()));
            d.set_net_warn(true);
            d.set_net_note("Routes and kill switches are not watched.".into());
        }
    }
}

/// Headline and the line under it: where general internet goes now, and why.
pub(crate) fn net_headline(r: &keel_net::service::Reply) -> (String, String) {
    let Some(c) = &r.config else { return ("Unknown".into(), r.summary.clone()) };
    if !c.enabled { return ("Windows decides".into(), "Keel's routing is off.".into()); }
    use keel_net::model::Side;
    // The side that carries it now: the chosen one when connected, else the other one. Services
    // before 0.1.12 do not report link states: then the summary line says it.
    let now = match (r.lan_up, r.wifi_up) {
        (Some(lan), Some(wifi)) => match (c.internet, lan, wifi) {
            (Side::Lan, true, _) | (Side::Wifi, false, true) => Some(Some(c.internet)),
            (Side::Lan, false, true) => Some(Some(Side::Wifi)),
            (Side::Wifi, true, false) => Some(Some(Side::Lan)),
            _ => Some(None),
        },
        _ => None,
    }.unwrap_or_else(|| {
        if r.summary.starts_with("internet via Wi-Fi") { Some(Side::Wifi) } else if r.summary.starts_with("internet via LAN") { Some(Side::Lan) } else { None }
    });
    match now {
        None => ("No internet".into(), "Neither LAN nor Wi-Fi is connected.".into()),
        Some(n) if n == c.internet => (format!("Internet over {n}"), "as you chose".into()),
        Some(n) => (format!("Internet over {n}"), format!("{} is disconnected; using {n}", c.internet)),
    }
}

/// Status lights: one per feature, plus the service itself when it does not answer.
pub fn lights(ui: &MainWindow, r: &Option<keel_service::Reply>) {
    let v: Vec<Light> = match r {
        None => vec![Light { label: "Service".into(), state: 2, tip: "The Keel service is not running.".into() }],
        Some(r) => [("ram", "Memory"), ("egpu", "eGPU"), ("net", "Network")].iter().map(|(n, label)| {
            let f = r.features.iter().find(|f| f.name == *n);
            let (state, tip) = match f {
                None => (2, "not in this service".to_string()),
                Some(f) if !f.enabled => (2, "switched off in Settings".into()),
                Some(f) if f.running && f.restarts == 0 => (0, "running".into()),
                Some(f) if f.running => (1, format!("running after {} restart(s); last error: {}", f.restarts, f.last_error.as_deref().unwrap_or("-"))),
                Some(f) => (1, format!("restarting: {}", f.last_error.as_deref().unwrap_or("-"))),
            };
            Light { label: (*label).into(), state, tip: tip.into() }
        }).collect(),
    };
    ui.global::<HomeData>().set_lights(ModelRc::new(VecModel::from(v)));
}

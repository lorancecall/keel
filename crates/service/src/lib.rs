//! The Keel service (LocalSystem, one process): the memory, eGPU and network features, each on its
//! own supervised thread (`keel_core::supervise`), and the control pipe `\\.\pipe\keel` shared by
//! them. Which features run is in `%ProgramData%\keel\service.toml`; the main window switches them
//! through the pipe (feature "service").
pub mod svc;

use anyhow::Result;
use keel_core::supervise::{self, Feature};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const FEATURE: &str = "service";
pub const NAMES: [&str; 3] = ["ram", "egpu", "net"];

pub fn log(msg: impl AsRef<str>) { keel_core::log::service(format!("service: {}", msg.as_ref())); }
fn log_owned(msg: String) { keel_core::log::service(msg); }

/// `service.toml`: which features run. Missing fields default to on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Features { pub ram: bool, pub egpu: bool, pub net: bool }

impl Default for Features { fn default() -> Self { Self { ram: true, egpu: true, net: true } } }

impl Features {
    pub fn path() -> PathBuf { keel_core::paths::data_dir().join("service.toml") }
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }
    fn save(&self) -> Result<()> { std::fs::write(Self::path(), toml::to_string(self)?)?; Ok(()) }
    pub fn get(&self, name: &str) -> bool { match name { "ram" => self.ram, "egpu" => self.egpu, "net" => self.net, _ => false } }
    pub fn set(&mut self, name: &str, on: bool) {
        match name { "ram" => self.ram = on, "egpu" => self.egpu = on, "net" => self.net = on, _ => {} }
    }
}

/// Runs a feature's loop.
fn body(name: &'static str, ram_slot: keel_ram::pipe::Slot) -> impl Fn(&Arc<AtomicBool>) -> Result<()> + Send + 'static {
    move |stop: &Arc<AtomicBool>| {
        match name {
            "ram" => {
                let slot = ram_slot.clone();
                let done = Arc::new(AtomicBool::new(false));
                let r = keel_ram::daemon::run(|tx| {
                    *slot.lock().unwrap() = Some(tx.clone());
                    // The daemon loop listens on a channel; turn the stop flag into its Stop message.
                    let (flag, done) = (stop.clone(), done.clone());
                    std::thread::spawn(move || {
                        while !flag.load(Ordering::SeqCst) && !done.load(Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(300)); }
                        let _ = tx.send(keel_ram::daemon::Msg::Stop);
                    });
                }, false);
                done.store(true, Ordering::SeqCst);
                *ram_slot.lock().unwrap() = None;
                r
            }
            "egpu" => keel_egpu::guard::run(|| stop.load(Ordering::SeqCst)),
            "net" => keel_net::service::watch(stop),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureStatus {
    pub name: String,
    pub enabled: bool,
    pub running: bool,
    pub restarts: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Status,
    SetFeature { name: String, on: bool },
    /// Admin jobs the tray asks for instead of a UAC prompt. Only these fixed jobs, with no paths
    /// or IDs from the caller: the pipe is open to every signed-in user.
    LegionFix { on: bool },
    /// Block / allow Windows Update for the AMD eGPU driver of the config (`gpus[].hardware_id`).
    DriverBlock { block: bool },
    /// Restart this service (a short-lived `keel service-restart` child does it).
    Restart,
    /// Power limit (W) and graphics clock ceiling (MHz) of the attached NVIDIA eGPU's profile;
    /// None = the driver's default. Saved in egpu.toml, applied by re-attaching (eGPU feature restart).
    EgpuLimits { power_limit_w: Option<u32>, clock_max_mhz: Option<u32>, #[serde(default)] mem_clock_max_mhz: Option<u32> },
    /// What counts as a game (Games page): folders in a program's path and program file names.
    EgpuGames { paths: Vec<String>, exes: Vec<String> },
    /// One NVIDIA 3D setting on the driver's global profile (`keel_egpu::nvdrs::SETTINGS` only);
    /// `None` puts the driver's default back.
    NvSetting { id: u32, value: Option<u32> },
    /// The Start menu and desktop shortcuts in the app icon's colour (`#rrggbb`; None: the
    /// built-in icon) and style (0 filled, 1 boat only).
    AppIcon { color: Option<String>, style: u8 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Reply {
    pub ok: bool,
    pub message: String,
    pub version: String,
    pub pid: u32,
    pub features: Vec<FeatureStatus>,
}

struct Manager {
    running: Mutex<HashMap<&'static str, Feature>>,
    ram_slot: keel_ram::pipe::Slot,
}

impl Manager {
    fn start(&self, name: &'static str) {
        let mut m = self.running.lock().unwrap();
        if m.contains_key(name) { return; }
        m.insert(name, Feature::start(name, log_owned, supervise::backoff, body(name, self.ram_slot.clone())));
        log(format!("{name}: on"));
    }

    fn stop(&self, name: &str) {
        let f = self.running.lock().unwrap().remove(name);
        if let Some(f) = f { f.stop(); log(format!("{name}: off")); }
    }

    fn status(&self, ok: bool, message: impl Into<String>) -> Reply {
        let cfg = Features::load();
        let m = self.running.lock().unwrap();
        let features = NAMES.iter().map(|&n| {
            let f = m.get(n);
            FeatureStatus {
                name: n.into(),
                enabled: cfg.get(n),
                running: f.is_some_and(|f| f.health.running.load(Ordering::SeqCst)),
                restarts: f.map_or(0, |f| f.health.restarts.load(Ordering::SeqCst)),
                last_error: f.and_then(|f| f.health.last_error.lock().unwrap().clone()),
            }
        }).collect();
        Reply { ok, message: message.into(), version: env!("CARGO_PKG_VERSION").into(), pid: std::process::id(), features }
    }

    fn answer(&self, req: Request) -> Reply {
        match req {
            Request::Status => self.status(true, "ok"),
            Request::SetFeature { name, on } => {
                let Some(&n) = NAMES.iter().find(|&&n| n == name) else { return self.status(false, format!("no feature {name}")) };
                let mut cfg = Features::load();
                cfg.set(n, on);
                if let Err(e) = cfg.save() { return self.status(false, format!("cannot save {}: {e:#}", Features::path().display())); }
                if on { self.start(n) } else { self.stop(n) }
                self.status(true, format!("{n} {}", if on { "on" } else { "off" }))
            }
            Request::LegionFix { on } => match legion_fix(on) {
                Ok(m) => { log(format!("legion-fix: {m}")); self.status(true, m) }
                Err(e) => self.status(false, format!("legion-fix: {e:#}")),
            },
            Request::DriverBlock { block } => {
                let cfg = keel_egpu::config::Config::load();
                let Some(id) = cfg.gpus.iter().find(|g| g.vendor_id == keel_egpu::gpu::VENDOR_AMD && !g.hardware_id.is_empty())
                    .map(|g| g.hardware_id.clone()) else { return self.status(false, "no AMD eGPU hardware_id in the config") };
                let r = if block { keel_egpu::driver::block(&id) } else { keel_egpu::driver::unblock(&id) };
                match r {
                    Ok(()) => { let m = format!("Windows Update {} for {id}", if block { "blocked" } else { "allowed" }); log(&m); self.status(true, m) }
                    Err(e) => self.status(false, format!("driver block: {e:#}")),
                }
            }
            Request::EgpuLimits { power_limit_w, clock_max_mhz, mem_clock_max_mhz } => {
                match egpu_limits(power_limit_w, clock_max_mhz, mem_clock_max_mhz) {
                    Ok(m) => {
                        log(format!("egpu limits: {m}"));
                        // Re-attach so the guard applies them now (and on every later attach).
                        if Features::load().get("egpu") { self.stop("egpu"); self.start("egpu"); }
                        self.status(true, m)
                    }
                    Err(e) => self.status(false, format!("egpu limits: {e:#}")),
                }
            }
            Request::EgpuGames { paths, exes } => {
                match egpu_games(paths, exes) {
                    Ok(m) => {
                        log(format!("egpu games: {m}"));
                        if Features::load().get("egpu") { self.stop("egpu"); self.start("egpu"); }
                        self.status(true, m)
                    }
                    Err(e) => self.status(false, format!("games: {e:#}")),
                }
            }
            Request::NvSetting { id, value } => {
                match keel_egpu::nvdrs::set(id, value) {
                    Ok(()) => {
                        // NVIDIA's display container must read the settings again (else the
                        // Control Panel closes right after opening).
                        let restarted = keel_egpu::nvsvc::restart_display_container();
                        let m = format!("NVIDIA setting {id:#x} = {}{}", value.map_or("driver default".into(), |v| v.to_string()),
                            match restarted { Ok(true) => "; NVIDIA display service restarted".to_string(), Ok(false) => String::new(), Err(e) => format!("; restarting the NVIDIA display service failed: {e:#}") });
                        log(&m);
                        self.status(true, m)
                    }
                    Err(e) => self.status(false, format!("NVIDIA setting: {e:#}")),
                }
            }
            Request::AppIcon { color, style } => {
                match app_icon(color.as_deref(), style) {
                    Ok(m) => { log(&m); self.status(true, m) }
                    Err(e) => self.status(false, format!("app icon: {e:#}")),
                }
            }
            Request::Restart => {
                let exe = svc::install_dir().join("keel.exe");
                match keel_core::cmd::hidden(&exe).arg("service-restart").spawn() {
                    Ok(_) => { log("restart requested"); self.status(true, "restarting") }
                    Err(e) => self.status(false, format!("restart: {e}")),
                }
            }
        }
    }
}

/// Writes the limits into the attached NVIDIA eGPU's profile. Values outside sane bounds are
/// refused here; NVML clamps the power limit to what the board allows when it is applied.
fn egpu_limits(power: Option<u32>, clock: Option<u32>, mem: Option<u32>) -> Result<String> {
    use keel_egpu::{config::Config, gpu};
    if let Some(w) = power { anyhow::ensure!((30..=600).contains(&w), "power limit {w} W is out of range"); }
    if let Some(c) = clock { anyhow::ensure!((300..=4000).contains(&c), "clock ceiling {c} MHz is out of range"); }
    if let Some(c) = mem { anyhow::ensure!((300..=20000).contains(&c), "memory clock ceiling {c} MHz is out of range"); }
    let mut cfg = Config::load_or_create()?;
    let adapters = gpu::adapters()?;
    let a = adapters.iter().find(|a| a.vendor_id == gpu::VENDOR_NVIDIA && cfg.profile_for(a.vendor_id, a.device_id).is_some())
        .ok_or_else(|| anyhow::anyhow!("no NVIDIA eGPU with a profile is attached"))?;
    let p = cfg.gpus.iter_mut().find(|g| g.vendor_id == a.vendor_id && g.device_id == a.device_id)
        .ok_or_else(|| anyhow::anyhow!("no profile for {}", a.name))?;
    p.power_limit_w = power;
    p.clock_max_mhz = clock;
    p.mem_clock_max_mhz = mem;
    let mut m = format!("{}: power limit {}, clock ceiling {}", p.name,
        power.map_or("driver default".into(), |w| format!("{w} W")), clock.map_or("none".into(), |c| format!("{c} MHz")));
    if let Some(c) = mem { m += &format!(", memory clock ceiling {c} MHz"); }
    cfg.save()?;
    Ok(m)
}

/// Pure check of the Games page's lists: folders are plain text matched inside program paths,
/// programs are bare file names ending in .exe; no control characters, sane lengths and counts.
pub fn check_games(paths: &[String], exes: &[String]) -> Result<()> {
    anyhow::ensure!(paths.len() <= 64 && exes.len() <= 128, "too many entries");
    let plain = |s: &str| !s.trim().is_empty() && s.len() <= 260 && !s.chars().any(char::is_control);
    for p in paths { anyhow::ensure!(plain(p), "bad folder entry {p:?}"); }
    for e in exes {
        anyhow::ensure!(plain(e) && e.to_lowercase().ends_with(".exe") && !e.contains(['\\', '/', ':']), "bad program name {e:?} (a file name such as game.exe)");
    }
    Ok(())
}

fn egpu_games(paths: Vec<String>, exes: Vec<String>) -> Result<String> {
    check_games(&paths, &exes)?;
    let mut cfg = keel_egpu::config::Config::load_or_create()?;
    cfg.game_paths = paths.iter().map(|p| p.trim().to_string()).collect();
    cfg.game_exes = exes.iter().map(|e| e.trim().to_string()).collect();
    cfg.save()?;
    Ok(format!("{} folder(s), {} program(s)", cfg.game_paths.len(), cfg.game_exes.len()))
}

/// Writes `keel-icon.ico` beside the program (admin-only folder) and points the all-users Keel
/// shortcuts (Start menu, public desktop) at it, or back at the program's own icon.
fn app_icon(color: Option<&str>, style: u8) -> Result<String> {
    use keel_core::appicon;
    use std::path::PathBuf;
    let dir = svc::install_dir();
    let gui = dir.join("keel-gui.exe");
    let ico = dir.join("keel-icon.ico");
    let icon = match color {
        Some(c) => {
            let rgb = appicon::parse_hex(c).ok_or_else(|| anyhow::anyhow!("colour {c:?} is not #rrggbb"))?;
            std::fs::write(&ico, appicon::ico_bytes(appicon::Style::from_index(style), rgb))?;
            Some(ico.as_path())
        }
        None => None,
    };
    let pd = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let public = std::env::var_os("PUBLIC").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"));
    let dirs = [pd.join("Microsoft").join("Windows").join("Start Menu").join("Programs").join("Keel"), public.join("Desktop")];
    let done = appicon::retarget_shortcuts(&dirs, &gui, icon)?;
    if icon.is_none() { let _ = std::fs::remove_file(&ico); }
    Ok(format!("app icon {}: {} shortcut(s)", color.unwrap_or("built-in"), done.len()))
}

/// `keel egpu legion-fix on|off` done by the service: copy the iGPU's ADL into Legion Space's
/// folders (from the iGPU driver folder named in the registry only) or remove those copies.
fn legion_fix(on: bool) -> Result<String> {
    use keel_egpu::{config::Config, legion};
    let mut cfg = Config::load_or_create()?;
    if on {
        let dir = legion::igpu_dir(&cfg).ok_or_else(|| anyhow::anyhow!("iGPU driver folder not found"))?;
        let n = legion::sync_adl(&dir)?.len();
        cfg.legion_space_fix = true;
        cfg.save()?;
        Ok(format!("on ({n} Legion Space folder(s) updated)"))
    } else {
        cfg.legion_space_fix = false;
        cfg.save()?;
        Ok(format!("off ({} copies removed)", legion::remove_adl().len()))
    }
}

/// The service's work until `stop` is set (also `keel daemon` in an admin console).
pub fn run(stop: &AtomicBool) -> Result<()> {
    keel_core::paths::create_data_dir()?;
    log(format!("started {} (pid {})", env!("CARGO_PKG_VERSION"), std::process::id()));
    let mgr = Arc::new(Manager { running: Mutex::new(HashMap::new()), ram_slot: Arc::new(Mutex::new(None)) });
    let mut handlers: HashMap<String, keel_core::pipe::Handler> = HashMap::new();
    handlers.insert(keel_ram::pipe::FEATURE.into(), keel_ram::pipe::handler(mgr.ram_slot.clone()));
    handlers.insert(keel_net::service::FEATURE.into(), keel_net::service::handler());
    let m = mgr.clone();
    handlers.insert(FEATURE.into(), Arc::new(move |body: Value| {
        let r = match serde_json::from_value::<Request>(body) {
            Ok(req) => m.answer(req),
            Err(e) => m.status(false, format!("bad request: {e}")),
        };
        serde_json::to_value(r).unwrap_or(Value::Null)
    }));
    keel_core::pipe::serve(handlers, log_owned);
    let cfg = Features::load();
    for n in NAMES { if cfg.get(n) { mgr.start(n) } else { log(format!("{n}: off in service.toml")) } }
    keel_core::trim::working_set();
    let mut ticks = 0u32;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(500));
        ticks += 1;
        if ticks % 1200 == 0 { keel_core::trim::working_set(); } // every 10 minutes
    }
    for n in NAMES { mgr.stop(n); }
    log("stopped");
    Ok(())
}

/// Client side: one request to the service.
pub fn call(req: &Request) -> Result<Reply> {
    Ok(serde_json::from_value(keel_core::pipe::call(FEATURE, &serde_json::to_value(req)?)?)?)
}

/// Puts back everything the features changed (service uninstall): on-demand families on again,
/// NVIDIA services started, network metrics / policy restored and Keel's routes and rules removed.
pub fn restore_all() -> Vec<String> {
    let mut out = keel_ram::daemon::restore_all();
    keel_egpu::nvsvc::restore_all();
    if keel_net::state_path().exists() {
        match keel_net::undo() { Ok(l) => out.extend(l), Err(e) => out.push(format!("network: {e:#}")) }
    }
    for l in &out { log(l); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_lists_are_checked() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(check_games(&s(&[r"\steamapps\common\", r"D:\Games\"]), &s(&["eldenring.exe"])).is_ok());
        assert!(check_games(&s(&[]), &s(&[r"C:\x\game.exe"])).is_err(), "a path, not a file name");
        assert!(check_games(&s(&[]), &s(&["game.bat"])).is_err());
        assert!(check_games(&s(&["a\nb"]), &s(&[])).is_err());
    }

    #[test]
    fn features_default_on_and_keep_unknown_fields_out() {
        let f: Features = toml::from_str("net = false").unwrap();
        assert_eq!(f, Features { ram: true, egpu: true, net: false });
        let mut g = Features::default();
        g.set("egpu", false);
        assert!(!g.get("egpu") && g.get("ram") && !g.get("nope"));
    }
}

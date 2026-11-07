//! NVIDIA user-space services (opt-in `nvidia_services_follow_egpu`): stopped while no NVIDIA GPU
//! is present, started again when one appears. Only stop/start; start types stay as they are, so
//! a reboot brings them back on its own. Names we stopped are written to `nvidia-services.json`
//! *before* the stop, so a crash, a restart with the option off, or an uninstall starts them again.
use crate::gpu::{Adapter, VENDOR_NVIDIA};
use crate::util::{data_dir, log};
use windows_service::service::{ServiceAccess, ServiceStartType, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// The only services this module may touch, whatever config.toml lists. All are NVIDIA
/// user-space containers / SDK services; kernel drivers are never in here.
pub const ALLOWED: &[&str] = &[
    "NvContainerLocalSystem", "NVDisplay.ContainerLocalSystem", "FvSvc",
    "NvContainerNetworkService", "NvTelemetryContainer", "NvBroadcast.ContainerLocalSystem",
];

/// Default `nvidia_services`: everything that runs with only AMD GPUs present (measured). NvContainerLocalSystem (NVIDIA App, ~40-60 MB with its children) and
/// NVDisplay.ContainerLocalSystem (the driver package's Display Container, ~17 MB with its child;
/// a user-space service, the driver itself is not touched).
pub const DEFAULT_SERVICES: &[&str] = &["NvContainerLocalSystem", "NVDisplay.ContainerLocalSystem"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action { None, Stop, Start }

/// Is any NVIDIA GPU present? `None` when the adapter list is unusable (DXGI failed or listed
/// nothing; the iGPU is always there), so a read failure never stops anything.
pub fn nvidia_present(adapters: Option<&[Adapter]>) -> Option<bool> {
    adapters.filter(|a| !a.is_empty()).map(|a| a.iter().any(|x| x.vendor_id == VENDOR_NVIDIA))
}

/// How long "no NVIDIA GPU" must hold before the services are stopped: after boot (and after a
/// replug) the eGPU shows up tens of seconds after the service starts, while Windows is still
/// starting the NVIDIA services.
pub const ABSENT_GRACE: std::time::Duration = std::time::Duration::from_secs(120);

/// `present` as the services should see it: an absence counts only once it has lasted `grace`
/// (pure; `absent_since` is kept by the caller).
pub fn settled(present: Option<bool>, absent_since: &mut Option<std::time::Instant>, now: std::time::Instant, grace: std::time::Duration) -> Option<bool> {
    match present {
        Some(false) => {
            let since = *absent_since.get_or_insert(now);
            (now.duration_since(since) >= grace).then_some(false)
        }
        Some(true) => { *absent_since = None; Some(true) }
        None => None,
    }
}

/// What to do with one service. We only start what we stopped ourselves; a service the user
/// stopped or disabled stays as it is.
pub fn decide(enabled: bool, present: Option<bool>, running: bool, stopped_by_us: bool) -> Action {
    match (enabled, present) {
        (_, None) => Action::None,
        (true, Some(false)) if running => Action::Stop,
        (true, Some(true)) | (false, _) if !running && stopped_by_us => Action::Start,
        _ => Action::None,
    }
}

/// Config names filtered to the allow-list (case-insensitive, deduplicated), plus the rejected ones.
pub fn allowed(names: &[String]) -> (Vec<&'static str>, Vec<String>) {
    let (mut ok, mut bad): (Vec<&'static str>, Vec<String>) = (Vec::new(), Vec::new());
    for n in names {
        match ALLOWED.iter().find(|a| a.eq_ignore_ascii_case(n.trim())) {
            Some(a) => if !ok.contains(a) { ok.push(a) },
            None => bad.push(n.clone()),
        }
    }
    (ok, bad)
}

fn state_path() -> std::path::PathBuf { data_dir().join("nvidia-services.json") }

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct State { stopped: Vec<String> }

fn load() -> State {
    std::fs::read_to_string(state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(s: &State) -> std::io::Result<()> {
    if s.stopped.is_empty() {
        match std::fs::remove_file(state_path()) { Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e), _ => Ok(()) }
    } else {
        std::fs::write(state_path(), serde_json::to_string_pretty(s).unwrap_or_default())
    }
}

/// Running for our purpose: anything but stopped / stopping.
fn query(mgr: &ServiceManager, name: &str) -> Option<(bool, ServiceStartType)> {
    let svc = mgr.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG).ok()?;
    let st = svc.query_status().ok()?.current_state;
    let cfg = svc.query_config().ok()?;
    Some((!matches!(st, ServiceState::Stopped | ServiceState::StopPending), cfg.start_type))
}

fn control(mgr: &ServiceManager, name: &str, start: bool) -> anyhow::Result<()> {
    let svc = mgr.open_service(name, ServiceAccess::STOP | ServiceAccess::START)?;
    if start { svc.start::<&str>(&[])?; } else { svc.stop()?; }
    Ok(())
}

/// Restarts NVIDIA's display container (service only) when it runs. After the driver's 3D settings
/// were written it kept the old settings and the NVIDIA Control Panel closed right after opening
/// until this service was restarted.
pub fn restart_display_container() -> anyhow::Result<bool> {
    const NAME: &str = "NVDisplay.ContainerLocalSystem";
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    if !query(&mgr, NAME).is_some_and(|(running, _)| running) { return Ok(false); }
    control(&mgr, NAME, false)?;
    for _ in 0..50 {
        if query(&mgr, NAME).is_some_and(|(running, _)| !running) { break; }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    control(&mgr, NAME, true)?;
    Ok(true)
}

/// One pass over the configured services. Cheap (a few SCM queries); the guard calls it when the
/// NVIDIA presence changes and once a minute. Returns the names currently stopped by us.
pub fn sync(enabled: bool, names: &[String], present: Option<bool>) -> Vec<String> {
    let mut state = load();
    let list: Vec<String> = if enabled { allowed(names).0.into_iter().map(String::from).collect() } else { Vec::new() };
    if list.is_empty() && state.stopped.is_empty() { return Vec::new(); }
    let Ok(mgr) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) else { return state.stopped; };
    // Services we stopped stay under our care even after they leave the list (then: start them).
    let mut todo = list.clone();
    for s in &state.stopped { if !todo.iter().any(|t| t.eq_ignore_ascii_case(s)) { todo.push(s.clone()); } }
    for name in &todo {
        let is = |s: &String| s.eq_ignore_ascii_case(name);
        let mine = state.stopped.iter().any(is);
        let Some((running, start_type)) = query(&mgr, name) else {
            state.stopped.retain(|s| !is(s)); // service gone
            continue;
        };
        if running && mine { state.stopped.retain(|s| !is(s)); } // someone else started it
        match decide(list.iter().any(is), present, running, mine) {
            Action::Stop if start_type != ServiceStartType::Disabled => {
                state.stopped.push(name.clone());
                if let Err(e) = save(&state) {
                    log(format!("nvidia-services: cannot record {name} before stopping it ({e}); left running"));
                    state.stopped.pop();
                    continue;
                }
                match control(&mgr, name, false) {
                    Ok(()) => log(format!("nvidia-services: no NVIDIA GPU present - stopped {name} (start type {start_type:?} kept)")),
                    Err(e) => { log(format!("nvidia-services: stopping {name} failed: {e}")); state.stopped.pop(); }
                }
            }
            Action::Start => match control(&mgr, name, true) {
                Ok(()) => {
                    log(format!("nvidia-services: started {name} ({})", if list.iter().any(is) { "NVIDIA GPU present" } else { "option off" }));
                    state.stopped.retain(|s| !is(s));
                }
                Err(e) => log(format!("nvidia-services: starting {name} failed: {e}")),
            },
            _ => {}
        }
    }
    if let Err(e) = save(&state) { log(format!("nvidia-services: cannot write {}: {e}", state_path().display())); }
    state.stopped
}

/// Uninstall: start every service we stopped and forget them.
pub fn restore_all() {
    let before = load().stopped;
    if before.is_empty() { return; }
    let left = sync(false, &[], Some(true));
    if left.is_empty() { println!("NVIDIA services started again: {before:?}"); }
    else { println!("NVIDIA services still recorded as stopped (start them by hand): {left:?}"); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(vendor_id: u32) -> Adapter { Adapter { name: String::new(), vendor_id, device_id: 0, vram_mb: 0, luid: 0 } }

    #[test]
    fn presence() {
        assert_eq!(nvidia_present(None), None);
        assert_eq!(nvidia_present(Some(&[])), None);
        assert_eq!(nvidia_present(Some(&[a(0x1002)])), Some(false));
        assert_eq!(nvidia_present(Some(&[a(0x1002), a(0x10DE)])), Some(true));
    }

    #[test]
    fn absence_must_last_before_stopping() {
        let t0 = std::time::Instant::now();
        let g = std::time::Duration::from_secs(120);
        let mut since = None;
        assert_eq!(settled(Some(false), &mut since, t0, g), None, "just booted: wait");
        assert_eq!(settled(Some(false), &mut since, t0 + std::time::Duration::from_secs(60), g), None);
        assert_eq!(settled(Some(true), &mut since, t0 + std::time::Duration::from_secs(70), g), Some(true), "eGPU showed up");
        assert_eq!(since, None);
        assert_eq!(settled(Some(false), &mut since, t0 + std::time::Duration::from_secs(80), g), None, "the clock starts again");
        assert_eq!(settled(Some(false), &mut since, t0 + std::time::Duration::from_secs(200), g), Some(false));
        assert_eq!(settled(None, &mut since, t0, g), None);
    }

    #[test]
    fn decisions() {
        use Action::*;
        // No NVIDIA GPU: stop a running service, leave a stopped one.
        assert_eq!(decide(true, Some(false), true, false), Stop);
        assert_eq!(decide(true, Some(false), false, false), None);
        // NVIDIA GPU back: start only what we stopped.
        assert_eq!(decide(true, Some(true), false, true), Start);
        assert_eq!(decide(true, Some(true), false, false), None);
        assert_eq!(decide(true, Some(true), true, false), None);
        // Option off: give back what we stopped, touch nothing else.
        assert_eq!(decide(false, Some(false), false, true), Start);
        assert_eq!(decide(false, Some(false), true, false), None);
        assert_eq!(decide(false, Some(false), false, false), None);
        // Unknown adapters: never act.
        assert_eq!(decide(true, Option::None, true, false), None);
        assert_eq!(decide(false, Option::None, false, true), None);
    }

    #[test]
    fn defaults_are_allowed() {
        let d: Vec<String> = DEFAULT_SERVICES.iter().map(|s| s.to_string()).collect();
        assert_eq!(allowed(&d), (DEFAULT_SERVICES.to_vec(), vec![]));
        assert!(d.iter().any(|s| s == "NVDisplay.ContainerLocalSystem"));
        assert_eq!(crate::config::Config::default().nvidia_services, d);
    }

    #[test]
    fn allow_list() {
        let names = vec!["nvcontainerlocalsystem".to_string(), "NvContainerLocalSystem".into(), "Spooler".into(), " FvSvc ".into()];
        assert_eq!(allowed(&names), (vec!["NvContainerLocalSystem", "FvSvc"], vec!["Spooler".to_string()]));
    }
}

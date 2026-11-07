//! The network feature in the service: re-applies the config after every network change
//! (interface, address and route notifications from IP Helper, no polling; plus a 5-minute check
//! for adapters that appeared without a notification), and answers the tray / main window on the
//! control pipe (feature "net").
use crate::model::{Config, Side};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::*;
use windows::Win32::Networking::WinSock::AF_INET;

pub const FEATURE: &str = "net";
/// Notifications must stop this long before acting (a link coming up sends a burst).
const QUIET: Duration = Duration::from_secs(3);
/// At most one reconcile this often (our own changes notify too; the second pass is a no-op).
const MIN_GAP: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_secs(300);

static EVENTS: AtomicU64 = AtomicU64::new(0);
/// Serializes reconcile / undo / config writes between the watch loop and pipe requests.
static WORK: Mutex<()> = Mutex::new(());

unsafe extern "system" fn on_change(_: *const core::ffi::c_void, _: *const core::ffi::c_void, _: MIB_NOTIFICATION_TYPE) {
    EVENTS.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "system" fn on_iface(c: *const core::ffi::c_void, r: *const MIB_IPINTERFACE_ROW, t: MIB_NOTIFICATION_TYPE) { unsafe { on_change(c, r.cast(), t) } }
unsafe extern "system" fn on_route(c: *const core::ffi::c_void, r: *const MIB_IPFORWARD_ROW2, t: MIB_NOTIFICATION_TYPE) { unsafe { on_change(c, r.cast(), t) } }
unsafe extern "system" fn on_addr(c: *const core::ffi::c_void, r: *const MIB_UNICASTIPADDRESS_ROW, t: MIB_NOTIFICATION_TYPE) { unsafe { on_change(c, r.cast(), t) } }

struct Notifications(Vec<HANDLE>);

impl Notifications {
    fn register() -> Result<Self> {
        let mut v = Vec::new();
        unsafe {
            let mut h = HANDLE::default();
            NotifyIpInterfaceChange(AF_INET, Some(on_iface), None, false, &mut h).ok().context("NotifyIpInterfaceChange")?;
            v.push(h);
            let mut h = HANDLE::default();
            NotifyRouteChange2(AF_INET, Some(on_route), std::ptr::null(), false, &mut h).ok().context("NotifyRouteChange2")?;
            v.push(h);
            let mut h = HANDLE::default();
            NotifyUnicastIpAddressChange(AF_INET, Some(on_addr), None, false, &mut h).ok().context("NotifyUnicastIpAddressChange")?;
            v.push(h);
        }
        Ok(Self(v))
    }
}

impl Drop for Notifications {
    fn drop(&mut self) { for h in &self.0 { unsafe { let _ = CancelMibChangeNotify2(*h); } } }
}

/// One pass: brings the system to the config.
fn pass(why: &str) {
    let _w = WORK.lock().unwrap();
    match crate::reconcile() {
        Ok((0, 0)) => {}
        Ok((ok, bad)) => crate::log(format!("{why}: {ok} change(s), {bad} failed")),
        Err(e) => crate::log(format!("{why}: {e:#}")),
    }
}

/// The feature loop; returns once `stop` is set.
pub fn watch(stop: &AtomicBool) -> Result<()> {
    keel_core::paths::ensure_may_write_data_dir()?;
    let _n = Notifications::register()?;
    pass("start");
    let (mut seen, mut changed_at, mut last_pass) = (EVENTS.load(Ordering::SeqCst), None::<Instant>, Instant::now());
    let mut next_check = Instant::now() + CHECK_EVERY;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(500));
        let now_events = EVENTS.load(Ordering::SeqCst);
        if now_events != seen { seen = now_events; changed_at = Some(Instant::now()); }
        let settled = changed_at.is_some_and(|t| t.elapsed() >= QUIET) && last_pass.elapsed() >= MIN_GAP;
        if settled || Instant::now() >= next_check {
            pass(if settled { "network change" } else { "check" });
            changed_at = None;
            last_pass = Instant::now();
            next_check = Instant::now() + CHECK_EVERY;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ pipe

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// The current report (summary, sides, pending changes, problems) and the config.
    Status,
    /// Re-apply now.
    Apply,
    /// Change where general internet goes.
    SetInternet { side: Side },
    /// Replace the whole config (the main window's network tab).
    SetConfig { config: Config },
    /// Turn the feature's routing on or off (off restores metrics, removes routes, disables rules).
    SetEnabled { enabled: bool },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Reply {
    pub ok: bool,
    pub message: String,
    pub config: Option<Config>,
    pub summary: String,
    pub internet: Option<Side>,
    pub follow: Option<Side>,
    pub pending: Vec<String>,
    pub problems: Vec<String>,
    /// Whether a LAN / Wi-Fi adapter is up, and the adapter default-route traffic leaves by now.
    pub lan_up: Option<bool>,
    pub wifi_up: Option<bool>,
    pub egress: Option<String>,
}

fn status(ok: bool, message: impl Into<String>) -> Reply {
    let mut r = Reply { ok, message: message.into(), ..Default::default() };
    match crate::report() {
        Ok(rep) => {
            r.summary = crate::model::summary(&rep.cfg, &rep.snap, &rep.plan);
            r.internet = rep.plan.internet;
            r.follow = rep.plan.follow;
            r.lan_up = Some(rep.snap.lans(&rep.cfg).iter().any(|a| a.up));
            r.wifi_up = Some(rep.snap.wifi(&rep.cfg).iter().any(|a| a.up));
            r.egress = rep.snap.default_egress(&rep.cfg).map(|a| a.alias.clone());
            r.pending = rep.plan.actions.iter().map(|a| a.to_string()).collect();
            r.problems = rep.plan.problems.iter().cloned().chain(crate::contested()).collect();
            r.config = Some(rep.cfg);
        }
        Err(e) => r.summary = format!("{e:#}"),
    }
    r
}

/// Writes `net.toml` (service only; the folder is admin-only).
pub fn save_config(cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(keel_core::paths::data_dir())?;
    let p = crate::config_path();
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(cfg)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

fn change(f: impl FnOnce(&mut Config)) -> Result<String> {
    {
        let _w = WORK.lock().unwrap();
        let mut cfg = crate::load_config()?;
        f(&mut cfg);
        save_config(&cfg)?;
        crate::log(format!("config changed: internet {}, enabled {}", cfg.internet, cfg.enabled));
    }
    let _w = WORK.lock().unwrap();
    let (ok, bad) = crate::reconcile()?;
    Ok(format!("saved and applied: {ok} change(s){}", if bad > 0 { format!(", {bad} failed (see service.log)") } else { String::new() }))
}

fn answer(req: Request) -> Reply {
    let r: Result<String> = match req {
        Request::Status => Ok("ok".into()),
        Request::Apply => {
            let _w = WORK.lock().unwrap();
            crate::reconcile().map(|(ok, bad)| format!("{ok} change(s), {bad} failed"))
        }
        Request::SetInternet { side } => change(|c| c.internet = side),
        Request::SetConfig { config } => change(|c| *c = config),
        Request::SetEnabled { enabled } => change(|c| c.enabled = enabled),
    };
    match r { Ok(m) => status(true, m), Err(e) => status(false, format!("{e:#}")) }
}

pub fn handler() -> keel_core::pipe::Handler {
    Arc::new(|body: Value| {
        let r = match serde_json::from_value::<Request>(body) {
            Ok(req) => answer(req),
            Err(e) => Reply { message: format!("bad request: {e}"), ..Default::default() },
        };
        serde_json::to_value(r).unwrap_or(Value::Null)
    })
}

/// Client side (tray, main window).
pub fn call(req: &Request) -> Result<Reply> {
    Ok(serde_json::from_value(keel_core::pipe::call(FEATURE, &serde_json::to_value(req)?)?)?)
}

//! Keel's network feature: keep Wi-Fi up
//! beside the wired LAN, pick where general internet goes (Wi-Fi or LAN), pin each VPN's outer
//! connection to Wi-Fi or LAN with an optional fail-closed kill switch, and route chosen
//! destinations. See `model` for how it works.
//!
//! Files in `%ProgramData%\keel` (service / admin only): `net.toml` (config), `net-state.json`
//! (values recorded before the first change), `net-status.json`.
pub mod contest;
pub mod fw;
pub mod model;
pub mod service;
pub mod sys;

use anyhow::{Context, Result};
use model::{Action, Config, Plan, Snapshot, State};
use std::path::PathBuf;

pub fn config_path() -> PathBuf { keel_core::paths::data_dir().join("net.toml") }
pub fn state_path() -> PathBuf { keel_core::paths::data_dir().join("net-state.json") }
pub fn status_path() -> PathBuf { keel_core::paths::data_dir().join("net-status.json") }

fn log(msg: impl AsRef<str>) {
    let m = format!("net: {}", msg.as_ref());
    if keel_core::paths::may_write_data_dir() { keel_core::log::service(m) } else { keel_core::log::user(m) }
}

/// `net.toml`, or `Config::default` when there is none.
pub fn load_config() -> Result<Config> {
    match std::fs::read_to_string(config_path()) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("{}", config_path().display())),
        Err(_) => Ok(Config::default()),
    }
}

/// Writes the default `net.toml` if missing (admin), so the user has a file to edit.
pub fn write_default_config() -> Result<PathBuf> {
    keel_core::paths::ensure_may_write_data_dir()?;
    let p = config_path();
    if !p.exists() {
        std::fs::create_dir_all(keel_core::paths::data_dir())?;
        std::fs::write(&p, toml::to_string_pretty(&Config::default())?)?;
    }
    Ok(p)
}

pub fn load_state() -> State {
    std::fs::read_to_string(state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Changes another program keeps undoing (see `contest`); shared by the watch loop and the pipe.
pub static CONTEST: std::sync::LazyLock<std::sync::Mutex<contest::Contest>> = std::sync::LazyLock::new(Default::default);

/// Contested changes, for the status and the main window.
pub fn contested() -> Vec<String> { CONTEST.lock().unwrap().problems(std::time::Instant::now()) }

pub struct Report { pub cfg: Config, pub snap: Snapshot, pub state: State, pub plan: Plan }

/// Reads everything and plans (no admin needed).
pub fn report() -> Result<Report> {
    let cfg = load_config()?;
    let snap = sys::snapshot()?;
    let state = load_state();
    let plan = model::plan(&cfg, &snap, &state);
    Ok(Report { cfg, snap, state, plan })
}

/// Brings the system to the config (admin): records originals first, then runs each action;
/// a failed action is logged and the rest still run. Returns (done, failed).
pub fn reconcile() -> Result<(usize, usize)> {
    keel_core::paths::ensure_may_write_data_dir()?;
    let r = report()?;
    // Changes another program keeps undoing are held for a while (never the kill switch).
    let actions = CONTEST.lock().unwrap().filter(&r.plan.actions, std::time::Instant::now());
    if actions.is_empty() { write_status(&r); return Ok((0, 0)); }
    // Record before acting (crash-safe undo); routes go into the record before they exist.
    let rec = model::record(&r.state, &r.snap, &actions, sys::policy().1);
    std::fs::create_dir_all(keel_core::paths::data_dir())?;
    save_state(&rec).context("cannot record the original values; nothing changed")?;
    let (mut ok, mut bad) = (0, 0);
    let mut kept = rec.clone();
    for a in &actions {
        match sys::apply(a) {
            Ok(()) => {
                ok += 1;
                log(format!("{a}"));
                if let Some(m) = CONTEST.lock().unwrap().applied(a, std::time::Instant::now()) { log(m); }
            }
            Err(e) => { bad += 1; log(format!("FAILED {a}: {e:#}")); }
        }
        if let Action::RemoveRoute(x) = a { kept.routes.retain(|o| o != x && !(o.prefix == x.prefix && o.len == x.len && o.index == x.index)); }
    }
    save_state(&kept)?;
    write_status(&report()?);
    Ok((ok, bad))
}

/// Puts everything back as recorded and removes Keel's rules and routes (admin).
pub fn undo() -> Result<Vec<String>> {
    keel_core::paths::ensure_may_write_data_dir()?;
    let mut done = Vec::new();
    let r = report()?;
    let off = model::plan(&Config { enabled: false, keep_wifi_with_lan: false, ..r.cfg.clone() }, &r.snap, &r.state);
    for a in off.actions.iter().filter(|a| !matches!(a, Action::DisableRule { .. })) {
        match sys::apply(a) { Ok(()) => done.push(a.to_string()), Err(e) => done.push(format!("FAILED {a}: {e:#}")) }
    }
    done.push(format!("removed {} firewall rule(s)", fw::remove_all()?));
    if r.state.policy_recorded {
        sys::restore_policy(r.state.policy_key_existed, r.state.policy_old)?;
        done.push(format!("restored {} ({:?})", sys::POLICY_NAME, r.state.policy_old));
    }
    let _ = std::fs::remove_file(state_path());
    let _ = std::fs::remove_file(status_path());
    for d in &done { log(d); }
    Ok(done)
}

fn save_state(s: &State) -> Result<()> {
    let tmp = state_path().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    std::fs::rename(&tmp, state_path())?;
    Ok(())
}

fn write_status(r: &Report) {
    let st = serde_json::json!({
        "time": chrono::Local::now().to_rfc3339(),
        "enabled": r.cfg.enabled,
        "summary": model::summary(&r.cfg, &r.snap, &r.plan),
        "egress": r.snap.default_egress(&r.cfg).map(|a| a.alias.clone()),
        "pending": r.plan.actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "problems": r.plan.problems.iter().cloned().chain(contested()).collect::<Vec<_>>(),
    });
    let _ = std::fs::write(status_path(), serde_json::to_string_pretty(&st).unwrap_or_default());
}

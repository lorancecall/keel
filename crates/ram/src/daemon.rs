//! The memory feature's service work (LocalSystem): on-demand families. State
//! and log live in `%ProgramData%\keel`; the state (with every original value) is written *before* any
//! change, so `service-uninstall` / `ondemand <family> normal` can always put things back.
use crate::ondemand::{self, Entry, Follower, Mode, Reply, Request, Seen, State, Switch};
use crate::{follow, pipe, procs, switch};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const DRIFT_EVERY: Duration = Duration::from_secs(300);
/// An app that restarts (update, crash) within this time does not switch its family off and on.
const OFF_GRACE: Duration = Duration::from_secs(20);

pub enum Msg {
    Req(Request, Sender<Reply>),
    Proc(follow::ProcEvent),
    Stop,
}

/// `%ProgramData%\keel` (created by the service, so users can read but not edit it).
/// `KEEL_DATA` overrides it for tests run without the service.
pub fn dir() -> PathBuf { keel_core::paths::data_dir() }
pub fn state_path() -> PathBuf { dir().join("ondemand.json") }

/// A line in the service log, tagged with this feature.
pub fn log(msg: impl AsRef<str>) { keel_core::log::service(format!("ram: {}", msg.as_ref())); }

pub fn load() -> State { load_from(&state_path()).unwrap_or_default() }

/// An on-demand state file.
pub fn load_from(p: &std::path::Path) -> Option<State> {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok())
}

fn save(s: &State) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir())?;
    let tmp = state_path().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    std::fs::rename(tmp, state_path())?;
    Ok(())
}

/// Switches everything of an entry off. Returns the problems (the rest is still done).
fn apply_off(e: &mut Entry) -> Vec<String> {
    let mut errs = Vec::new();
    for s in &e.services {
        if s.start != "disabled" { if let Err(x) = switch::set_service_start(&s.name, "disabled", false) { errs.push(format!("{x:#}")); } }
    }
    // Stop in passes: a service another one depends on only stops after its dependents.
    let names: Vec<String> = e.services.iter().map(|s| s.name.clone()).collect();
    errs.extend(switch::stop_all(&names, Duration::from_secs(15)));
    for t in e.tasks.iter().filter(|t| t.enabled) {
        if let Err(x) = switch::set_task(&t.path, false) { errs.push(format!("{x:#}")); }
    }
    for s in &e.startup {
        if let Err(x) = switch::startup_set(&s.key, &s.value, Some(&switch::STARTUP_OFF)) { errs.push(format!("{x:#}")); }
    }
    errs.extend(close_leftovers(e));
    e.on = false;
    errs
}

/// Puts everything of an entry back as it was; starts services that were automatic.
fn apply_on(e: &mut Entry) -> Vec<String> {
    let mut errs = Vec::new();
    for s in &e.services {
        if let Err(x) = switch::set_service_start(&s.name, &s.start, s.delayed) { errs.push(format!("{x:#}")); }
        if s.start == "auto" { if let Err(x) = switch::start_service(&s.name) { errs.push(format!("{x:#}")); } }
    }
    for t in e.tasks.iter().filter(|t| t.enabled) {
        if let Err(x) = switch::set_task(&t.path, true) { errs.push(format!("{x:#}")); }
    }
    for s in &e.startup {
        if let Err(x) = switch::startup_set(&s.key, &s.value, s.bytes.as_deref()) { errs.push(format!("{x:#}")); }
    }
    e.on = true;
    errs
}

/// Builds a new entry from the family as LocalSystem sees it, with the original values now.
fn new_entry(family: &str, key: &str, mode: Mode, apps: Vec<String>) -> Result<Entry, String> {
    let report = crate::scan::run(0, crate::scan::SortBy::Private, false).map_err(|e| format!("{e:#}"))?;
    make_entry(&report, family, key, mode, apps, false)
}

/// `procs_only`: a family with only processes (no services, tasks or startup items) is accepted
/// when it has vendor folders (AI mode closes its processes).
fn make_entry(report: &crate::scan::Report, family: &str, key: &str, mode: Mode, apps: Vec<String>, procs_only: bool) -> Result<Entry, String> {
    let f = report.families.iter().find(|f| f.key == key).or_else(|| report.families.iter().find(|f| f.name.eq_ignore_ascii_case(family)))
        .ok_or_else(|| format!("no family {family} ({key}) seen by the service"))?;
    let p = ondemand::parts(f)?;
    if p.services.is_empty() && p.tasks.is_empty() && p.startup.is_empty() && !(procs_only && !ondemand::family_dirs(f).is_empty()) {
        return Err(format!("{} has no services, tasks or machine-wide startup items to switch", f.name));
    }
    let services = p.services.iter().map(|(n, _)| {
        let (start, delayed) = switch::service_start(n).unwrap_or(("manual".into(), false));
        ondemand::Svc { name: n.clone(), start, delayed }
    }).collect();
    let tasks = p.tasks.iter().map(|(path, en)| ondemand::Task { path: path.clone(), enabled: switch::task_enabled(path).unwrap_or(*en) }).collect();
    let startup = p.startup.iter().map(|(k, v)| ondemand::Startup { key: k.clone(), value: v.clone(), bytes: switch::startup_get(k, v) }).collect();
    Ok(Entry { family: f.name.clone(), key: f.key.clone(), mode, apps, services, tasks, startup, on: true,
        since: chrono::Local::now().to_rfc3339(), dirs: ondemand::family_dirs(f), ai_off: false, ai_only: false })
}

/// Entries saved before `dirs` existed get their vendor folders from one fresh scan.
fn fill_dirs(st: &mut State) {
    if st.entries.iter().all(|e| !e.dirs.is_empty()) { return; }
    let Ok(report) = crate::scan::run(0, crate::scan::SortBy::Private, false) else { return };
    for e in st.entries.iter_mut().filter(|e| e.dirs.is_empty()) {
        if let Some(f) = report.families.iter().find(|f| f.key == e.key).or_else(|| report.families.iter().find(|f| f.name == e.family)) {
            e.dirs = ondemand::family_dirs(f);
        }
    }
}

/// Force-closes the family's processes still running from its vendor folders (the service runs in
/// session 0 and cannot ask a user-session process to close; the services were stopped first).
fn close_leftovers(e: &Entry) -> Vec<String> {
    if e.dirs.is_empty() { return vec![]; }
    let Ok(ps) = procs::snapshot() else { return vec![] };
    let devmap = procs::device_map();
    // Service processes are the SCM's to stop (and some refuse termination): never force them.
    let (svc_pids, _) = crate::sources::scm_active();
    let list: Vec<(u32, String, Option<String>)> = ps.iter().filter(|p| !svc_pids.contains_key(&p.pid))
        .map(|p| (p.pid, p.name.clone(), procs::image_path(p.pid, &devmap).0)).collect();
    let mut errs = Vec::new();
    for pid in ondemand::leftovers(&e.dirs, &list, std::process::id()) {
        let name = list.iter().find(|x| x.0 == pid).map(|x| x.1.clone()).unwrap_or_default();
        match switch::terminate(pid) {
            Ok(()) => log(format!("{}: closed leftover {name} ({pid})", e.family)),
            Err(x) => errs.push(format!("close {name} ({pid}): {x:#}")),
        }
    }
    errs
}

fn exe_name(pid: u32, devmap: &[(String, String)]) -> Option<String> {
    let (p, _) = procs::image_path(pid, devmap);
    Some(crate::paths::file_name(&p?.to_lowercase()).to_string())
}

/// Observed start types / task states of an entry's parts.
fn observe(e: &Entry) -> Vec<Seen> {
    let mut v: Vec<Seen> = e.services.iter().filter_map(|s| switch::service_start(&s.name).map(|(start, _)| Seen::Service { name: s.name.clone(), start })).collect();
    v.extend(e.tasks.iter().filter(|t| t.enabled).filter_map(|t| switch::task_enabled(&t.path).map(|enabled| Seen::Task { path: t.path.clone(), enabled })));
    v
}

struct Daemon {
    st: State,
    follower: Follower,
    pending_off: HashMap<String, Instant>,
    devmap: Vec<(String, String)>,
}

impl Daemon {
    fn reply(&self, ok: bool, message: impl Into<String>) -> Reply { Reply { ok, message: message.into(), state: self.st.clone(), failed: vec![] } }

    /// AI mode: switches entry `i` off unless it is off already; originals on disk first.
    fn ai_off_one(&mut self, i: usize) -> Option<String> {
        let e = &self.st.entries[i];
        if !e.on || e.ai_off { return None; }
        self.st.entries[i].ai_off = true;
        if let Some(m) = self.persist() { self.st.entries[i].ai_off = false; return Some(m); }
        let e = &mut self.st.entries[i];
        let errs = apply_off(e);
        let name = e.family.clone();
        let _ = self.persist();
        log(format!("{name}: AI model loaded, switched off{}", if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) }));
        (!errs.is_empty()).then(|| format!("{name}: {}", errs.join("; ")))
    }

    fn persist(&self) -> Option<String> { save(&self.st).err().map(|e| format!("state not saved: {e:#}")) }

    fn handle(&mut self, req: Request) -> Reply {
        match req {
            Request::Status => self.reply(true, "ok"),
            Request::Set { family, key, mode: Some(mode), apps } => {
                if mode == Mode::Follow && apps.is_empty() { return self.reply(false, "follow needs at least one --app <exe>"); }
                let apps: Vec<String> = apps.iter().map(|a| a.to_lowercase()).collect();
                let idx = match self.st.entries.iter().position(|e| e.key == key) {
                    // Already managed: keep the recorded originals, change only the mode. An entry
                    // AI mode made becomes the user's (AI mode no longer puts it back).
                    Some(i) => { let e = &mut self.st.entries[i]; e.mode = mode; e.apps = apps; e.ai_off = false; e.ai_only = false; i }
                    None => match new_entry(&family, &key, mode, apps) {
                        Ok(e) => { self.st.entries.push(e); self.st.entries.len() - 1 }
                        Err(m) => return self.reply(false, m),
                    },
                };
                if let Some(m) = self.persist() { return self.reply(false, m); } // originals on disk before any change
                let run_now = mode == Mode::Follow && self.seed_one(idx);
                let e = &mut self.st.entries[idx];
                let errs = if run_now { if e.on { vec![] } else { apply_on(e) } } else { apply_off(e) };
                let name = e.family.clone();
                let on = e.on;
                let _ = self.persist();
                log(format!("{name}: mode {mode:?}, now {}{}", if on { "on" } else { "off" }, if errs.is_empty() { String::new() } else { format!(" ({} problems: {})", errs.len(), errs.join("; ")) }));
                self.reply(errs.is_empty(), format!("{name}: {} ({})", if on { "on (its app is running)" } else { "off" }, if errs.is_empty() { "ok".into() } else { errs.join("; ") }))
            }
            Request::Set { key, mode: None, .. } => {
                let Some(i) = self.st.entries.iter().position(|e| e.key == key) else { return self.reply(false, "not managed") };
                let mut e = self.st.entries.remove(i);
                let errs = apply_on(&mut e);
                let _ = self.persist();
                log(format!("{}: back to normal{}", e.family, if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) }));
                self.reply(errs.is_empty(), format!("{}: back to normal{}", e.family, if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) }))
            }
            Request::Ai { keys, off: true } => {
                // Families already managed first (no scan), then one scan for the new ones.
                let mut errs = Vec::new();
                let (known, new): (Vec<String>, Vec<String>) = keys.into_iter().partition(|k| self.st.entries.iter().any(|e| &e.key == k));
                for k in &known {
                    if let Some(i) = self.st.entries.iter().position(|e| &e.key == k) { errs.extend(self.ai_off_one(i)); }
                }
                let mut failed = Vec::new();
                if !new.is_empty() {
                    match crate::scan::run(0, crate::scan::SortBy::Private, false) {
                        Ok(report) => for k in new {
                            match make_entry(&report, &k, &k, Mode::Off, vec![], true) {
                                Ok(mut e) => {
                                    e.ai_only = true;
                                    self.st.entries.push(e);
                                    let i = self.st.entries.len() - 1;
                                    errs.extend(self.ai_off_one(i));
                                }
                                Err(m) => { log(format!("{k}: AI mode: {m} (the tray closes its processes)")); failed.push(k); }
                            }
                        },
                        Err(e) => { errs.push(format!("scan: {e:#}")); failed = new; }
                    }
                }
                let mut r = self.reply(errs.is_empty(), if errs.is_empty() { "off for AI mode".to_string() } else { errs.join("; ") });
                r.failed = failed;
                r
            }
            Request::Ai { keys, off: false } => {
                let mut errs = Vec::new();
                for key in keys {
                    let Some(i) = self.st.entries.iter().position(|e| e.key == key && e.ai_off) else { continue };
                    let e_errs = if self.st.entries[i].ai_only {
                        let mut e = self.st.entries.remove(i);
                        apply_on(&mut e)
                    } else {
                        let e = &mut self.st.entries[i];
                        e.ai_off = false;
                        if e.on { vec![] } else { apply_on(e) }
                    };
                    let _ = self.persist();
                    log(format!("{key}: AI model unloaded, back on{}", if e_errs.is_empty() { String::new() } else { format!(" ({})", e_errs.join("; ")) }));
                    errs.extend(e_errs);
                }
                self.reply(errs.is_empty(), if errs.is_empty() { "back on".to_string() } else { errs.join("; ") })
            }
            Request::Turn { key, on } => {
                let Some(e) = self.st.entries.iter_mut().find(|e| e.key == key) else { return self.reply(false, "not managed") };
                let errs = if on { apply_on(e) } else { apply_off(e) };
                let name = e.family.clone();
                let _ = self.persist();
                log(format!("{name}: turned {}", if on { "on" } else { "off" }));
                self.reply(errs.is_empty(), format!("{name}: {}{}", if on { "on" } else { "off" }, if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) }))
            }
        }
    }

    /// Feeds the follower with the entry's apps already running; true if any is.
    fn seed_one(&mut self, idx: usize) -> bool {
        let Ok(ps) = procs::snapshot() else { return false };
        let e = self.st.entries[idx].clone();
        let mut probe = e.clone();
        probe.on = true; // so `started` only records pids
        for p in ps.iter().filter(|p| e.apps.contains(&p.name.to_lowercase())) {
            let _ = self.follower.started(std::slice::from_ref(&probe), p.pid, &p.name.to_lowercase());
        }
        self.follower.any_running(&e.key)
    }

    fn switch(&mut self, s: Switch) {
        match s {
            Switch::On(k) => {
                self.pending_off.remove(&k);
                if let Some(e) = self.st.entries.iter_mut().find(|e| e.key == k) {
                    let errs = apply_on(e);
                    log(format!("{}: app started, switched on{}", e.family, if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) }));
                }
                let _ = self.persist();
            }
            Switch::Off(k) => { self.pending_off.insert(k, Instant::now() + OFF_GRACE); }
        }
    }

    fn due_offs(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self.pending_off.iter().filter(|(_, t)| **t <= now).map(|(k, _)| k.clone()).collect();
        for k in due {
            self.pending_off.remove(&k);
            if self.follower.any_running(&k) { continue; }
            if let Some(e) = self.st.entries.iter_mut().find(|e| e.key == k && e.mode == Mode::Follow) {
                let errs = apply_off(e);
                log(format!("{}: app closed, switched off{}", e.family, if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) }));
            }
            let _ = self.persist();
        }
    }

    /// Puts back to "off" whatever an updater or the user's other tools switched on.
    fn fix_drift(&mut self) {
        let mut changed = false;
        for e in self.st.entries.iter_mut().filter(|e| !e.on) {
            for d in ondemand::drift(e, &observe(e)) {
                let r = match &d {
                    Seen::Service { name, .. } => switch::set_service_start(name, "disabled", false),
                    Seen::Task { path, .. } => switch::set_task(path, false),
                };
                log(format!("{}: {:?} was switched back on by something else; off again{}", e.family, d, r.err().map(|x| format!(" (failed: {x:#})")).unwrap_or_default()));
                changed = true;
            }
            // A service that should be off but runs (a stop that failed earlier, or started by hand).
            let running: Vec<String> = e.services.iter().filter(|s| switch::service_state(&s.name) == Some(4)).map(|s| s.name.clone()).collect();
            if !running.is_empty() {
                let left = switch::stop_all(&running, Duration::from_secs(15));
                log(format!("{}: stopped {} still running{}", e.family, running.join(", "), if left.is_empty() { String::new() } else { format!(" ({})", left.join("; ")) }));
            }
        }
        if changed { let _ = self.persist(); }
    }
}

/// Runs until a `Msg::Stop` arrives on `rx`'s sender (given to the caller through `ready`).
/// `serve_pipe`: serve the control pipe itself (`keel ram daemon`); in the service the pipe is
/// shared by all features and routes to this loop through `ready`'s sender.
pub fn run(ready: impl FnOnce(Sender<Msg>), serve_pipe: bool) -> anyhow::Result<()> {
    let (tx, rx) = channel();
    ready(tx.clone());
    let mut d = Daemon { st: load(), follower: Follower::default(), pending_off: HashMap::new(), devmap: procs::device_map() };
    log(format!("started; {} managed families", d.st.entries.len()));
    fill_dirs(&mut d.st);
    // After a reboot or crash: follow families on only if their app runs, everything else off.
    for i in 0..d.st.entries.len() {
        let running = d.st.entries[i].mode == Mode::Follow && d.seed_one(i);
        let e = &mut d.st.entries[i];
        let errs = if running { if e.on { vec![] } else { apply_on(e) } } else { apply_off(e) };
        if !errs.is_empty() { log(format!("{}: {}", e.family, errs.join("; "))); }
    }
    let _ = d.persist();
    if serve_pipe { pipe::serve(tx.clone()); }
    follow::spawn(tx.clone());
    crate::observe::trim_self();
    let mut next_drift = Instant::now() + DRIFT_EVERY;
    loop {
        let wake = d.pending_off.values().copied().chain(std::iter::once(next_drift)).min().unwrap_or(next_drift);
        match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(Msg::Stop) => break,
            Ok(Msg::Req(req, reply)) => { let r = d.handle(req); let _ = reply.send(r); crate::observe::trim_self(); }
            Ok(Msg::Proc(follow::ProcEvent::Start(pid))) => {
                if d.st.entries.iter().any(|e| e.mode == Mode::Follow) {
                    if let Some(name) = exe_name(pid, &d.devmap) {
                        for s in d.follower.started(&d.st.entries, pid, &name) { d.switch(s); }
                    }
                }
            }
            Ok(Msg::Proc(follow::ProcEvent::Stop(pid))) => {
                let offs = d.follower.stopped(&d.st.entries, pid);
                for s in offs { d.switch(s); }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        d.due_offs();
        if Instant::now() >= next_drift { d.fix_drift(); next_drift = Instant::now() + DRIFT_EVERY; crate::observe::trim_self(); }
    }
    log("stopped");
    Ok(())
}

/// Puts every managed family back to normal and clears the state (used by `service-uninstall`).
pub fn restore_all() -> Vec<String> {
    let mut st = load();
    let mut out = Vec::new();
    for e in st.entries.iter_mut() {
        let errs = apply_on(e);
        out.push(format!("{}: restored{}", e.family, if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) }));
    }
    st.entries.clear();
    let _ = save(&st);
    out
}

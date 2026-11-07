//! On-demand families: a family is kept *off* (services Disabled, tasks disabled,
//! startup items off) and, in `follow` mode, switched *on* while one of its apps runs. This module
//! is the pure part: what a family's switchable parts are, the state file, drift and follow logic.
use crate::family::{Family, Member};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Always off.
    Off,
    /// Off, except while one of `apps` runs.
    Follow,
}

/// A service as found before Keel touched it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Svc {
    pub name: String,
    /// Original start type: auto | manual | disabled.
    pub start: String,
    #[serde(default)]
    pub delayed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub path: String,
    pub enabled: bool,
}

/// A StartupApproved switch (`HKLM\...\StartupApproved\Run` + value name); `bytes` = original value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Startup {
    pub key: String,
    pub value: String,
    pub bytes: Option<Vec<u8>>,
}

/// One managed family, with the original state of everything it switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub family: String,
    pub key: String,
    pub mode: Mode,
    /// Lowercase exe names that switch a `follow` family on.
    #[serde(default)]
    pub apps: Vec<String>,
    pub services: Vec<Svc>,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub startup: Vec<Startup>,
    /// True while switched on (follow mode, app running).
    #[serde(default)]
    pub on: bool,
    /// The family's vendor folders (lowercase, no trailing `\`): processes running from them are
    /// closed when the family switches off.
    #[serde(default)]
    pub dirs: Vec<String>,
    pub since: String,
    /// AI mode switched this family off (it comes back when the model unloads, also after a reboot).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ai_off: bool,
    /// The entry exists only for AI mode (not put under on-demand control by the user): it is
    /// removed, with everything put back, when the model unloads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ai_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub entries: Vec<Entry>,
}

/// The parts of a family that on-demand switching touches. Drivers and boot/system services never.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parts {
    /// (name, start type now)
    pub services: Vec<(String, String)>,
    pub tasks: Vec<(String, bool)>,
    /// HKLM Run / common Startup items as (StartupApproved key, value). HKCU ones are the user's
    /// side (`hkcu`), switched by the client without admin.
    pub startup: Vec<(String, String)>,
    pub hkcu: Vec<(String, String)>,
    /// Exe names of the family's Run / Startup items (candidates for `--app`).
    pub app_hints: Vec<String>,
}

pub fn parts(f: &Family) -> Result<Parts, String> {
    if let Some(why) = f.protected { return Err(format!("{} is protected ({why})", f.name)); }
    let mut p = Parts::default();
    let mut hints = BTreeSet::new();
    for m in &f.members {
        match m.kind {
            "service" => match m.start.as_deref() {
                Some("boot" | "system") | None => {}
                Some(s) => p.services.push((m.name.clone(), s.to_string())),
            },
            "task" => if let Some(l) = &m.location { p.tasks.push((l.clone(), m.enabled != Some(false))) },
            "run" | "startup" => {
                if let Some(k) = crate::cleanup::approved_key(m) {
                    if k.starts_with("HKCU") { p.hkcu.push((k, m.name.clone())) } else { p.startup.push((k, m.name.clone())) }
                }
                if let Some(e) = exe_of(m) { hints.insert(e); }
            }
            _ => {}
        }
    }
    p.app_hints = hints.into_iter().collect();
    Ok(p)
}

/// Lowercase exe file name a launcher member starts.
fn exe_of(m: &Member) -> Option<String> {
    let cmd = m.path.as_deref()?.split_once(" = ").map(|x| x.1).unwrap_or(m.path.as_deref()?);
    let exe = crate::paths::command_exe(cmd, &crate::sources::env_lookup)?;
    Some(crate::paths::file_name(&exe).to_string())
}

/// Vendor folders of a family worth closing processes from: never Windows' own directories.
pub fn family_dirs(f: &Family) -> Vec<String> {
    let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()).to_lowercase();
    let mut v: Vec<String> = f.members.iter().filter(|m| m.kind == "folder").filter_map(|m| m.path.as_deref())
        .map(|p| p.to_lowercase().trim_end_matches('\\').to_string())
        .filter(|p| !p.starts_with(&win) && p.matches('\\').count() >= 2).collect();
    v.sort();
    v.dedup();
    v
}

/// Pids to close when a family switches off: processes whose exe is inside one of `dirs`, except
/// protected ones and Keel itself (pure; `procs` = (pid, name, exe path)).
pub fn leftovers(dirs: &[String], procs: &[(u32, String, Option<String>)], self_pid: u32) -> Vec<u32> {
    procs.iter().filter(|(pid, name, path)| {
        let Some(p) = path else { return false };
        let p = p.to_lowercase();
        *pid != self_pid && *pid > 4 && crate::deny::protected(name, Some(&p)).is_none()
            && dirs.iter().any(|d| p.starts_with(d.as_str()) && p.as_bytes().get(d.len()) == Some(&b'\\'))
    }).map(|x| x.0).collect()
}

/// Observed state of one switchable thing, for drift checks.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    Service { name: String, start: String },
    Task { path: String, enabled: bool },
}

/// What must be put back to "off" for an entry that should be off (pure).
pub fn drift(e: &Entry, seen: &[Seen]) -> Vec<Seen> {
    if e.on { return vec![]; }
    seen.iter().filter(|s| match s {
        Seen::Service { name, start } => e.services.iter().any(|x| &x.name == name) && start != "disabled",
        Seen::Task { path, enabled } => e.tasks.iter().any(|x| &x.path == path && x.enabled) && *enabled,
    }).cloned().collect()
}

/// Follow mode: running app pids per family. Returns families to switch on / off.
#[derive(Debug, Default)]
pub struct Follower {
    running: BTreeMap<String, BTreeSet<u32>>,
}

#[derive(Debug, PartialEq)]
pub enum Switch { On(String), Off(String) }

impl Follower {
    /// A process started (`name` lowercase exe). Families whose app it is turn on with their first app.
    pub fn started(&mut self, entries: &[Entry], pid: u32, name: &str) -> Vec<Switch> {
        let mut v = Vec::new();
        for e in entries.iter().filter(|e| e.mode == Mode::Follow && e.apps.iter().any(|a| a == name)) {
            let set = self.running.entry(e.key.clone()).or_default();
            if set.is_empty() && !e.on { v.push(Switch::On(e.key.clone())); }
            set.insert(pid);
        }
        v
    }
    /// A process ended. Families whose last app ended turn off.
    pub fn stopped(&mut self, entries: &[Entry], pid: u32) -> Vec<Switch> {
        let mut v = Vec::new();
        for e in entries.iter().filter(|e| e.mode == Mode::Follow) {
            if let Some(set) = self.running.get_mut(&e.key) {
                if set.remove(&pid) && set.is_empty() && e.on { v.push(Switch::Off(e.key.clone())); }
            }
        }
        v
    }
    pub fn any_running(&self, key: &str) -> bool { self.running.get(key).is_some_and(|s| !s.is_empty()) }
}

/// The service's request / reply over the pipe (one JSON line each way).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Status,
    /// Put a family under on-demand control (`mode`), or release it (`mode: None` = back to normal).
    Set { family: String, key: String, mode: Option<Mode>, #[serde(default)] apps: Vec<String> },
    /// Switch a managed family on or off now (tray buttons).
    Turn { key: String, on: bool },
    /// AI mode: switch families off while a model is loaded (`off`), or put back what AI mode
    /// switched off. Families not under on-demand control get a temporary entry (one scan for all).
    Ai { keys: Vec<String>, off: bool },
}

/// AI mode step (pure): families to switch off (chosen, on now, not already switched off by AI
/// mode in this load; an app the user opens again is left alone) and families to put back
/// (switched off by AI mode, and the model unloaded or the family no longer chosen).
/// `failed`: chosen families the service could not switch off in this load (not retried).
pub fn ai_plan(loaded: bool, chosen: &[String], entries: &[Entry], failed: &[String]) -> (Vec<String>, Vec<String>) {
    let mut off = Vec::new();
    if loaded {
        for k in chosen.iter().filter(|k| !failed.contains(k)) {
            match entries.iter().find(|e| &e.key == k) {
                None => off.push(k.clone()),
                Some(e) if e.on && !e.ai_off => off.push(k.clone()),
                _ => {}
            }
        }
    }
    let on = entries.iter().filter(|e| e.ai_off && (!loaded || !chosen.contains(&e.key))).map(|e| e.key.clone()).collect();
    (off, on)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub message: String,
    pub state: State,
    /// AI mode: families the service cannot switch off (not seen as LocalSystem, e.g. installed
    /// per user, or nothing machine-wide to switch); the tray closes their processes itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, mode: Mode, apps: &[&str], on: bool) -> Entry {
        Entry { family: key.into(), key: key.into(), mode, apps: apps.iter().map(|s| s.to_string()).collect(),
            services: vec![Svc { name: "FileSyncHelper".into(), start: "manual".into(), delayed: false }],
            tasks: vec![Task { path: r"\OneDrive Startup Task".into(), enabled: true }, Task { path: r"\Was Off".into(), enabled: false }],
            startup: vec![], on, since: "t".into(), dirs: vec![], ai_off: false, ai_only: false }
    }

    #[test]
    fn ai_mode_plan() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let on = entry("adobe", Mode::Follow, &[], true);
        let kept_off = entry("onedrive", Mode::Off, &[], false);
        let chosen = s(&["adobe", "onedrive", "discord"]);
        assert_eq!(ai_plan(true, &chosen, &[on.clone(), kept_off.clone()], &[]), (s(&["adobe", "discord"]), vec![]),
            "kept-off families stay as they are; unmanaged ones get switched off");
        assert_eq!(ai_plan(true, &chosen, &[on.clone(), kept_off.clone()], &s(&["discord"])).0, s(&["adobe"]), "no retry after a failure");
        let mut ai = on.clone();
        ai.ai_off = true;
        assert_eq!(ai_plan(true, &chosen, &[ai.clone(), kept_off.clone()], &[]).0, s(&["discord"]), "an app opened again is left alone");
        assert_eq!(ai_plan(false, &chosen, &[ai.clone(), kept_off.clone()], &[]), (vec![], s(&["adobe"])), "unloaded: put back");
        assert_eq!(ai_plan(true, &s(&[]), &[ai], &[]).1, s(&["adobe"]), "no longer chosen: put back");
    }

    #[test]
    fn picks_leftovers_inside_vendor_folders_only() {
        let dirs = vec![r"c:\program files\microsoft onedrive".to_string()];
        let p = |pid: u32, n: &str, path: &str| (pid, n.to_string(), Some(path.to_string()));
        let procs = vec![
            p(10, "OneDrive.Sync.Service.exe", r"C:\Program Files\Microsoft OneDrive\26.195\OneDrive.Sync.Service.exe"),
            p(11, "FileCoAuth.exe", r"C:\Program Files\Microsoft OneDrive\26.195\FileCoAuth.exe"),
            p(12, "evil.exe", r"C:\Program Files\Microsoft OneDrive Extra\evil.exe"),
            p(13, "explorer.exe", r"C:\Program Files\Microsoft OneDrive\explorer.exe"),
            p(14, "keel.exe", r"C:\Program Files\Microsoft OneDrive\x.exe"),
            (15, "nopath.exe".to_string(), None),
        ];
        assert_eq!(leftovers(&dirs, &procs, 14), vec![10, 11], "prefix must end at a folder boundary; protected / self skipped");
    }

    #[test]
    fn follows_the_app_with_several_processes() {
        let es = vec![entry("onedrive", Mode::Follow, &["onedrive.exe"], false), entry("adsk", Mode::Off, &["onedrive.exe"], false)];
        let mut f = Follower::default();
        assert_eq!(f.started(&es, 10, "onedrive.exe"), vec![Switch::On("onedrive".into())], "off-mode families never follow");
        let mut on = es.clone();
        on[0].on = true;
        assert!(f.started(&on, 11, "onedrive.exe").is_empty(), "second process: already on");
        assert!(f.started(&on, 12, "notepad.exe").is_empty());
        assert!(f.stopped(&on, 10).is_empty(), "one app process left");
        assert_eq!(f.stopped(&on, 11), vec![Switch::Off("onedrive".into())]);
        assert!(f.stopped(&on, 99).is_empty());
    }

    #[test]
    fn drift_only_for_things_that_should_be_off() {
        let e = entry("onedrive", Mode::Off, &[], false);
        let seen = vec![
            Seen::Service { name: "FileSyncHelper".into(), start: "manual".into() },
            Seen::Service { name: "Other".into(), start: "auto".into() },
            Seen::Task { path: r"\OneDrive Startup Task".into(), enabled: true },
            Seen::Task { path: r"\Was Off".into(), enabled: true },
        ];
        assert_eq!(drift(&e, &seen), vec![seen[0].clone(), seen[2].clone()], "a task the user had off is not ours to manage");
        let mut on = e.clone();
        on.on = true;
        assert!(drift(&on, &seen).is_empty(), "switched on: nothing to fix");
    }

    #[test]
    fn requests_round_trip() {
        let r = Request::Set { family: "Microsoft OneDrive".into(), key: "microsoft onedrive".into(), mode: Some(Mode::Follow), apps: vec!["onedrive.exe".into()] };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""cmd":"set""#) && s.contains(r#""mode":"follow""#));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"status"}"#).unwrap(), Request::Status));
    }
}

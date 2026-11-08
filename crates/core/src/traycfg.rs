//! The tray's per-user settings, shared by the tray (reads, writes its own toggles) and the main
//! window's Settings (picks the menu's favorites). Two files in `%LOCALAPPDATA%\keel`:
//! `tray.toml` holds what the user chooses, `tray-state.toml` what the tray remembers by itself
//! (attach LUIDs, game hand-over), so the main window and the tray never overwrite each other's
//! values.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Menu items the user can add to the tray (id, menu label, what it does). "Open Keel" and
/// "Quit" are always there.
pub const ITEMS: &[(&str, &str, &str)] = &[
    ("status.ram", "Free memory", "line: free RAM of total"),
    ("status.egpu", "eGPU", "line: attached eGPU or none"),
    ("status.telemetry", "eGPU telemetry", "line: temperature, power, clock, VRAM"),
    ("status.game", "Game", "line: running game"),
    ("status.net", "Network", "line: where internet and VPNs go"),
    ("egpu.display", "eGPU display", "submenu: modes and integer scaling of eGPU monitors"),
    ("egpu.primary", "eGPU as main display", "check: make the eGPU monitor the main display on attach"),
    ("egpu.game_gpu", "Game on eGPU", "run the current game on the eGPU from its next start"),
    ("egpu.legion", "Legion Space on iGPU", "check: keep Legion Space on the iGPU and built-in screen"),
    ("egpu.driver", "Driver", "submenu: update the AMD eGPU driver, Windows Update block, status"),
    ("egpu.adrenalin", "Restart Adrenalin", "end and start AMD Adrenalin"),
    ("ram.ondemand", "On-demand apps", "submenu: apps that run only when used, on / off"),
    ("ram.leaks", "Memory history", "open Memory history in the main window"),
    ("net.internet", "Internet", "submenu: general internet over Wi-Fi or LAN"),
    ("net.apply", "Re-apply network", "bring routes and kill switches to the config now"),
    ("logs", "Logs", "submenu: service log, tray log, data folder"),
    ("service.restart", "Restart service", "restart the Keel service"),
];

pub const DEFAULT_FAVORITES: &[&str] = &["status.ram", "status.egpu", "status.net"];

fn default_favorites() -> Vec<String> { DEFAULT_FAVORITES.iter().map(|s| s.to_string()).collect() }

/// What the user chooses (`tray.toml`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Prefs {
    /// Menu items in menu order (ids from `ITEMS`).
    pub favorites: Vec<String>,
    /// Keep Legion Space on the iGPU (graphics preference) and on the built-in screen.
    pub legion_space_pin: bool,
    /// Delete duplicate tray-icon settings of AMD's dgtrayicon.exe (one per start) after a backup.
    pub prune_amd_tray_icons: bool,
    /// Once per eGPU attach, make the first display on the eGPU the Windows main display.
    pub egpu_primary_display: bool,
    /// After integer scaling changes the mode / scale, end SearchHost.exe so Windows Search starts fresh.
    pub restart_search_after_scaling: bool,
    /// Notifications for eGPU alerts (temperature, power, attach / detach).
    pub notifications: bool,
    /// Main window side menu: 0 icon and text, 1 icons only (names as tooltips), 2 text only.
    pub side_menu: u8,
    /// External GPU page widgets the user hid: "history", "gauges", "bars", "tuning", "stress".
    pub hidden_widgets: Vec<String>,
    /// Memory history across restarts: keep a week of sessions. Off: this session and the one before.
    pub keep_memory_history: bool,
    /// App icon colour `#rrggbb` (empty: the built-in dark badge) and style (0 filled,
    /// 1 boat only), and whether the tray's boat uses that colour instead of following the taskbar.
    pub icon_color: String,
    pub icon_style: u8,
    pub tray_icon_color: bool,
    /// AI mode: while a local AI model is loaded, these app families (picked on the Apps page)
    /// are switched off by the service, and back on when it unloads.
    pub ai_mode: bool,
    pub ai_mode_apps: Vec<AiApp>,
    /// Game mode: the same, while a game (GPU apps page: folders and programs) runs, on any GPU.
    pub game_mode: bool,
    pub game_mode_apps: Vec<AiApp>,
    /// Game mode also turns every display but the main one off while the game runs.
    pub game_one_display: bool,
    /// A game set to the eGPU that opens on another display is moved to the eGPU monitor.
    pub game_to_egpu_screen: bool,
    /// Legion Space coming to the front right after a game starts is minimized (the game keeps the screen).
    pub game_hide_legion: bool,
    /// Home widget order ("memory", "egpu", "network"); empty: the default order.
    pub home_order: Vec<String>,
    /// External GPU page: the Tuning section above Live.
    pub egpu_tuning_first: bool,
    /// Memory history: sample every minute instead of every 5 minutes (more detail sooner).
    pub fast_samples: bool,
    /// Memory history: process names the user follows (shown even without a verdict).
    pub watched: Vec<String>,
    /// Memory history: process names (or pool tags) the user dismissed.
    pub ignored: Vec<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { favorites: default_favorites(), legion_space_pin: false, prune_amd_tray_icons: true, egpu_primary_display: false,
               restart_search_after_scaling: true, notifications: true, side_menu: 0, hidden_widgets: Vec::new(),
               keep_memory_history: false, fast_samples: false, home_order: Vec::new(), egpu_tuning_first: false, ai_mode: false, ai_mode_apps: Vec::new(),
               game_mode: false, game_mode_apps: Vec::new(), game_one_display: false, game_to_egpu_screen: true, game_hide_legion: true,
               icon_color: String::new(), icon_style: 0, tray_icon_color: false, watched: Vec::new(), ignored: Vec::new() }
    }
}

/// An app family AI mode switches off (family key from the scan, and its name for the UI).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiApp {
    pub key: String,
    pub name: String,
}

/// What the tray remembers by itself (`tray-state.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Memory {
    /// LUID of the eGPU attach already handled (LUIDs change on every attach and boot).
    pub primary_done_luid: u64,
    /// LUID of the eGPU attach on which integer-scaling monitors were re-applied.
    pub int_done_luid: u64,
    /// Monitors in the game hand-over (the integer desktop mode comes back after the game exits).
    pub int_game_handover: Vec<String>,
}

pub fn prefs_path() -> PathBuf { crate::paths::user_data_dir().join("tray.toml") }
pub fn memory_path() -> PathBuf { crate::paths::user_data_dir().join("tray-state.toml") }

fn read<T: for<'de> Deserialize<'de> + Default>(p: &std::path::Path) -> Option<T> {
    std::fs::read_to_string(p).ok().and_then(|s| toml::from_str(&s).ok())
}

fn write<T: Serialize>(p: &std::path::Path, v: &T) -> anyhow::Result<()> {
    std::fs::create_dir_all(crate::paths::user_data_dir())?;
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string(v)?)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

/// Home widget ids in display order: the saved order, unknown ids dropped, missing ones appended (pure).
pub fn home_order(saved: &[String]) -> Vec<String> {
    const ALL: [&str; 3] = ["memory", "egpu", "network"];
    let mut v: Vec<String> = saved.iter().filter(|s| ALL.contains(&s.as_str())).cloned().collect();
    v.dedup();
    for a in ALL { if !v.iter().any(|x| x == a) { v.push(a.to_string()); } }
    v
}

/// Moves the item at `i` by `delta` places (pure; out-of-range moves change nothing).
pub fn moved(mut v: Vec<String>, i: usize, delta: i32) -> Vec<String> {
    let j = i as i64 + delta as i64;
    if i < v.len() && j >= 0 && (j as usize) < v.len() { v.swap(i, j as usize); }
    v
}

impl Prefs {
    pub fn load() -> Self { read(&prefs_path()).unwrap_or_default() }
    pub fn save(&self) -> anyhow::Result<()> { write(&prefs_path(), self) }
    pub fn has(&self, id: &str) -> bool { self.favorites.iter().any(|f| f == id) }
}

impl Memory {
    pub fn load() -> Self { read(&memory_path()).unwrap_or_default() }
    pub fn save(&self) -> anyhow::Result<()> { write(&memory_path(), self) }
}

/// A display job the main window hands to the tray: mode changes and integer scaling need the
/// tray's keep-or-go-back box and its integer scaling records. One file, newest request wins;
/// the tray takes it within a second and ignores requests older than 30 s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrayRequest {
    /// Unix milliseconds when it was written.
    pub t: i64,
    /// "integer" (switch integer scaling `on` for `monitor`) or "mode" (set `mode` on `monitor`).
    pub cmd: String,
    pub monitor: String,
    #[serde(default)]
    pub on: bool,
    /// Width, height, refresh rate.
    #[serde(default)]
    pub mode: Option<(u32, u32, u32)>,
}

pub fn request_path() -> PathBuf { crate::paths::user_data_dir().join("tray-request.json") }

impl TrayRequest {
    pub fn send(mut self) -> anyhow::Result<()> {
        self.t = chrono::Utc::now().timestamp_millis();
        let p = request_path();
        if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
        std::fs::write(p, serde_json::to_string(&self)?)?;
        Ok(())
    }

    /// The request newer than `seen` (then remembered), if it is fresh.
    pub fn take(seen: &mut i64) -> Option<TrayRequest> {
        let r: TrayRequest = serde_json::from_str(&std::fs::read_to_string(request_path()).ok()?).ok()?;
        if r.t <= *seen { return None; }
        *seen = r.t;
        (chrono::Utc::now().timestamp_millis() - r.t < 30_000).then_some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_files_keep_defaults_and_round_trip() {
        let p: Prefs = toml::from_str("legion_space_pin = true\n").unwrap();
        assert!(p.legion_space_pin && p.notifications);
        assert_eq!(p.favorites, default_favorites());
        assert_eq!(toml::from_str::<Prefs>(&toml::to_string(&p).unwrap()).unwrap(), p);
        let m = Memory { primary_done_luid: 103219, int_game_handover: vec!["DISPLAY5".into()], ..Default::default() };
        assert_eq!(toml::from_str::<Memory>(&toml::to_string(&m).unwrap()).unwrap(), m);
    }

    #[test]
    fn widget_order() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(home_order(&[]), s(&["memory", "egpu", "network"]));
        assert_eq!(home_order(&s(&["network", "bogus", "memory"])), s(&["network", "memory", "egpu"]));
        assert_eq!(moved(s(&["a", "b", "c"]), 2, -1), s(&["a", "c", "b"]));
        assert_eq!(moved(s(&["a", "b"]), 0, -1), s(&["a", "b"]));
    }

    #[test]
    fn default_favorites_are_known_items() {
        for f in DEFAULT_FAVORITES { assert!(ITEMS.iter().any(|(id, _, _)| id == f)); }
    }
}

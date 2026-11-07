//! Games page (ui/games.slint). Installed games come from a scan (`keel_egpu::scan`, a few seconds,
//! so the last result is kept in `%LOCALAPPDATA%\keel\games.json` and shown at once). What counts
//! as a game lives in the service's `egpu.toml` (`game_paths`, `game_exes`; changed through the
//! service, which re-attaches the eGPU feature). "Runs on the eGPU" is Windows' per-user GPU choice
//! (`keel_egpu::gpupref`), set directly.
use crate::{GameApp, GameRow, GamesData, MainWindow};
use keel_egpu::gpupref;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;

#[derive(Serialize, Deserialize, Clone, Default)]
struct Game { name: String, exe: String, source: String }

#[derive(Serialize, Deserialize, Default)]
struct Cache { scanned: i64, games: Vec<Game> }

thread_local! {
    static CACHE: RefCell<Option<Cache>> = Default::default();
    /// Programs on the eGPU that are not installed games (rows of "Other programs").
    static OTHERS: RefCell<Vec<String>> = Default::default();
}

fn cache_path() -> std::path::PathBuf { keel_core::paths::user_data_dir().join("games.json") }

fn cache() -> Cache {
    CACHE.with_borrow_mut(|c| {
        if c.is_none() { *c = Some(std::fs::read_to_string(cache_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()); }
        let c = c.as_ref().unwrap();
        Cache { scanned: c.scanned, games: c.games.clone() }
    })
}

/// Built-in folder patterns, said in words; the owner's folders are shown as they are.
fn folder_label(p: &str) -> (String, String) {
    match p.to_lowercase().as_str() {
        r"\steamapps\common\" => ("Steam libraries".into(), r"any drive: ...\steamapps\common\".into()),
        r"\epic games\" => ("Epic Games".into(), r"any ...\Epic Games\ folder".into()),
        r"\xboxgames\" => ("Xbox app games".into(), r"any ...\XboxGames\ folder".into()),
        r"\gog galaxy\games\" => ("GOG Galaxy".into(), r"any ...\GOG Galaxy\Games\ folder".into()),
        _ => (p.to_string(), String::new()),
    }
}

fn split(p: &str) -> GameApp {
    let path = std::path::Path::new(p);
    GameApp { name: path.file_name().map_or(p.to_string(), |n| n.to_string_lossy().into_owned()).into(),
              path: path.parent().map_or(String::new(), |d| d.display().to_string()).into() }
}

/// The running game from the service's status file: name and full path.
fn running() -> Option<(String, String)> {
    let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(keel_egpu::util::status_path()).ok()?).ok()?;
    Some((s["game"].as_str()?.to_string(), s["game_exe"].as_str().unwrap_or("").to_string()))
}

pub fn refresh(ui: &MainWindow) {
    let d = ui.global::<GamesData>();
    let cfg = keel_egpu::config::Config::load();
    match running() {
        Some((name, path)) => {
            d.set_has_game(true);
            d.set_now_name(name.into());
            d.set_now_on_egpu(!path.is_empty() && gpupref::is_high_performance(&path));
            d.set_now_path(path.into());
        }
        None => d.set_has_game(false),
    }
    let on = gpupref::high_performance();
    let has = |exe: &str| on.iter().any(|e| e.eq_ignore_ascii_case(exe));
    let c = cache();
    let rows: Vec<GameRow> = c.games.iter().map(|g| GameRow {
        name: g.name.clone().into(), path: g.exe.clone().into(), source: g.source.clone().into(), egpu: has(&g.exe),
    }).collect();
    d.set_installed(ModelRc::new(VecModel::from(rows)));
    if !d.get_scanning() {
        d.set_scan_note(if c.scanned == 0 { "not scanned yet".into() } else {
            let when = chrono::DateTime::from_timestamp(c.scanned, 0).map(|t| t.with_timezone(&chrono::Local).format("%b %d %H:%M").to_string()).unwrap_or_default();
            format!("{} found · scanned {when}", c.games.len()).into()
        });
    }
    let others: Vec<String> = on.into_iter().filter(|e| !c.games.iter().any(|g| g.exe.eq_ignore_ascii_case(e))).collect();
    d.set_on_egpu(ModelRc::new(VecModel::from(others.iter().map(|p| split(p)).collect::<Vec<_>>())));
    OTHERS.with_borrow_mut(|o| *o = others);
    d.set_folders(ModelRc::new(VecModel::from(cfg.game_paths.iter().map(|p| {
        let (name, path) = folder_label(p);
        GameApp { name: name.into(), path: path.into() }
    }).collect::<Vec<_>>())));
    d.set_programs(ModelRc::new(VecModel::from(cfg.game_exes.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>())));
}

/// Scans on a worker thread, keeps the result and shows it.
pub fn scan(ui: &MainWindow) {
    let d = ui.global::<GamesData>();
    if d.get_scanning() { return; }
    d.set_scanning(true);
    d.set_scan_note("reading Steam, Epic and GOG...".into());
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let found = keel_egpu::scan::installed();
        let c = Cache { scanned: chrono::Utc::now().timestamp(), games: found.into_iter().map(|f| Game {
            name: f.name, exe: f.exe.display().to_string(), source: f.source.into() }).collect() };
        let _ = std::fs::write(cache_path(), serde_json::to_string(&c).unwrap_or_default());
        let _ = w.upgrade_in_event_loop(move |ui| {
            CACHE.with_borrow_mut(|x| *x = Some(c));
            ui.global::<GamesData>().set_scanning(false);
            refresh(&ui);
        });
    });
}

/// Sends both lists to the service (it saves them and re-attaches), then shows the result.
fn save(ui: &MainWindow, paths: Vec<String>, exes: Vec<String>) {
    let w = ui.as_weak();
    ui.global::<GamesData>().set_result("Saving...".into());
    std::thread::spawn(move || {
        let r = keel_service::call(&keel_service::Request::EgpuGames { paths, exes });
        let _ = w.upgrade_in_event_loop(move |ui| {
            ui.global::<GamesData>().set_result(match r {
                Ok(r) if r.ok => format!("Saved: {}", r.message).into(),
                Ok(r) => format!("Not saved: {}", r.message).into(),
                Err(e) => format!("Not saved: {e:#}").into(),
            });
            refresh(&ui);
        });
    });
}

fn lists() -> (Vec<String>, Vec<String>) {
    let cfg = keel_egpu::config::Config::load();
    (cfg.game_paths, cfg.game_exes)
}

fn set_pref(ui: &MainWindow, exe: &str, on: bool) {
    let r = gpupref::set_high_performance(exe, on);
    keel_core::log::user(format!("games: {exe} high-performance GPU {on}: {r:?}"));
    ui.global::<GamesData>().set_result(match r {
        Ok(()) if on => "eGPU from its next start.".into(),
        Ok(()) => "Back to Windows' choice.".into(),
        Err(e) => format!("Not changed: {e:#}").into(),
    });
    refresh(ui);
}

pub fn wire(ui: &MainWindow) {
    let d = ui.global::<GamesData>();
    let w = ui.as_weak();
    d.on_refresh(move || if let Some(ui) = w.upgrade() {
        refresh(&ui);
        // First visit with nothing scanned yet: scan once by itself.
        if cache().scanned == 0 { scan(&ui); }
    });
    let w = ui.as_weak();
    d.on_scan(move || if let Some(ui) = w.upgrade() { scan(&ui); });
    let w = ui.as_weak();
    d.on_set_egpu(move |i, on| {
        let Some(ui) = w.upgrade() else { return };
        if let Some(g) = cache().games.get(i.max(0) as usize) { set_pref(&ui, &g.exe, on); }
    });
    let w = ui.as_weak();
    d.on_run_now_on_egpu(move || {
        let Some(ui) = w.upgrade() else { return };
        if let Some((_, path)) = running().filter(|(_, p)| !p.is_empty()) { set_pref(&ui, &path, true); }
    });
    let w = ui.as_weak();
    d.on_egpu_remove(move |i| {
        let Some(ui) = w.upgrade() else { return };
        if let Some(exe) = OTHERS.with_borrow(|o| o.get(i.max(0) as usize).cloned()) { set_pref(&ui, &exe, false); }
    });
    let w = ui.as_weak();
    d.on_folder_add(move || {
        let Some(ui) = w.upgrade() else { return };
        let Some(dir) = rfd::FileDialog::new().set_title("Folder with games").pick_folder() else { return };
        let mut s = dir.display().to_string();
        if !s.ends_with('\\') { s.push('\\'); }
        let (mut paths, exes) = lists();
        if !paths.iter().any(|p| p.eq_ignore_ascii_case(&s)) { paths.push(s); }
        save(&ui, paths, exes);
    });
    let w = ui.as_weak();
    d.on_folder_remove(move |i| {
        let Some(ui) = w.upgrade() else { return };
        let (mut paths, exes) = lists();
        if (i as usize) < paths.len() { paths.remove(i as usize); }
        save(&ui, paths, exes);
    });
    let w = ui.as_weak();
    d.on_program_add(move || {
        let Some(ui) = w.upgrade() else { return };
        let Some(file) = rfd::FileDialog::new().set_title("Game program").add_filter("Program", &["exe"]).pick_file() else { return };
        let Some(name) = file.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
        let _ = gpupref::set_high_performance(&file.display().to_string(), true);
        let (paths, mut exes) = lists();
        if !exes.iter().any(|e| e.eq_ignore_ascii_case(&name)) { exes.push(name); }
        save(&ui, paths, exes);
    });
    let w = ui.as_weak();
    d.on_program_remove(move |i| {
        let Some(ui) = w.upgrade() else { return };
        let (paths, mut exes) = lists();
        if (i as usize) < exes.len() { exes.remove(i as usize); }
        save(&ui, paths, exes);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn built_in_folders_read_as_words() {
        assert_eq!(super::folder_label(r"\steamapps\common\").0, "Steam libraries");
        assert_eq!(super::folder_label(r"D:\Games\"), (r"D:\Games\".to_string(), String::new()));
    }
}

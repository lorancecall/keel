//! "Is a game running?" by exe path / name.
use crate::config::Config;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub struct GameWatch {
    sys: Option<System>,
}

impl GameWatch {
    pub fn new() -> Self { Self { sys: System::new().ok() } }

    /// Returns (exe name, full exe path) of the first running game, if any.
    pub fn running_game(&mut self, cfg: &Config) -> Option<(String, String)> {
        let sys = self.sys.as_mut()?;
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet));
        let paths: Vec<String> = cfg.game_paths.iter().map(|p| p.to_lowercase()).collect();
        for p in sys.processes().values() {
            let name = p.name().to_string_lossy().to_string();
            if cfg.game_ignore.iter().any(|i| i.eq_ignore_ascii_case(&name)) { continue; }
            let exe = p.exe().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            if cfg.game_exes.iter().any(|g| g.eq_ignore_ascii_case(&name)) { return Some((name, exe)); }
            let low = exe.to_lowercase();
            if !low.is_empty() && paths.iter().any(|s| low.contains(s.as_str())) { return Some((name, exe)); }
        }
        None
    }
}

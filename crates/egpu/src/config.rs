//! egpu.toml (in %ProgramData%\keel). Created on first service / admin run (see `Config::load_or_create`).
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Seconds between adapter / game checks.
    pub poll_secs: u64,
    /// Ollama endpoint; models are unloaded when a game starts or the eGPU goes away.
    pub ollama_url: String,
    /// Shared GPU lock file. While a game runs the guard holds it as
    /// holder "game", so cooperating GPU jobs wait. Unset = do not touch.
    pub gpu_lock: Option<PathBuf>,
    /// A running process counts as a game if its exe path contains one of these (case-insensitive).
    pub game_paths: Vec<String>,
    /// ... or if its file name is listed here.
    pub game_exes: Vec<String>,
    /// File names that never count as games (launchers, crash handlers, anti-cheat).
    pub game_ignore: Vec<String>,
    /// Tray: restart Adrenalin automatically when it hangs without a window after a launch.
    pub adrenalin_autorestart: bool,
    /// Keep the iGPU driver's atiadlxx.dll in each Legion Space folder, so Legion Space's AMD
    /// features reach the iGPU instead of whatever stale ADL System32 holds.
    /// Opt-in; `keel egpu legion-fix on|off` sets it. The service keeps the copy current.
    pub legion_space_fix: bool,
    /// Stop NVIDIA's user-space container services while no NVIDIA GPU is present and start them
    /// again when one appears (saves ~40 MB with only AMD GPUs). Start types are not changed.
    /// Opt-in; re-read once a minute. Drivers are never touched.
    pub nvidia_services_follow_egpu: bool,
    /// Services handled by `nvidia_services_follow_egpu`; limited to `nvsvc::ALLOWED`.
    pub nvidia_services: Vec<String>,
    /// Driver versions with known problems on this machine (version -> note). driver-update
    /// refuses them without --force; driver-status prints the note.
    pub driver_known_issues: std::collections::BTreeMap<String, String>,
    pub gpus: Vec<GpuProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuProfile {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Exact PnP hardware ID, used by driver-update / driver-block.
    pub hardware_id: String,
    /// Board power cap in watts (NVIDIA). Clamped to what the card allows.
    pub power_limit_w: Option<u32>,
    /// Graphics clock ceiling in MHz (NVIDIA). Main protection against transient spikes.
    pub clock_max_mhz: Option<u32>,
    /// Memory clock ceiling in MHz (NVIDIA RTX 30 and newer; older GPUs ignore it).
    pub mem_clock_max_mhz: Option<u32>,
    /// Thermal watchdog: at or above this the clock ceiling steps down by clock_step_mhz,
    /// never below clock_floor_mhz; it steps back up after 60 s at least 8 C cooler.
    pub temp_max_c: Option<u32>,
    pub clock_step_mhz: u32,
    pub clock_floor_mhz: u32,
}

impl Default for GpuProfile {
    fn default() -> Self {
        Self {
            name: String::new(), vendor_id: 0, device_id: 0, hardware_id: String::new(),
            power_limit_w: None, clock_max_mhz: None, mem_clock_max_mhz: None, temp_max_c: None,
            clock_step_mhz: 75, clock_floor_mhz: 900,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_secs: 2,
            ollama_url: "http://127.0.0.1:11434".into(),
            gpu_lock: None,
            game_paths: vec![
                r"\steamapps\common\".into(), r"\Epic Games\".into(),
                r"\XboxGames\".into(), r"\GOG Galaxy\Games\".into(),
            ],
            game_exes: vec![],
            adrenalin_autorestart: true,
            legion_space_fix: false,
            nvidia_services_follow_egpu: false,
            nvidia_services: crate::nvsvc::DEFAULT_SERVICES.iter().map(|s| s.to_string()).collect(),
            driver_known_issues: [
                ("32.0.32015.7029".to_string(), "AMD 26.10.1: Miracast (wireless display) drops right after connecting".to_string()),
                ("32.0.32015.12012".to_string(), "AMD 26.10.2: Miracast (wireless display) drops right after connecting".to_string()),
            ].into(),
            game_ignore: vec![
                "UnityCrashHandler64.exe".into(), "UnityCrashHandler32.exe".into(),
                "CrashReportClient.exe".into(), "EasyAntiCheat.exe".into(),
                "EasyAntiCheat_EOS.exe".into(), "BEService.exe".into(),
            ],
            // eGPUs get a profile when they first attach (`add_found`); none built in.
            gpus: Vec::new(),
        }
    }
}

impl Config {
    /// Read-only load for non-admin callers (CLI, tray): `egpu.toml`, else defaults.
    pub fn load() -> Self {
        std::fs::read_to_string(crate::util::config_path()).ok().and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Service / admin only: writes the default `egpu.toml` on first use.
    pub fn load_or_create() -> anyhow::Result<Self> {
        let path = crate::util::config_path();
        if !path.exists() {
            keel_core::paths::ensure_may_write_data_dir()?;
            std::fs::create_dir_all(crate::util::data_dir())?;
            std::fs::write(&path, toml::to_string_pretty(&Config::default())?)?;
            crate::util::log(format!("wrote default config {}", path.display()));
        }
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path)?)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(cfg)
    }

    /// Admin only (ProgramData). Rewrites the whole file from the parsed config.
    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::write(crate::util::config_path(), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn profile_for(&self, vendor_id: u32, device_id: u32) -> Option<&GpuProfile> {
        self.gpus.iter().find(|g| g.vendor_id == vendor_id && g.device_id == device_id)
    }

    /// Adds a profile (driver defaults, no limits) for each attached external GPU that has none:
    /// `found` = (adapter name, vendor, device, hardware ID). Returns the names added. Pure; tested.
    pub fn add_found(&mut self, found: &[(String, u32, u32, String)]) -> Vec<String> {
        let mut added = Vec::new();
        for (name, ven, dev, hw) in found {
            if self.profile_for(*ven, *dev).is_some() { continue; }
            let p = GpuProfile { name: format!("{name} (eGPU)"), vendor_id: *ven, device_id: *dev, hardware_id: hw.clone(), ..Default::default() };
            added.push(p.name.clone());
            self.gpus.push(p);
        }
        added
    }
}

#[cfg(test)]
mod found_tests {
    use super::*;

    #[test]
    fn adds_profiles_for_new_egpus() {
        let mut c = Config::default();
        assert!(c.gpus.is_empty(), "no GPUs built in");
        let found = vec![("AMD Radeon RX 7600M XT".to_string(), 0x1002, 0x7480, r"PCI\VEN_1002&DEV_7480".to_string())];
        assert_eq!(c.add_found(&found), vec!["AMD Radeon RX 7600M XT (eGPU)".to_string()]);
        assert!(c.profile_for(0x1002, 0x7480).is_some_and(|p| p.power_limit_w.is_none() && p.clock_max_mhz.is_none()));
        c.gpus[0].power_limit_w = Some(120);
        assert!(c.add_found(&found).is_empty(), "a known eGPU keeps its profile");
        assert_eq!(c.gpus[0].power_limit_w, Some(120));
    }
}

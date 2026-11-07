//! NVIDIA limits + telemetry through NVML (nvml.dll ships with the driver; loaded at runtime,
//! so the guard still starts when no NVIDIA driver is present). Setting limits needs admin.
use crate::config::GpuProfile;
use crate::util::{alert, log};
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::enums::device::GpuLockedClocksSetting;
use nvml_wrapper::Nvml;
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Telemetry {
    pub power_w: f32,
    pub power_limit_w: f32,
    pub temp_c: u32,
    pub clock_mhz: u32,
    pub clock_cap_mhz: Option<u32>,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub usage_pct: u32,
    pub mem_clock_mhz: Option<u32>,
    pub fan_pct: Option<u32>,
    pub power_min_w: Option<f32>,
    pub power_max_w: Option<f32>,
    pub power_default_w: Option<f32>,
    pub clock_max_supported_mhz: Option<u32>,
    pub mem_clock_max_supported_mhz: Option<u32>,
    pub mem_clock_cap_mhz: Option<u32>,
    pub held_back: Vec<String>,
}

/// Whether NVML can lock memory clocks on this architecture (Ampere and newer; pure, tested).
pub fn mem_lock_supported(arch: &str) -> bool {
    !matches!(arch, "Kepler" | "Maxwell" | "Pascal" | "Volta" | "Turing" | "Unknown" | "")
}

/// NVML throttle-reason bits as words for the UI (pure; tested).
pub fn held_back(bits: u64) -> Vec<String> {
    use nvml_wrapper::bitmasks::device::ThrottleReasons as R;
    let r = R::from_bits_truncate(bits);
    let mut v = Vec::new();
    if r.intersects(R::SW_POWER_CAP | R::HW_POWER_BRAKE_SLOWDOWN) { v.push("power".to_string()); }
    if r.intersects(R::SW_THERMAL_SLOWDOWN | R::HW_THERMAL_SLOWDOWN | R::HW_SLOWDOWN) { v.push("thermal".to_string()); }
    if r.intersects(R::APPLICATIONS_CLOCKS_SETTING) { v.push("clock ceiling".to_string()); }
    if v.is_empty() && r.contains(R::GPU_IDLE) { v.push("idle".to_string()); }
    v
}

impl From<&Telemetry> for crate::telemetry::GpuTelemetry {
    fn from(t: &Telemetry) -> Self {
        Self {
            source: "nvml".into(),
            usage_pct: Some(t.usage_pct as f32),
            vram_used_mb: Some(t.mem_used_mb),
            vram_total_mb: Some(t.mem_total_mb),
            clock_mhz: Some(t.clock_mhz),
            power_w: Some(t.power_w),
            power_limit_w: Some(t.power_limit_w),
            temp_c: Some(t.temp_c as f32),
            clock_cap_mhz: t.clock_cap_mhz,
            mem_clock_mhz: t.mem_clock_mhz,
            fan_pct: t.fan_pct,
            power_min_w: t.power_min_w,
            power_max_w: t.power_max_w,
            power_default_w: t.power_default_w,
            clock_max_supported_mhz: t.clock_max_supported_mhz,
            mem_clock_max_supported_mhz: t.mem_clock_max_supported_mhz,
            mem_clock_cap_mhz: t.mem_clock_cap_mhz,
            held_back: t.held_back.clone(),
            ..Default::default()
        }
    }
}

pub struct NvGuard {
    nvml: Nvml,
    index: u32,
    profile: GpuProfile,
    cap: Option<u32>,
    hot_since: Option<Instant>,
    cool_since: Option<Instant>,
    at_limit_ticks: u32,
    /// The GPU accepts a memory clock ceiling (RTX 30 and newer).
    mem_lock: bool,
}

impl NvGuard {
    /// Finds the profile's device in NVML and applies its power and clock limits.
    pub fn attach(profile: &GpuProfile) -> anyhow::Result<Self> {
        let nvml = Nvml::init()?;
        let want = (profile.device_id << 16) | profile.vendor_id;
        let mut index = None;
        for i in 0..nvml.device_count()? {
            if nvml.device_by_index(i)?.pci_info()?.pci_device_id == want { index = Some(i); break; }
        }
        let index = index.ok_or_else(|| anyhow::anyhow!("NVML does not list {}", profile.name))?;
        let mut g = Self { nvml, index, profile: profile.clone(), cap: profile.clock_max_mhz,
                           hot_since: None, cool_since: None, at_limit_ticks: 0, mem_lock: false };
        g.mem_lock = g.nvml.device_by_index(index).and_then(|d| d.architecture())
            .map(|a| mem_lock_supported(&format!("{a:?}"))).unwrap_or(false);
        g.apply_power_limit();
        g.apply_clock_cap();
        g.apply_mem_cap();
        Ok(g)
    }

    fn apply_power_limit(&mut self) {
        let Some(w) = self.profile.power_limit_w else {
            // No limit in the profile: the board's default (a limit set earlier stays until then).
            let r = self.nvml.device_by_index(self.index).and_then(|mut d| {
                let def = d.power_management_limit_default()?;
                if d.power_management_limit()? != def { d.set_power_management_limit(def)?; }
                Ok(def / 1000)
            });
            match r {
                Ok(w) => log(format!("nvidia: power limit at the driver default ({w} W)")),
                Err(e) => log(format!("nvidia: resetting the power limit failed: {e}")),
            }
            return;
        };
        let r = (|| -> anyhow::Result<u32> {
            let mut d = self.nvml.device_by_index(self.index)?;
            let c = d.power_management_limit_constraints()?;
            let mw = (w * 1000).clamp(c.min_limit, c.max_limit);
            d.set_power_management_limit(mw)?;
            Ok(mw / 1000)
        })();
        match r {
            Ok(set) if set != w => log(format!("nvidia: power limit {w} W out of range, set {set} W")),
            Ok(set) => log(format!("nvidia: power limit {set} W")),
            Err(e) => log(format!("nvidia: power limit failed (admin needed?): {e}")),
        }
    }

    fn apply_clock_cap(&mut self) {
        let Some(max) = self.cap else {
            // No ceiling in the profile: release a ceiling set earlier.
            if let Err(e) = self.nvml.device_by_index(self.index).and_then(|mut d| d.reset_gpu_locked_clocks()) {
                log(format!("nvidia: releasing the clock ceiling failed: {e}"));
            }
            return;
        };
        let r = self.nvml.device_by_index(self.index).and_then(|mut d| {
            d.set_gpu_locked_clocks(GpuLockedClocksSetting::Numeric { min_clock_mhz: 300, max_clock_mhz: max })
        });
        match r {
            Ok(()) => log(format!("nvidia: graphics clock ceiling {max} MHz")),
            Err(e) => log(format!("nvidia: clock ceiling failed (admin needed?): {e}")),
        }
    }

    fn apply_mem_cap(&mut self) {
        if !self.mem_lock { return; }
        let r = self.nvml.device_by_index(self.index).and_then(|mut d| match self.profile.mem_clock_max_mhz {
            None => d.reset_mem_locked_clocks(),
            Some(max) => {
                let lo = d.supported_memory_clocks().ok().and_then(|v| v.into_iter().min()).unwrap_or(max).min(max);
                d.set_mem_locked_clocks(lo, max)
            }
        });
        match (r, self.profile.mem_clock_max_mhz) {
            (Ok(()), Some(m)) => log(format!("nvidia: memory clock ceiling {m} MHz")),
            (Ok(()), None) => {}
            (Err(e), _) => log(format!("nvidia: memory clock ceiling failed: {e}")),
        }
    }

    pub fn telemetry(&self) -> anyhow::Result<Telemetry> {
        let d = self.nvml.device_by_index(self.index)?;
        let mem = d.memory_info()?;
        Ok(Telemetry {
            power_w: d.power_usage()? as f32 / 1000.0,
            power_limit_w: d.enforced_power_limit()? as f32 / 1000.0,
            temp_c: d.temperature(TemperatureSensor::Gpu)?,
            clock_mhz: d.clock_info(Clock::Graphics)?,
            clock_cap_mhz: self.cap,
            mem_used_mb: mem.used / (1024 * 1024),
            mem_total_mb: mem.total / (1024 * 1024),
            usage_pct: d.utilization_rates().map(|u| u.gpu).unwrap_or(0),
            mem_clock_mhz: d.clock_info(Clock::Memory).ok(),
            fan_pct: d.fan_speed(0).ok(),
            power_min_w: d.power_management_limit_constraints().ok().map(|c| c.min_limit as f32 / 1000.0),
            power_max_w: d.power_management_limit_constraints().ok().map(|c| c.max_limit as f32 / 1000.0),
            power_default_w: d.power_management_limit_default().ok().map(|w| w as f32 / 1000.0),
            clock_max_supported_mhz: d.max_clock_info(Clock::Graphics).ok(),
            mem_clock_max_supported_mhz: if self.mem_lock { d.max_clock_info(Clock::Memory).ok() } else { None },
            mem_clock_cap_mhz: if self.mem_lock { self.profile.mem_clock_max_mhz } else { None },
            held_back: d.current_throttle_reasons().map(|r| held_back(r.bits())).unwrap_or_default(),
        })
    }

    /// One watchdog step: thermal step-down / recovery of the clock ceiling, and a log line
    /// when the board sits at its power limit (that is where enclosure PSUs struggle).
    pub fn tick(&mut self) -> Option<Telemetry> {
        let t = match self.telemetry() {
            Ok(t) => t,
            Err(e) => { log(format!("nvidia: telemetry failed: {e}")); return None; }
        };
        if let (Some(tmax), Some(cap)) = (self.profile.temp_max_c, self.cap) {
            let now = Instant::now();
            if t.temp_c >= tmax {
                self.cool_since = None;
                let hot = *self.hot_since.get_or_insert(now);
                if now - hot >= Duration::from_secs(5) && cap > self.profile.clock_floor_mhz {
                    self.cap = Some(cap.saturating_sub(self.profile.clock_step_mhz).max(self.profile.clock_floor_mhz));
                    alert(format!("GPU {} C >= {tmax} C - clock ceiling lowered", t.temp_c));
                    self.apply_clock_cap();
                    self.hot_since = Some(now);
                }
            } else if t.temp_c + 8 <= tmax && cap < self.profile.clock_max_mhz.unwrap_or(cap) {
                self.hot_since = None;
                let cool = *self.cool_since.get_or_insert(now);
                if now - cool >= Duration::from_secs(60) {
                    let max = self.profile.clock_max_mhz.unwrap_or(cap);
                    self.cap = Some((cap + self.profile.clock_step_mhz).min(max));
                    self.apply_clock_cap();
                    self.cool_since = Some(now);
                }
            } else {
                self.hot_since = None;
                self.cool_since = None;
            }
        }
        if t.power_w >= t.power_limit_w * 0.97 {
            self.at_limit_ticks += 1;
            if self.at_limit_ticks == 5 {
                alert(format!("GPU at power limit ({:.0}/{:.0} W, {} MHz, {} C)", t.power_w, t.power_limit_w, t.clock_mhz, t.temp_c));
            }
        } else {
            self.at_limit_ticks = 0;
        }
        Some(t)
    }
}

/// Keeps every NVIDIA DLL in this process loaded until the process ends. The service crashed
/// (access violation at address 0, called from ntdll) when the eGPU came back after a detach had
/// shut NVML down and unloaded its DLLs: something they registered was still called. A pinned
/// module is never unloaded, so nvmlShutdown / nvmlInit across attaches stay safe.
pub fn pin_nvidia_modules() {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_PIN};
    use windows::Win32::System::ProcessStatus::EnumProcessModules;
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut mods = vec![HMODULE::default(); 1024];
        let mut need = 0u32;
        if EnumProcessModules(GetCurrentProcess(), mods.as_mut_ptr(), (mods.len() * size_of::<HMODULE>()) as u32, &mut need).is_err() { return; }
        mods.truncate((need as usize / size_of::<HMODULE>()).min(mods.len()));
        for m in mods {
            let mut buf = [0u16; 520];
            let n = GetModuleFileNameW(Some(m), &mut buf) as usize;
            let path = String::from_utf16_lossy(&buf[..n]);
            let file = path.rsplit('\\').next().unwrap_or("").to_ascii_lowercase();
            if !file.starts_with("nv") { continue; }
            let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
            let mut h = HMODULE::default();
            let _ = GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, windows::core::PCWSTR(w.as_ptr()), &mut h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mem_lock_supported;
    use nvml_wrapper::bitmasks::device::ThrottleReasons as R;

    #[test]
    fn memory_clock_lock_from_ampere_on() {
        assert!(!mem_lock_supported("Turing"), "RTX 20: read-only");
        assert!(mem_lock_supported("Ampere") && mem_lock_supported("Ada") && mem_lock_supported("Blackwell"));
        assert!(!mem_lock_supported("Unknown"));
    }

    #[test]
    fn held_back_reasons_as_words() {
        assert_eq!(super::held_back((R::SW_POWER_CAP | R::APPLICATIONS_CLOCKS_SETTING).bits()), ["power", "clock ceiling"]);
        assert_eq!(super::held_back(R::HW_THERMAL_SLOWDOWN.bits()), ["thermal"]);
        assert_eq!(super::held_back(R::GPU_IDLE.bits()), ["idle"]);
        assert!(super::held_back(0).is_empty());
    }
}

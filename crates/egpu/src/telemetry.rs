//! Vendor-neutral eGPU telemetry. Each attached eGPU gets one backend: NVML (NVIDIA), ADLX
//! (AMD) or PDH counters (any GPU, usage + VRAM only). Fields a backend cannot read stay None.
use crate::config::GpuProfile;
use crate::gpu::{Adapter, VENDOR_AMD};
use crate::util::log;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuTelemetry {
    /// "nvml" | "adlx" | "pdh"
    pub source: String,
    pub name: String,
    pub usage_pct: Option<f32>,
    pub vram_used_mb: Option<u64>,
    pub vram_total_mb: Option<u64>,
    pub clock_mhz: Option<u32>,
    pub mem_clock_mhz: Option<u32>,
    pub power_w: Option<f32>,
    pub power_limit_w: Option<f32>,
    pub temp_c: Option<f32>,
    pub hotspot_c: Option<f32>,
    pub fan_rpm: Option<u32>,
    pub clock_cap_mhz: Option<u32>,
    /// Fan speed in percent of its maximum (NVML).
    pub fan_pct: Option<u32>,
    /// Power limit range the board allows and its factory default (W), for the tuning sliders.
    pub power_min_w: Option<f32>,
    pub power_max_w: Option<f32>,
    pub power_default_w: Option<f32>,
    /// Highest graphics clock the GPU supports (MHz).
    pub clock_max_supported_mhz: Option<u32>,
    /// Highest memory clock, only when the GPU accepts a memory clock ceiling (NVIDIA RTX 30+).
    pub mem_clock_max_supported_mhz: Option<u32>,
    /// Memory clock ceiling in force (MHz).
    pub mem_clock_cap_mhz: Option<u32>,
    /// What holds the clock back now: "power", "thermal", "clock ceiling", "idle" (empty = nothing).
    pub held_back: Vec<String>,
}

impl GpuTelemetry {
    /// One line for the tray tooltip / CLI, e.g. "47 C  13 W  1510 MHz  53%  VRAM 561/8176 MB".
    pub fn summary(&self) -> String {
        let mut p = Vec::new();
        if let Some(t) = self.temp_c { p.push(format!("{t:.0} C")); }
        match (self.power_w, self.power_limit_w) {
            (Some(w), Some(l)) => p.push(format!("{w:.0}/{l:.0} W")),
            (Some(w), None) => p.push(format!("{w:.0} W")),
            _ => {}
        }
        if let Some(c) = self.clock_mhz { p.push(format!("{c} MHz")); }
        if let Some(u) = self.usage_pct { p.push(format!("{u:.0}%")); }
        match (self.vram_used_mb, self.vram_total_mb) {
            (Some(u), Some(t)) => p.push(format!("VRAM {u}/{t} MB")),
            (Some(u), None) => p.push(format!("VRAM {u} MB")),
            _ => {}
        }
        p.join("  ")
    }
}

pub trait GpuBackend {
    /// One sample; None when the GPU stopped answering (the caller then drops the backend).
    fn tick(&mut self) -> Option<GpuTelemetry>;
}

/// Telemetry-only backend for a non-NVIDIA eGPU: ADLX for AMD, PDH when ADLX is unavailable.
pub fn open(a: &Adapter, p: &GpuProfile) -> Option<Box<dyn GpuBackend>> {
    if a.vendor_id == VENDOR_AMD {
        match crate::amd::AmdMonitor::attach(p) {
            Ok(m) => { log(format!("telemetry: ADLX on {}", m.name())); return Some(Box::new(m)); }
            Err(e) => log(format!("telemetry: ADLX unavailable ({e:#}), using PDH")),
        }
    }
    match crate::pdh::PdhMonitor::open(a) {
        Ok(m) => Some(Box::new(m)),
        Err(e) => { log(format!("telemetry: PDH unavailable: {e:#}")); None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_shows_only_known_fields() {
        let t = GpuTelemetry {
            temp_c: Some(47.4), power_w: Some(13.2), clock_mhz: Some(1510), usage_pct: Some(53.0),
            vram_used_mb: Some(561), vram_total_mb: Some(8176), ..Default::default()
        };
        assert_eq!(t.summary(), "47 C  13 W  1510 MHz  53%  VRAM 561/8176 MB");
        let pdh = GpuTelemetry { usage_pct: Some(5.0), vram_used_mb: Some(700), ..Default::default() };
        assert_eq!(pdh.summary(), "5%  VRAM 700 MB");
        assert_eq!(GpuTelemetry::default().summary(), "");
    }

    #[test]
    fn old_status_without_new_fields_still_parses() {
        let t: GpuTelemetry = serde_json::from_str(r#"{"source":"pdh"}"#).unwrap();
        assert_eq!(t.source, "pdh");
        assert!(t.temp_c.is_none());
    }
}

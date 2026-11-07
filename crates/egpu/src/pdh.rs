//! Fallback telemetry for any GPU from the Windows "GPU Engine" / "GPU Adapter Memory"
//! performance counters, keyed by adapter LUID. Usage and dedicated VRAM only.
use crate::gpu::Adapter;
use crate::telemetry::{GpuBackend, GpuTelemetry};
use anyhow::{bail, Result};
use std::collections::HashMap;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::System::Performance::*;

pub struct PdhMonitor {
    query: PDH_HQUERY,
    engine: PDH_HCOUNTER,
    memory: PDH_HCOUNTER,
    tag: String,
    name: String,
    vram_total_mb: u64,
}

/// Counter instance names contain e.g. `luid_0x00000000_0x0b0f3405_phys_0`.
pub fn luid_tag(luid: u64) -> String {
    format!("luid_0x{:08x}_0x{:08x}_", luid >> 32, luid & 0xFFFF_FFFF)
}

/// Like Task Manager: sum the per-process utilization per engine type, report the busiest type.
fn engine_usage(items: &[(String, f64)], tag: &str) -> Option<f64> {
    let mut by_type: HashMap<&str, f64> = HashMap::new();
    for (name, v) in items.iter().filter(|(n, _)| n.contains(tag)) {
        let t = name.rsplit("engtype_").next().unwrap_or("");
        *by_type.entry(t).or_default() += v;
    }
    by_type.values().cloned().reduce(f64::max).map(|v| v.min(100.0))
}

fn add(query: PDH_HQUERY, path: &str) -> Result<PDH_HCOUNTER> {
    let mut c = PDH_HCOUNTER::default();
    let r = unsafe { PdhAddEnglishCounterW(query, &HSTRING::from(path), 0, &mut c) };
    if r != 0 { bail!("PdhAddEnglishCounter {path}: {r:#x}"); }
    Ok(c)
}

fn read_array(c: PDH_HCOUNTER) -> Vec<(String, f64)> {
    unsafe {
        let (mut size, mut count) = (0u32, 0u32);
        let _ = PdhGetFormattedCounterArrayW(c, PDH_FMT_DOUBLE, &mut size, &mut count, None);
        if size == 0 { return Vec::new(); }
        // u64 storage keeps the item array aligned; the names live in the same buffer.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        if PdhGetFormattedCounterArrayW(c, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 { return Vec::new(); }
        std::slice::from_raw_parts(items, count as usize).iter()
            .filter(|i| i.FmtValue.CStatus == 0)
            .map(|i| (i.szName.to_string().unwrap_or_default(), i.FmtValue.Anonymous.doubleValue))
            .collect()
    }
}

impl PdhMonitor {
    pub fn open(a: &Adapter) -> Result<Self> {
        let mut query = PDH_HQUERY::default();
        let r = unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) };
        if r != 0 { bail!("PdhOpenQuery: {r:#x}"); }
        let mut m = Self { query, engine: PDH_HCOUNTER::default(), memory: PDH_HCOUNTER::default(),
                           tag: luid_tag(a.luid), name: a.name.clone(), vram_total_mb: a.vram_mb };
        m.engine = add(query, r"\GPU Engine(*)\Utilization Percentage")?;
        m.memory = add(query, r"\GPU Adapter Memory(*)\Dedicated Usage")?;
        unsafe { PdhCollectQueryData(query) };
        Ok(m)
    }
}

impl GpuBackend for PdhMonitor {
    fn tick(&mut self) -> Option<GpuTelemetry> {
        if unsafe { PdhCollectQueryData(self.query) } != 0 { return None; }
        let mem = read_array(self.memory);
        let used = mem.iter().find(|(n, _)| n.contains(&self.tag)).map(|(_, v)| *v as u64 / (1024 * 1024));
        // The adapter's memory instance vanishes when it is unplugged.
        used?;
        Some(GpuTelemetry {
            source: "pdh".into(),
            name: self.name.clone(),
            usage_pct: engine_usage(&read_array(self.engine), &self.tag).map(|v| v as f32),
            vram_used_mb: used,
            vram_total_mb: Some(self.vram_total_mb),
            ..Default::default()
        })
    }
}

impl Drop for PdhMonitor {
    fn drop(&mut self) { unsafe { PdhCloseQuery(self.query) }; }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luid_tag_matches_counter_instances() {
        assert_eq!(luid_tag(0x0b0f3405), "luid_0x00000000_0x0b0f3405_");
        assert_eq!(luid_tag(0x1_0000_0002), "luid_0x00000001_0x00000002_");
    }

    #[test]
    fn engine_usage_takes_busiest_engine_type_of_this_adapter() {
        let tag = luid_tag(0x0b0f3405);
        let items = vec![
            ("pid_1_luid_0x00000000_0x0b0f3405_phys_0_eng_0_engtype_3D".to_string(), 30.0),
            ("pid_2_luid_0x00000000_0x0b0f3405_phys_0_eng_0_engtype_3D".to_string(), 20.0),
            ("pid_3_luid_0x00000000_0x0b0f3405_phys_0_eng_1_engtype_Compute".to_string(), 70.0),
            ("pid_4_luid_0x00000000_0x0aadda9e_phys_0_eng_0_engtype_3D".to_string(), 90.0),
        ];
        assert_eq!(engine_usage(&items, &tag), Some(70.0));
        assert_eq!(engine_usage(&items[..2], &tag), Some(50.0));
        assert_eq!(engine_usage(&[], &tag), None);
    }
}

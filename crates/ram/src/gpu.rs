//! GPU memory per process and adapter: one wildcard PDH query per counter
//! (`\GPU Process Memory(*)\...`), adapter names from DXGI so iGPU and eGPU are told apart.
use std::collections::HashMap;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
};
use windows::core::{HSTRING, PCWSTR};

/// Adapter key as PDH writes it: `HHHHHHHH_LLLLLLLL` (LUID high / low part, lowercase hex).
pub fn luid_key(high: i32, low: u32) -> String { format!("{:08x}_{:08x}", high as u32, low) }

/// `pid_1234_luid_0x00000000_0x00019333_phys_0` -> (1234, "00000000_00019333").
pub fn parse_instance(name: &str) -> Option<(u32, String)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once('_')?;
    let rest = rest.strip_prefix("luid_0x")?;
    let (high, rest) = rest.split_once("_0x")?;
    let low = rest.split('_').next()?;
    if high.len() != 8 || low.len() != 8 { return None; }
    Some((pid.parse().ok()?, format!("{}_{}", high.to_lowercase(), low.to_lowercase())))
}

/// Adapter key -> description (DXGI). Software adapters included (they get a LUID too).
pub fn adapters() -> Vec<(String, String)> {
    let mut v = Vec::new();
    let Ok(f) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return v };
    for i in 0.. {
        let Ok(a) = (unsafe { f.EnumAdapters1(i) }) else { break };
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        let end = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
        v.push((luid_key(d.AdapterLuid.HighPart, d.AdapterLuid.LowPart), String::from_utf16_lossy(&d.Description[..end])));
    }
    v
}

/// Bytes per (pid, adapter): dedicated, shared, total committed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GpuMem { pub dedicated: u64, pub shared: u64, pub committed: u64 }

fn read_array(counter: PDH_HCOUNTER) -> Vec<(String, u64)> {
    let mut size = 0u32;
    let mut count = 0u32;
    let st = unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, None) };
    if st != PDH_MORE_DATA || size == 0 { return vec![]; }
    let mut buf: Vec<u64> = vec![0; size as usize / 8 + 1];
    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
    if unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, Some(items)) } != 0 { return vec![]; }
    let rows = unsafe { std::slice::from_raw_parts(items, count as usize) };
    rows.iter().filter_map(|r| {
        let name = unsafe { r.szName.to_string().ok()? };
        let v = unsafe { r.FmtValue.Anonymous.largeValue };
        Some((name, v.max(0) as u64))
    }).collect()
}

/// One sample of every process's GPU memory, summed over `phys_N` per (pid, adapter).
pub fn process_memory() -> HashMap<(u32, String), GpuMem> {
    let mut out: HashMap<(u32, String), GpuMem> = HashMap::new();
    unsafe {
        let mut q = PDH_HQUERY::default();
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut q) != 0 { return out; }
        let names = ["Dedicated Usage", "Shared Usage", "Total Committed"];
        let mut counters = [PDH_HCOUNTER::default(); 3];
        for (i, n) in names.iter().enumerate() {
            let path = HSTRING::from(format!(r"\GPU Process Memory(*)\{n}"));
            let _ = PdhAddEnglishCounterW(q, &path, 0, &mut counters[i]);
        }
        if PdhCollectQueryData(q) == 0 {
            for (i, c) in counters.iter().enumerate() {
                for (inst, v) in read_array(*c) {
                    let Some(k) = parse_instance(&inst) else { continue };
                    let e = out.entry(k).or_default();
                    match i { 0 => e.dedicated += v, 1 => e.shared += v, _ => e.committed += v }
                }
            }
        }
        let _ = PdhCloseQuery(q);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gpu_instances_and_luids() {
        assert_eq!(parse_instance("pid_2636_luid_0x00000000_0x00019333_phys_0"), Some((2636, "00000000_00019333".into())));
        assert_eq!(parse_instance("pid_4_luid_0x00000000_0x000192FF_phys_1"), Some((4, "00000000_000192ff".into())));
        assert_eq!(parse_instance("_Total"), None);
        assert_eq!(parse_instance("pid_x_luid_0x00000000_0x00019333_phys_0"), None);
        assert_eq!(luid_key(0, 0x19333), "00000000_00019333");
    }
}

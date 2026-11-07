//! Present display adapters via DXGI (a fresh factory each call, so hot-plug is seen).
use serde::Serialize;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Adapter {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub vram_mb: u64,
    /// Adapter LUID (high << 32 | low): the key of the PDH GPU counters and of ADLX's LUID.
    pub luid: u64,
}

/// Display devices Windows expects to be unplugged (an eGPU on a USB4 / Thunderbolt / OCuLink
/// dock): vendor ID, device ID, hardware ID. The built-in GPU says "no removal".
pub fn external_devices() -> Vec<(u32, u32, String)> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::*;
    let class = windows::core::w!("{4d36e968-e325-11ce-bfc1-08002be10318}");
    let flags = CM_GETIDLIST_FILTER_CLASS | CM_GETIDLIST_FILTER_PRESENT;
    let mut out = Vec::new();
    unsafe {
        let mut len = 0u32;
        if CM_Get_Device_ID_List_SizeW(&mut len, class, flags) != CR_SUCCESS || len == 0 { return out; }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_ID_ListW(class, &mut buf, flags) != CR_SUCCESS { return out; }
        for id in buf.split(|c| *c == 0).filter(|s| !s.is_empty()) {
            let id_str = String::from_utf16_lossy(id);
            let Some((ven, dev, hwid)) = parse_pci_id(&id_str) else { continue };
            let wide: Vec<u16> = id.iter().copied().chain(std::iter::once(0)).collect();
            let mut node = 0u32;
            if CM_Locate_DevNodeW(&mut node, windows::core::PCWSTR(wide.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS { continue; }
            let prop = |p: u32| {
                let mut v = 0u32;
                let mut n = 4u32;
                (CM_Get_DevNode_Registry_PropertyW(node, p, None, Some(&mut v as *mut u32 as *mut _), &mut n, 0) == CR_SUCCESS).then_some(v)
            };
            if is_external(prop(CM_DRP_REMOVAL_POLICY).unwrap_or(0), prop(CM_DRP_CAPABILITIES).unwrap_or(0)) {
                out.push((ven, dev, hwid));
            }
        }
    }
    out
}

/// `PCI\VEN_1002&DEV_7480&SUBSYS_80431E50&REV_C7\4&...` -> (vendor, device, hardware ID). Pure; tested.
pub fn parse_pci_id(instance: &str) -> Option<(u32, u32, String)> {
    let mut parts = instance.split('\\');
    if !parts.next()?.eq_ignore_ascii_case("PCI") { return None; }
    let hw = parts.next()?;
    let field = |k: &str| hw.split('&').find_map(|f| f.to_uppercase().strip_prefix(k).and_then(|v| u32::from_str_radix(v, 16).ok()));
    Some((field("VEN_")?, field("DEV_")?, format!("PCI\\{hw}")))
}

/// Removal policy "expect orderly / surprise removal" (2, 3) or the removable capability. Pure; tested.
pub fn is_external(removal_policy: u32, capabilities: u32) -> bool {
    matches!(removal_policy, 2 | 3) || capabilities & 0x4 != 0
}

pub const VENDOR_NVIDIA: u32 = 0x10DE;
pub const VENDOR_AMD: u32 = 0x1002;

pub fn adapters() -> anyhow::Result<Vec<Adapter>> {
    let mut out = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        for i in 0.. {
            let adapter = match factory.EnumAdapters1(i) {
                Ok(a) => a,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let d = adapter.GetDesc1()?;
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 { continue; }
            let len = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
            let a = Adapter {
                name: String::from_utf16_lossy(&d.Description[..len]),
                vendor_id: d.VendorId,
                device_id: d.DeviceId,
                vram_mb: d.DedicatedVideoMemory as u64 / (1024 * 1024),
                luid: ((d.AdapterLuid.HighPart as u32 as u64) << 32) | d.AdapterLuid.LowPart as u64,
            };
            // One physical GPU can show up once per output path; keep the first.
            if !out.contains(&a) { out.push(a); }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_gpus_from_device_ids_and_removal() {
        assert_eq!(parse_pci_id(r"PCI\VEN_1002&DEV_7480&SUBSYS_80431E50&REV_C7\4&1a2b&0&0008"),
                   Some((0x1002, 0x7480, r"PCI\VEN_1002&DEV_7480&SUBSYS_80431E50&REV_C7".to_string())));
        assert_eq!(parse_pci_id(r"ROOT\BasicDisplay\0000"), None);
        assert!(is_external(3, 0), "surprise removal (USB4 dock)");
        assert!(is_external(1, 0x4), "removable capability");
        assert!(!is_external(1, 0), "the built-in GPU");
    }

    #[test]
    #[ignore]
    fn lists_external_gpus_now() { println!("{:?}", external_devices()); }
}

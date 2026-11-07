//! AMD eGPU through ADLX. ADLX only sees the GPUs of the driver package whose ADL
//! (atiadlxx.dll) is loaded, and the System32 copies can belong to a package no GPU uses. So
//! the eGPU driver's own atiadlxx.dll / amdadlx64.dll are loaded by full path first. One ADL per process: this never sees the iGPU.
use crate::config::GpuProfile;
use crate::telemetry::{GpuBackend, GpuTelemetry};
use adlx_lite::{ffi, AdlxHelper, Gpu, Interface, PerformanceMonitoringServices};
use anyhow::{anyhow, Context, Result};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};

const DISPLAY_CLASS: &str = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

/// Picks the driver folder of the device `PCI\VEN_v&DEV_d...` from (MatchingDeviceId,
/// first UserModeDriverName entry) pairs of the display class keys.
fn pick_driver_dir(keys: &[(String, String)], vendor_id: u32, device_id: u32) -> Option<PathBuf> {
    let want = format!(r"PCI\VEN_{vendor_id:04X}&DEV_{device_id:04X}");
    keys.iter()
        .find(|(id, umd)| id.to_ascii_uppercase().starts_with(&want) && !umd.is_empty())
        .and_then(|(_, umd)| Path::new(umd).parent().map(Path::to_path_buf))
}

/// The DriverStore folder of the installed driver for this GPU (changes with every driver update).
pub fn driver_dir(vendor_id: u32, device_id: u32) -> Option<PathBuf> {
    let class = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE).open_subkey(DISPLAY_CLASS).ok()?;
    let keys: Vec<(String, String)> = class.enum_keys().flatten()
        .filter_map(|n| class.open_subkey(&n).ok())
        .map(|k| {
            let id: String = k.get_value("MatchingDeviceId").unwrap_or_default();
            let umd: Vec<String> = k.get_value("UserModeDriverName").unwrap_or_default();
            (id, umd.into_iter().next().unwrap_or_default())
        })
        .collect();
    pick_driver_dir(&keys, vendor_id, device_id)
}

/// Modules loaded by full path; freed after ADLX has terminated (field order in AmdMonitor).
struct Dlls(Vec<HMODULE>);

impl Dlls {
    fn load(dir: &Path) -> Result<Self> {
        let mut v = Vec::new();
        for name in ["atiadlxx.dll", "amdadlx64.dll"] {
            let p = dir.join(name);
            let w: Vec<u16> = p.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
            let h = unsafe { LoadLibraryExW(PCWSTR(w.as_ptr()), None, LOAD_WITH_ALTERED_SEARCH_PATH) }
                .with_context(|| format!("load {}", p.display()))?;
            v.push(h);
        }
        Ok(Self(v))
    }
}

impl Drop for Dlls {
    fn drop(&mut self) {
        for h in self.0.drain(..).rev() { unsafe { let _ = FreeLibrary(h); } }
    }
}

/// ADLX bound to the eGPU's driver. Fields drop in order: ADLX objects, ADLX itself, the DLLs.
pub struct Adlx {
    gpu: Gpu,
    adlx: AdlxHelper,
    _dlls: Dlls,
}

impl Adlx {
    pub fn open(p: &GpuProfile) -> Result<Self> {
        let dir = driver_dir(p.vendor_id, p.device_id).ok_or_else(|| anyhow!("no installed driver for {:04X}:{:04X}", p.vendor_id, p.device_id))?;
        let dlls = Dlls::load(&dir)?;
        let adlx = AdlxHelper::new()?;
        let want = format!("{:X}", p.device_id);
        let gpu = adlx.system().gpus().map_err(|e| anyhow!("GetGPUs: {e}"))?.iter()
            .find(|g| g.device_id().is_ok_and(|d| d.trim_start_matches("0x").eq_ignore_ascii_case(&want)))
            .ok_or_else(|| anyhow!("ADLX does not list {} (driver {})", p.name, dir.display()))?;
        Ok(Self { gpu, adlx, _dlls: dlls })
    }

    pub fn gpu(&self) -> &Gpu { &self.gpu }
    pub fn system(&self) -> &adlx_lite::System { self.adlx.system() }
}

pub struct AmdMonitor {
    pms: PerformanceMonitoringServices,
    name: String,
    vram_total_mb: Option<u64>,
    adlx: Adlx,
}

impl AmdMonitor {
    pub fn attach(p: &GpuProfile) -> Result<Self> {
        let adlx = Adlx::open(p)?;
        let pms = adlx.system().performance_monitoring_services().map_err(|e| anyhow!("performance monitoring: {e}"))?;
        let name = adlx.gpu().name().unwrap_or("AMD GPU").to_string();
        let vram_total_mb = adlx.gpu().total_vram().ok().map(u64::from);
        Ok(Self { pms, name, vram_total_mb, adlx })
    }

    pub fn name(&self) -> &str { &self.name }
}

impl GpuBackend for AmdMonitor {
    fn tick(&mut self) -> Option<GpuTelemetry> {
        let m = self.pms.current_gpu_metrics(self.adlx.gpu()).ok()?;
        let pos = |v: Option<f64>| v.filter(|x| *x > 0.0);
        Some(GpuTelemetry {
            source: "adlx".into(),
            name: self.name.clone(),
            usage_pct: m.usage().ok().map(|v| v as f32),
            vram_used_mb: m.vram().ok().map(|v| v.max(0) as u64),
            vram_total_mb: self.vram_total_mb,
            clock_mhz: m.clock_speed().ok().map(|v| v.max(0) as u32),
            mem_clock_mhz: m.vram_clock_speed().ok().map(|v| v.max(0) as u32),
            power_w: pos(m.power().ok()).map(|v| v as f32),
            temp_c: pos(m.temperature().ok()).map(|v| v as f32),
            hotspot_c: pos(m.hotspot_temperature().ok()).map(|v| v as f32),
            fan_rpm: m.fan_speed().ok().map(|v| v.max(0) as u32),
            ..Default::default()
        })
    }
}

// --- read-only feature listing (`keel egpu amd-features`) ---

/// Every ADLX feature interface starts with Acquire, Release, QueryInterface, IsSupported and
/// (for the toggles) IsEnabled, so one vtable prefix covers all of them.
#[repr(C)]
struct FeatVtbl {
    _acquire: unsafe extern "C" fn(*mut c_void) -> i32,
    release: unsafe extern "C" fn(*mut c_void) -> i32,
    qi: unsafe extern "C" fn(*mut c_void, *const u16, *mut *mut c_void) -> ffi::ADLX_RESULT,
    is_supported: unsafe extern "C" fn(*mut c_void, *mut u8) -> ffi::ADLX_RESULT,
    is_enabled: unsafe extern "C" fn(*mut c_void, *mut u8) -> ffi::ADLX_RESULT,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Feature {
    pub scope: &'static str,
    pub name: &'static str,
    pub supported: bool,
    /// None for settings that are not on/off (Scaling Mode, VSync mode) or when unsupported.
    pub enabled: Option<bool>,
}

unsafe fn vt(p: *mut c_void) -> &'static FeatVtbl { unsafe { &**(p as *mut *const FeatVtbl) } }

/// Reads a feature object returned by `get` (a Get* call on a services interface).
unsafe fn feature(scope: &'static str, name: &'static str, toggle: bool, get: impl FnOnce(*mut *mut c_void) -> ffi::ADLX_RESULT) -> Feature {
    let mut p = std::ptr::null_mut();
    let mut f = Feature { scope, name, supported: false, enabled: None };
    if get(&mut p) != ffi::ADLX_RESULT_ADLX_OK || p.is_null() { return f; }
    unsafe {
        let v = vt(p);
        let (mut s, mut e) = (0u8, 0u8);
        f.supported = (v.is_supported)(p, &mut s) == 0 && s != 0;
        if toggle && f.supported && (v.is_enabled)(p, &mut e) == 0 { f.enabled = Some(e != 0); }
        (v.release)(p);
    }
    f
}

unsafe fn qi(p: *mut c_void, iid: &str) -> Option<*mut c_void> {
    let w: Vec<u16> = iid.encode_utf16().chain([0]).collect();
    let mut out = std::ptr::null_mut();
    (unsafe { (vt(p).qi)(p, w.as_ptr(), &mut out) } == 0 && !out.is_null()).then_some(out)
}

/// 3D settings (per GPU and global) and the display settings of displays on the eGPU.
/// Must run in the user session: in session 0 the 3D settings calls never return.
pub fn features(p: &GpuProfile) -> Result<(String, Vec<Feature>)> {
    let adlx = Adlx::open(p)?;
    let mut out = Vec::new();
    let gr = adlx.gpu().as_raw();
    if let Ok(t) = adlx.system().get_3d_settings_services() {
        let t: *mut ffi::IADLX3DSettingsServices = t.as_raw();
        unsafe {
            let v = &*(*t).pVtbl;
            out.push(feature("3D", "Anti-Lag", true, |o| (v.GetAntiLag.unwrap())(t, gr, o.cast())));
            out.push(feature("3D", "Chill", true, |o| (v.GetChill.unwrap())(t, gr, o.cast())));
            out.push(feature("3D", "Boost", true, |o| (v.GetBoost.unwrap())(t, gr, o.cast())));
            out.push(feature("3D", "Image Sharpening", true, |o| (v.GetImageSharpening.unwrap())(t, gr, o.cast())));
            out.push(feature("3D", "Enhanced Sync", true, |o| (v.GetEnhancedSync.unwrap())(t, gr, o.cast())));
            out.push(feature("3D", "Frame Rate Target", true, |o| (v.GetFrameRateTargetControl.unwrap())(t, gr, o.cast())));
            out.push(feature("3D", "Wait for VSync", false, |o| (v.GetWaitForVerticalRefresh.unwrap())(t, gr, o.cast())));
            out.push(feature("3D global", "Radeon Super Resolution", true, |o| (v.GetRadeonSuperResolution.unwrap())(t, o.cast())));
            if let Some(t1) = qi(t.cast(), "IADLX3DSettingsServices1") {
                let t1: *mut ffi::IADLX3DSettingsServices1 = t1.cast();
                out.push(feature("3D global", "Fluid Motion Frames", true, |o| ((*(*t1).pVtbl).GetAMDFluidMotionFrames.unwrap())(t1, o.cast())));
                (vt(t1.cast()).release)(t1.cast());
            }
        }
    }
    if let Ok(ds) = adlx.system().get_displays_services() {
        let d_raw: *mut ffi::IADLXDisplayServices = ds.as_raw();
        if let Ok(list) = ds.get_displays() {
            for d in list.iter() {
                let dr: *mut ffi::IADLXDisplay = d.as_raw();
                unsafe {
                    let v = &*(*d_raw).pVtbl;
                    out.push(feature("Display", "Integer Scaling", true, |o| (v.GetIntegerScaling.unwrap())(d_raw, dr, o.cast())));
                    out.push(feature("Display", "GPU Scaling", true, |o| (v.GetGPUScaling.unwrap())(d_raw, dr, o.cast())));
                    out.push(feature("Display", "Scaling Mode", false, |o| (v.GetScalingMode.unwrap())(d_raw, dr, o.cast())));
                    out.push(feature("Display", "Virtual Super Resolution", true, |o| (v.GetVirtualSuperResolution.unwrap())(d_raw, dr, o.cast())));
                    out.push(feature("Display", "FreeSync", true, |o| (v.GetFreeSync.unwrap())(d_raw, dr, o.cast())));
                }
            }
        }
    }
    let name = adlx.gpu().name().unwrap_or("AMD GPU").to_string();
    Ok((name, out))
}

/// GPU Scaling / Integer Scaling objects: IsSupported, IsEnabled, SetEnabled after the IADLX header.
#[repr(C)]
struct ToggleVtbl {
    _acquire: unsafe extern "C" fn(*mut c_void) -> i32,
    release: unsafe extern "C" fn(*mut c_void) -> i32,
    _qi: unsafe extern "C" fn(*mut c_void, *const u16, *mut *mut c_void) -> ffi::ADLX_RESULT,
    is_supported: unsafe extern "C" fn(*mut c_void, *mut u8) -> ffi::ADLX_RESULT,
    is_enabled: unsafe extern "C" fn(*mut c_void, *mut u8) -> ffi::ADLX_RESULT,
    set_enabled: unsafe extern "C" fn(*mut c_void, u8) -> ffi::ADLX_RESULT,
}

/// Scaling Mode: IsSupported, GetMode, SetMode (ADLX_SCALE_MODE: 0 aspect, 1 full panel, 2 centered).
#[repr(C)]
struct ModeVtbl {
    _acquire: unsafe extern "C" fn(*mut c_void) -> i32,
    release: unsafe extern "C" fn(*mut c_void) -> i32,
    _qi: unsafe extern "C" fn(*mut c_void, *const u16, *mut *mut c_void) -> ffi::ADLX_RESULT,
    is_supported: unsafe extern "C" fn(*mut c_void, *mut u8) -> ffi::ADLX_RESULT,
    get_mode: unsafe extern "C" fn(*mut c_void, *mut i32) -> ffi::ADLX_RESULT,
    set_mode: unsafe extern "C" fn(*mut c_void, i32) -> ffi::ADLX_RESULT,
}

/// Scaling settings of one eGPU display; None = not supported (so nothing to restore).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Scaling {
    pub display: String,
    pub gpu_scaling: Option<bool>,
    pub scaling_mode: Option<i32>,
    pub integer_scaling: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalingWrite { GpuScaling(bool), Mode(i32), Integer(bool) }

/// Writes that bring `now` back to `before`: Integer Scaling first (it depends on GPU Scaling),
/// then the mode, then GPU Scaling. Only settings that were supported before and differ now.
pub fn restore_plan(before: &Scaling, now: &Scaling) -> Vec<ScalingWrite> {
    let mut w = Vec::new();
    // Integer Scaling may only have become supported through GPU Scaling: then "before" is off.
    if let Some(n) = now.integer_scaling { if n != before.integer_scaling.unwrap_or(false) { w.push(ScalingWrite::Integer(!n)); } }
    if let (Some(b), Some(n)) = (before.scaling_mode, now.scaling_mode) { if b != n { w.push(ScalingWrite::Mode(b)); } }
    if let (Some(b), Some(n)) = (before.gpu_scaling, now.gpu_scaling) { if b != n { w.push(ScalingWrite::GpuScaling(b)); } }
    w
}

/// Display services + one display, for reading and writing its scaling objects.
struct Disp { ds: *mut ffi::IADLXDisplayServices, d: *mut ffi::IADLXDisplay }

impl Disp {
    unsafe fn name(&self) -> String {
        let mut p: *const std::os::raw::c_char = std::ptr::null();
        unsafe {
            if ((*(*self.d).pVtbl).Name.unwrap())(self.d, &mut p) == ffi::ADLX_RESULT_ADLX_OK && !p.is_null() {
                return std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
            }
        }
        "display".into()
    }
    unsafe fn obj(&self, which: u8) -> Option<*mut c_void> {
        let mut p: *mut c_void = std::ptr::null_mut();
        let r = unsafe {
            let v = &*(*self.ds).pVtbl;
            match which {
                0 => (v.GetGPUScaling.unwrap())(self.ds, self.d, (&mut p as *mut *mut c_void).cast()),
                1 => (v.GetScalingMode.unwrap())(self.ds, self.d, (&mut p as *mut *mut c_void).cast()),
                _ => (v.GetIntegerScaling.unwrap())(self.ds, self.d, (&mut p as *mut *mut c_void).cast()),
            }
        };
        (r == ffi::ADLX_RESULT_ADLX_OK && !p.is_null()).then_some(p)
    }
    unsafe fn toggle(&self, which: u8) -> Option<bool> {
        let p = unsafe { self.obj(which) }?;
        unsafe {
            let v = &**(p as *mut *const ToggleVtbl);
            let (mut s, mut e) = (0u8, 0u8);
            let out = ((v.is_supported)(p, &mut s) == 0 && s != 0 && (v.is_enabled)(p, &mut e) == 0).then_some(e != 0);
            (v.release)(p);
            out
        }
    }
    unsafe fn mode(&self) -> Option<i32> {
        let p = unsafe { self.obj(1) }?;
        unsafe {
            let v = &**(p as *mut *const ModeVtbl);
            let (mut s, mut m) = (0u8, 0i32);
            let out = ((v.is_supported)(p, &mut s) == 0 && s != 0 && (v.get_mode)(p, &mut m) == 0).then_some(m);
            (v.release)(p);
            out
        }
    }
    unsafe fn read(&self) -> Scaling {
        unsafe { Scaling { display: self.name(), gpu_scaling: self.toggle(0), scaling_mode: self.mode(), integer_scaling: self.toggle(2) } }
    }
    unsafe fn write(&self, w: ScalingWrite) -> ffi::ADLX_RESULT {
        let which = match w { ScalingWrite::GpuScaling(_) => 0, ScalingWrite::Mode(_) => 1, ScalingWrite::Integer(_) => 2 };
        let Some(p) = (unsafe { self.obj(which) }) else { return ffi::ADLX_RESULT_ADLX_NOT_SUPPORTED };
        unsafe {
            let r = match w {
                ScalingWrite::GpuScaling(on) | ScalingWrite::Integer(on) => ((**(p as *mut *const ToggleVtbl)).set_enabled)(p, on as u8),
                ScalingWrite::Mode(m) => ((**(p as *mut *const ModeVtbl)).set_mode)(p, m),
            };
            ((**(p as *mut *const ToggleVtbl)).release)(p);
            r
        }
    }
}

/// Runs `f` on every display ADLX lists for the eGPU (user session only).
fn with_displays<T>(p: &GpuProfile, mut f: impl FnMut(&Disp) -> T) -> Result<Vec<T>> {
    let adlx = Adlx::open(p)?;
    let ds = adlx.system().get_displays_services().map_err(|e| anyhow!("display services: {e}"))?;
    let list = ds.get_displays().map_err(|e| anyhow!("displays: {e}"))?;
    let out = list.iter().map(|d| f(&Disp { ds: ds.as_raw(), d: d.as_raw() })).collect();
    Ok(out)
}

/// The AMD GPU driving the built-in screen (the iGPU), as a profile for the scaling calls.
pub fn igpu_profile() -> Result<GpuProfile> {
    let adapters = crate::gpu::adapters()?;
    let shown = crate::display::active_displays();
    let a = adapters.iter().find(|a| a.vendor_id == crate::gpu::VENDOR_AMD && shown.iter().any(|d| d.internal && d.adapter_luid == a.luid))
        .or_else(|| adapters.iter().find(|a| a.vendor_id == crate::gpu::VENDOR_AMD))
        .ok_or_else(|| anyhow!("no AMD GPU"))?;
    Ok(GpuProfile { name: a.name.clone(), vendor_id: a.vendor_id, device_id: a.device_id, ..Default::default() })
}

pub fn scaling_status(p: &GpuProfile) -> Result<Vec<Scaling>> {
    with_displays(p, |d| unsafe { d.read() })
}

fn result_text(r: ffi::ADLX_RESULT) -> String {
    if r == ffi::ADLX_RESULT_ADLX_OK { "ok".into() } else { format!("ADLX result {r}") }
}

/// GPU Scaling on, then Integer Scaling on if the driver then reports it as supported. Returns the
/// values before any change (per display) and a line per step.
/// `only` = ADLX display name (the monitor's EDID name); None = every display on the eGPU.
pub fn scaling_on(p: &GpuProfile, only: Option<&str>) -> Result<(Vec<Scaling>, Vec<String>)> {
    let mut log = Vec::new();
    let before = with_displays(p, |d| unsafe {
        let b = d.read();
        if only.is_some_and(|o| o != b.display) { return (None, Vec::new()); }
        let mut lines = vec![format!("{}: before {:?}", b.display, b)];
        if b.gpu_scaling == Some(false) {
            lines.push(format!("{}: GPU Scaling on -> {}", b.display, result_text(d.write(ScalingWrite::GpuScaling(true)))));
        }
        let mid = d.read();
        lines.push(format!("{}: after GPU Scaling {:?}", mid.display, mid));
        match mid.integer_scaling {
            Some(false) => lines.push(format!("{}: Integer Scaling on -> {}", mid.display, result_text(d.write(ScalingWrite::Integer(true))))),
            Some(true) => lines.push(format!("{}: Integer Scaling already on", mid.display)),
            None => lines.push(format!("{}: Integer Scaling still not supported", mid.display)),
        }
        lines.push(format!("{}: now {:?}", mid.display, d.read()));
        (Some(b), lines)
    })?;
    let mut out = Vec::new();
    for (b, l) in before { out.extend(b); log.extend(l); }
    Ok((out, log))
}

/// One scaling write on one display (ADLX display name), with the result and the values after.
pub fn scaling_write(p: &GpuProfile, monitor: &str, w: ScalingWrite) -> Result<Vec<String>> {
    let lines = with_displays(p, |d| unsafe {
        let now = d.read();
        if now.display != monitor { return Vec::new(); }
        vec![format!("{monitor}: {w:?} -> {}", result_text(d.write(w))), format!("{monitor}: now {:?}", d.read())]
    })?.concat();
    if lines.is_empty() { return Err(anyhow!("ADLX does not list {monitor} on the eGPU")); }
    Ok(lines)
}

/// Puts each display back to its recorded values. Displays not in `saved` are left alone.
pub fn scaling_restore(p: &GpuProfile, saved: &[Scaling]) -> Result<Vec<String>> {
    let lines = with_displays(p, |d| unsafe {
        let now = d.read();
        let Some(b) = saved.iter().find(|s| s.display == now.display) else { return vec![format!("{}: no record, left alone", now.display)] };
        let mut l: Vec<String> = restore_plan(b, &now).into_iter()
            .map(|w| format!("{}: {w:?} -> {}", now.display, result_text(d.write(w)))).collect();
        l.push(format!("{}: now {:?}", now.display, d.read()));
        l
    })?;
    Ok(lines.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_undoes_only_what_changed_integer_first() {
        let s = |g, m, i| Scaling { display: "LG".into(), gpu_scaling: g, scaling_mode: m, integer_scaling: i };
        let before = s(Some(false), Some(0), None);
        assert_eq!(restore_plan(&before, &s(Some(true), Some(0), Some(true))),
                   vec![ScalingWrite::Integer(false), ScalingWrite::GpuScaling(false)]);
        assert_eq!(restore_plan(&before, &s(Some(true), Some(0), Some(false))), vec![ScalingWrite::GpuScaling(false)]);
        let before = s(Some(false), Some(0), Some(false));
        assert_eq!(restore_plan(&before, &s(Some(true), Some(1), Some(true))),
                   vec![ScalingWrite::Integer(false), ScalingWrite::Mode(0), ScalingWrite::GpuScaling(false)]);
        assert!(restore_plan(&before, &before).is_empty());
    }

    #[test]
    fn driver_dir_matches_vendor_and_device_only() {
        let keys = vec![
            (r"PCI\VEN_1002&DEV_15BF&REV_04".to_string(), r"C:\DS\u0202094\B026069\atidx9loader64.dll".to_string()),
            (r"pci\ven_1002&dev_7480&subsys_80431e50&rev_c7".to_string(), r"C:\DS\u0203731\B026470\atidx9loader64.dll".to_string()),
        ];
        assert_eq!(pick_driver_dir(&keys, 0x1002, 0x7480), Some(PathBuf::from(r"C:\DS\u0203731\B026470")));
        assert_eq!(pick_driver_dir(&keys, 0x1002, 0x15BF), Some(PathBuf::from(r"C:\DS\u0202094\B026069")));
        assert_eq!(pick_driver_dir(&keys, 0x10DE, 0x1E07), None);
    }

    #[test]
    fn driver_dir_skips_keys_without_user_mode_driver() {
        let keys = vec![(r"PCI\VEN_1002&DEV_7480".to_string(), String::new())];
        assert_eq!(pick_driver_dir(&keys, 0x1002, 0x7480), None);
    }
}

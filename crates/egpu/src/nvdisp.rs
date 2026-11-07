//! NVIDIA display scaling through NVAPI's display config (`NvAPI_DISP_GetDisplayConfig` /
//! `NvAPI_DISP_SetDisplayConfig`, nvapi.h R5xx). Integer scaling is the scaling value
//! `NV_SCALING_GPU_INTEGER_ASPECT_SCALING` (8) on the monitor's target. A set reads the whole
//! config, changes only that target's `scaling`, validates it (`VALIDATE_ONLY`) and then applies it.
//! nvapi64.dll is loaded from System32 only. User session; no admin needed.
use anyhow::{Result, anyhow, bail, ensure};
use std::ffi::c_void;
use std::sync::OnceLock;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::{HSTRING, s};

/// NV_SCALING values (nvapi.h).
pub const DEFAULT: i32 = 0;
/// Force GPU, aspect ratio: GPU scaling that fills the panel keeping the aspect (game hand-over).
pub const GPU_ASPECT: i32 = 5;
pub const INTEGER: i32 = 8;

pub fn scaling_name(v: i32) -> &'static str {
    match v {
        0 => "default",
        1 => "balanced, full screen",
        2 => "GPU, full screen",
        3 => "GPU, centered",
        5 => "GPU, aspect ratio",
        6 => "balanced, aspect ratio",
        7 => "balanced, centered",
        8 => "GPU, integer scaling",
        _ => "other",
    }
}

// --- nvapi.h structs (pack 8, the C default on x64) ---

#[repr(C)]
#[derive(Clone, Copy)]
struct TimingExt { flag: u32, rr: u16, rrx1k: u32, aspect: u32, rep: u16, status: u32, name: [u8; 40] }

#[repr(C)]
#[derive(Clone, Copy)]
struct Timing {
    h_visible: u16, h_border: u16, h_front_porch: u16, h_sync_width: u16, h_total: u16, h_sync_pol: u8,
    v_visible: u16, v_border: u16, v_front_porch: u16, v_sync_width: u16, v_total: u16, v_sync_pol: u8,
    interlaced: u16, pclk: u32, etc: TimingExt,
}

/// NV_DISPLAYCONFIG_PATH_ADVANCED_TARGET_INFO_V1
#[repr(C)]
#[derive(Clone, Copy)]
struct Advanced {
    version: u32, rotation: i32, scaling: i32, refresh_rate_1k: u32, flags: u32,
    connector: i32, tv_format: i32, timing_override: i32, timing: Timing,
}

/// NV_DISPLAYCONFIG_PATH_TARGET_INFO_V2
#[repr(C)]
struct Target { display_id: u32, details: *mut Advanced, target_id: u32 }

/// NV_DISPLAYCONFIG_SOURCE_MODE_INFO_V1
#[repr(C)]
#[derive(Clone, Copy)]
struct SourceMode { w: u32, h: u32, depth: u32, color_format: i32, x: i32, y: i32, spanning: i32, flags: u32 }

/// NV_DISPLAYCONFIG_PATH_INFO_V2
#[repr(C)]
struct Path {
    version: u32, source_id: u32, target_count: u32, targets: *mut Target,
    source_mode: *mut SourceMode, flags: u32, os_adapter: *mut c_void,
}

const fn version<T>(v: u32) -> u32 { std::mem::size_of::<T>() as u32 | (v << 16) }
const VALIDATE_ONLY: u32 = 1;

type QueryFn = unsafe extern "C" fn(u32) -> *const c_void;
type InitFn = unsafe extern "C" fn() -> i32;
type ErrFn = unsafe extern "C" fn(i32, *mut u8) -> i32;
type IdByNameFn = unsafe extern "C" fn(*const u8, *mut u32) -> i32;
type GetCfgFn = unsafe extern "C" fn(*mut u32, *mut Path) -> i32;
type SetCfgFn = unsafe extern "C" fn(u32, *mut Path, u32) -> i32;

struct Api { err: ErrFn, id_by_name: IdByNameFn, get: GetCfgFn, set: SetCfgFn }

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let h = LoadLibraryExW(&HSTRING::from("nvapi64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| format!("nvapi64.dll: {e}"))?;
        let q: QueryFn = std::mem::transmute(GetProcAddress(h, s!("nvapi_QueryInterface")).ok_or("nvapi_QueryInterface missing")?);
        let f = |id: u32, name: &str| { let p = q(id); if p.is_null() { Err(format!("NVAPI has no {name}")) } else { Ok(p) } };
        let init: InitFn = std::mem::transmute(f(0x0150e828, "NvAPI_Initialize")?);
        let api = Api {
            err: std::mem::transmute(f(0x6c2d048c, "NvAPI_GetErrorMessage")?),
            id_by_name: std::mem::transmute(f(0xae457190, "NvAPI_DISP_GetDisplayIdByDisplayName")?),
            get: std::mem::transmute(f(0x11abccf8, "NvAPI_DISP_GetDisplayConfig")?),
            set: std::mem::transmute(f(0x5d8cf8de, "NvAPI_DISP_SetDisplayConfig")?),
        };
        let st = init();
        if st != 0 { return Err(format!("NvAPI_Initialize: status {st}")); }
        Ok(api)
    }).as_ref().map_err(|e| anyhow!("{e}"))
}

fn check(a: &Api, what: &str, st: i32) -> Result<()> {
    if st == 0 { return Ok(()); }
    let mut buf = [0u8; 64];
    unsafe { (a.err)(st, buf.as_mut_ptr()); }
    let msg = String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(64)]).into_owned();
    bail!("{what}: {msg} ({st})")
}

/// The whole display config with owned buffers (the raw paths point into them).
struct Config {
    paths: Vec<Path>,
    _targets: Vec<Vec<Target>>,
    _details: Vec<Vec<Advanced>>,
    _modes: Vec<SourceMode>,
}

fn zeroed<T>() -> T { unsafe { std::mem::zeroed() } }

fn read_config(a: &Api) -> Result<Config> {
    unsafe {
        let mut n = 0u32;
        check(a, "GetDisplayConfig (count)", (a.get)(&mut n, std::ptr::null_mut()))?;
        ensure!(n > 0 && n < 64, "GetDisplayConfig: {n} paths");
        let mut paths: Vec<Path> = (0..n).map(|_| { let mut p: Path = zeroed(); p.version = version::<Path>(2); p }).collect();
        check(a, "GetDisplayConfig (targets)", (a.get)(&mut n, paths.as_mut_ptr()))?;
        paths.truncate(n as usize);
        let mut modes: Vec<SourceMode> = vec![zeroed(); paths.len()];
        let mut targets = Vec::new();
        let mut details = Vec::new();
        for (i, p) in paths.iter_mut().enumerate() {
            let c = p.target_count as usize;
            ensure!(c > 0 && c < 16, "GetDisplayConfig: {c} targets");
            let mut d: Vec<Advanced> = (0..c).map(|_| { let mut x: Advanced = zeroed(); x.version = version::<Advanced>(1); x }).collect();
            let mut t: Vec<Target> = (0..c).map(|j| Target { display_id: 0, details: d.as_mut_ptr().add(j), target_id: 0 }).collect();
            p.targets = t.as_mut_ptr();
            p.source_mode = modes.as_mut_ptr().add(i);
            targets.push(t);
            details.push(d);
        }
        check(a, "GetDisplayConfig", (a.get)(&mut n, paths.as_mut_ptr()))?;
        Ok(Config { paths, _targets: targets, _details: details, _modes: modes })
    }
}

fn display_id(a: &Api, gdi: &str) -> Result<u32> {
    let name = std::ffi::CString::new(gdi)?;
    let mut id = 0u32;
    check(a, &format!("display id of {gdi}"), unsafe { (a.id_by_name)(name.as_ptr() as *const u8, &mut id) })?;
    Ok(id)
}

/// Finds the target of `id` in the config.
fn target_of(cfg: &Config, id: u32) -> Option<*mut Advanced> {
    for p in &cfg.paths {
        for j in 0..p.target_count as usize {
            let t = unsafe { &*p.targets.add(j) };
            if t.display_id == id && !t.details.is_null() { return Some(t.details); }
        }
    }
    None
}

/// Current NV_SCALING of the monitor at `gdi` (e.g. `\\.\DISPLAY6`).
pub fn scaling(gdi: &str) -> Result<i32> {
    let a = api()?;
    let id = display_id(a, gdi)?;
    let cfg = read_config(a)?;
    let d = target_of(&cfg, id).ok_or_else(|| anyhow!("{gdi} is not in NVIDIA's display config"))?;
    Ok(unsafe { (*d).scaling })
}

/// Sets the monitor's NV_SCALING; validated first, then applied. Returns the value before.
pub fn set_scaling(gdi: &str, value: i32) -> Result<i32> {
    let a = api()?;
    let id = display_id(a, gdi)?;
    let mut cfg = read_config(a)?;
    let d = target_of(&cfg, id).ok_or_else(|| anyhow!("{gdi} is not in NVIDIA's display config"))?;
    let before = unsafe { (*d).scaling };
    if before == value { return Ok(before); }
    unsafe { (*d).scaling = value; }
    let n = cfg.paths.len() as u32;
    check(a, "SetDisplayConfig (validate)", unsafe { (a.set)(n, cfg.paths.as_mut_ptr(), VALIDATE_ONLY) })?;
    check(a, "SetDisplayConfig", unsafe { (a.set)(n, cfg.paths.as_mut_ptr(), 0) })?;
    Ok(before)
}

/// One line per NVIDIA path: display id, source mode, position, scaling (diagnostics).
pub fn describe() -> Result<Vec<String>> {
    let a = api()?;
    let cfg = read_config(a)?;
    let mut out = Vec::new();
    for p in &cfg.paths {
        let m = unsafe { &*p.source_mode };
        for j in 0..p.target_count as usize {
            let t = unsafe { &*p.targets.add(j) };
            let d = unsafe { &*t.details };
            out.push(format!("source {} {}x{} at {},{}{}  display 0x{:08x}  scaling {} ({})  refresh {}.{:03} Hz",
                p.source_id, m.w, m.h, m.x, m.y, if m.flags & 1 != 0 { " (primary)" } else { "" },
                t.display_id, d.scaling, scaling_name(d.scaling), d.refresh_rate_1k / 1000, d.refresh_rate_1k % 1000));
        }
    }
    Ok(out)
}

/// Display id of `gdi` (diagnostics).
pub fn id_of(gdi: &str) -> Result<u32> { display_id(api()?, gdi) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_nvapi_h() {
        // Sizes from nvapi.h's layout (pack 8): NV_TIMINGEXT 64, NV_TIMING 96,
        // ADVANCED_TARGET_INFO_V1 128, PATH_TARGET_INFO_V2 24, SOURCE_MODE_INFO_V1 32, PATH_INFO_V2 48.
        // NVAPI checks the size in each `version` too and refuses a mismatch.
        assert_eq!(std::mem::size_of::<TimingExt>(), 64);
        assert_eq!(std::mem::size_of::<Timing>(), 96);
        assert_eq!(std::mem::size_of::<Advanced>(), 128);
        assert_eq!(std::mem::size_of::<Target>(), 24);
        assert_eq!(std::mem::size_of::<SourceMode>(), 32);
        assert_eq!(std::mem::size_of::<Path>(), 48);
    }
}

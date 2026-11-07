//! NVIDIA 3D settings (NVIDIA Control Panel > Manage 3D settings > Global) through NVAPI's driver
//! settings (DRS): the base profile's Max Frame Rate, Low Latency (max pre-rendered frames 1),
//! Vertical sync and Power management mode. Only these settings, only on the base profile. "Default"
//! writes the driver's default value: deleting a setting from the base profile left the NVIDIA
//! Control Panel unable to open. These settings belong to the
//! NVIDIA driver, so they only ever affect NVIDIA GPUs (here: the eGPU). nvapi64.dll from System32.
use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;
use std::sync::OnceLock;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::{HSTRING, s};

/// A setting Keel offers: name, driver id, the values it may take (label, value).
pub struct Def { pub name: &'static str, pub id: u32, pub choices: &'static [(&'static str, u32)], pub numeric: bool, pub default: u32 }

pub const FRL_FPS: u32 = 0x1083_5002;
pub const PRERENDER: u32 = 0x007B_A09E;
pub const VSYNC: u32 = 0x00A8_79CF;
pub const PSTATE: u32 = 0x1057_EB71;

pub const SETTINGS: &[Def] = &[
    Def { name: "Max Frame Rate", id: FRL_FPS, choices: &[], numeric: true, default: 0 },
    Def { name: "Low Latency", id: PRERENDER, choices: &[("Off", 0), ("On", 1)], numeric: false, default: 0 },
    Def { name: "Vertical sync", id: VSYNC, choices: &[("Use the app's setting", 0x6092_5292), ("Off", 0x0841_6747), ("On", 0x4781_4940)], numeric: false, default: 0x6092_5292 },
    Def { name: "Power management", id: PSTATE, choices: &[("Prefer maximum performance", 1), ("Adaptive", 0), ("Optimal power", 5)], numeric: false, default: 5 },
];

/// Pure: whether `value` is allowed for setting `id` (the service checks every request with it).
pub fn allowed(id: u32, value: u32) -> bool {
    SETTINGS.iter().find(|d| d.id == id).is_some_and(|d| if d.numeric { value <= 1000 } else { d.choices.iter().any(|(_, v)| *v == value) })
}

// NVDRS_SETTING_V1 (nvapi.h): 12320 bytes.
#[repr(C)]
struct Setting {
    version: u32,
    name: [u16; 2048],
    id: u32,
    kind: u32,
    location: u32,
    is_current_predefined: u32,
    is_predefined_valid: u32,
    predefined: [u8; 4100],
    current: [u8; 4100],
}
const SETTING_VER: u32 = std::mem::size_of::<Setting>() as u32 | (1 << 16);
const NOT_FOUND: i32 = -160;

type QueryFn = unsafe extern "C" fn(u32) -> *const c_void;
type InitFn = unsafe extern "C" fn() -> i32;
type SessionFn = unsafe extern "C" fn(*mut *mut c_void) -> i32;
type HandleFn = unsafe extern "C" fn(*mut c_void) -> i32;
type BaseFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> i32;
type GetFn = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut Setting) -> i32;
type SetFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut Setting) -> i32;

struct Api { create: SessionFn, destroy: HandleFn, load: HandleFn, save: HandleFn, base: BaseFn, get: GetFn, set: SetFn }

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let h = LoadLibraryExW(&HSTRING::from("nvapi64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).map_err(|e| format!("nvapi64.dll: {e}"))?;
        let q: QueryFn = std::mem::transmute(GetProcAddress(h, s!("nvapi_QueryInterface")).ok_or("nvapi_QueryInterface missing")?);
        let f = |id: u32, name: &str| { let p = q(id); if p.is_null() { Err(format!("NVAPI has no {name}")) } else { Ok(p) } };
        let init: InitFn = std::mem::transmute(f(0x0150_e828, "NvAPI_Initialize")?);
        let api = Api {
            create: std::mem::transmute(f(0x0694_d52e, "NvAPI_DRS_CreateSession")?),
            destroy: std::mem::transmute(f(0xdad9_cff8, "NvAPI_DRS_DestroySession")?),
            load: std::mem::transmute(f(0x375d_bd6b, "NvAPI_DRS_LoadSettings")?),
            save: std::mem::transmute(f(0xfcbc_7e14, "NvAPI_DRS_SaveSettings")?),
            base: std::mem::transmute(f(0xda84_66a0, "NvAPI_DRS_GetBaseProfile")?),
            get: std::mem::transmute(f(0x73bf_8338, "NvAPI_DRS_GetSetting")?),
            set: std::mem::transmute(f(0x577d_d202, "NvAPI_DRS_SetSetting")?),
        };
        let st = init();
        if st != 0 { return Err(format!("NvAPI_Initialize: status {st} (no NVIDIA driver?)")); }
        Ok(api)
    }).as_ref().map_err(|e| anyhow!("{e}"))
}

/// A loaded DRS session on the base profile, destroyed when dropped.
struct Session { a: &'static Api, h: *mut c_void, base: *mut c_void }

impl Session {
    fn open() -> Result<Self> {
        let a = api()?;
        let mut h = std::ptr::null_mut();
        let st = unsafe { (a.create)(&mut h) };
        if st != 0 { bail!("DRS session: status {st}"); }
        // Owns the handle from here on: an early return destroys it once.
        let mut s = Session { a, h, base: std::ptr::null_mut() };
        let st = unsafe { (a.load)(h) };
        if st != 0 { bail!("DRS load: status {st}"); }
        let st = unsafe { (a.base)(h, &mut s.base) };
        if st != 0 { bail!("DRS base profile: status {st}"); }
        Ok(s)
    }
}

impl Drop for Session { fn drop(&mut self) { unsafe { (self.a.destroy)(self.h); } } }

/// One setting as the driver reports it: its own name, Keel's value or None (driver default).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Current { pub id: u32, pub label: String, pub driver_name: String, pub value: Option<u32> }

/// The offered settings on the base profile.
pub fn read() -> Result<Vec<Current>> {
    let s = Session::open()?;
    let mut out = Vec::new();
    for d in SETTINGS {
        let mut v: Box<Setting> = unsafe { Box::new(std::mem::zeroed()) };
        v.version = SETTING_VER;
        let st = unsafe { (s.a.get)(s.h, s.base, d.id, &mut *v) };
        let (driver_name, value) = match st {
            0 => {
                let end = v.name.iter().position(|&c| c == 0).unwrap_or(v.name.len());
                let cur = u32::from_le_bytes(v.current[..4].try_into().unwrap());
                (String::from_utf16_lossy(&v.name[..end]), (v.is_current_predefined == 0).then_some(cur))
            }
            NOT_FOUND => (String::new(), None),
            other => bail!("DRS get {:#x}: status {other}", d.id),
        };
        out.push(Current { id: d.id, label: d.name.to_string(), driver_name, value });
    }
    Ok(out)
}

/// Sets `id` to `value` on the base profile, or to the driver's default value (`None`). Saved at once.
/// Writing the driver's settings store needs administrator rights (the Keel service does it).
pub fn set(id: u32, value: Option<u32>) -> Result<()> {
    let def = SETTINGS.iter().find(|d| d.id == id).ok_or_else(|| anyhow!("setting {id:#x} is not offered"))?;
    let v = value.unwrap_or(def.default);
    if !allowed(id, v) { bail!("value {v} is not offered for setting {id:#x}"); }
    let s = Session::open()?;
    let mut x: Box<Setting> = unsafe { Box::new(std::mem::zeroed()) };
    x.version = SETTING_VER;
    x.id = id;
    x.kind = 0; // NVDRS_DWORD_TYPE
    x.current[..4].copy_from_slice(&v.to_le_bytes());
    let st = unsafe { (s.a.set)(s.h, s.base, &mut *x) };
    if st != 0 { bail!("DRS set {id:#x}: status {st}"); }
    let st = unsafe { (s.a.save)(s.h) };
    if st != 0 { bail!("DRS save: status {st}"); }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn layout_and_allowed_values() {
        assert_eq!(std::mem::size_of::<super::Setting>(), 12320);
        assert!(super::allowed(super::FRL_FPS, 141));
        assert!(!super::allowed(super::FRL_FPS, 5000));
        assert!(super::allowed(super::VSYNC, 0x0841_6747));
        assert!(!super::allowed(super::VSYNC, 7));
        assert!(!super::allowed(0x1234, 1), "only the offered settings");
        assert!(super::SETTINGS.iter().all(|d| super::allowed(d.id, d.default)), "defaults are writable values");
    }
}

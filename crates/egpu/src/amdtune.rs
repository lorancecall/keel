//! AMD eGPU tuning and Radeon features through ADLX (the eGPU driver's own copy, `amd::Adlx`):
//! power limit (% of stock), maximum GPU clock, reset to factory, and the on/off Radeon features
//! (Anti-Lag, Chill, Boost, Image Sharpening, Enhanced Sync, Radeon Super Resolution, Fluid Motion
//! Frames). Must run in the user session (in session 0 the 3D settings calls never return), so the
//! CLI (`keel egpu amd-tune`, `keel egpu amd-feature`) does it for the main window and the tray.
use crate::amd::Adlx;
use crate::config::GpuProfile;
use adlx_lite::{ffi, Interface};
use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;

/// A value with its allowed range (min, max, step) and the current setting.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Ranged { pub min: i32, pub max: i32, pub step: i32, pub now: i32 }

/// What the eGPU offers and how it is set now.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Tuning {
    pub gpu: String,
    pub at_factory: Option<bool>,
    /// Power limit in % (0 = stock, e.g. -10..+15).
    pub power: Option<Ranged>,
    /// Maximum GPU clock in MHz.
    pub max_mhz: Option<Ranged>,
    pub features: Vec<Toggle>,
    /// Which tuning kinds the driver offers for this GPU (auto, preset, manual clocks / VRAM / fan / power).
    pub offers: Vec<String>,
    /// Features the driver refused to switch on for this GPU; left out of `features`.
    #[serde(default)]
    pub refused: Vec<String>,
}

/// The message `set_feature` fails with when the driver will not switch a feature on.
pub const REFUSED: &str = "the driver refused";

/// Leaves the refused features out of the list and names them. Pure; tested.
pub fn hide_refused(t: &mut Tuning, refused: &std::collections::BTreeSet<String>) {
    t.features.retain(|f| !refused.contains(&f.name));
    t.refused = refused.iter().cloned().collect();
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Toggle { pub name: String, pub on: bool }

/// The on/off features this module can switch, as named in the CLI and the main window.
pub const FEATURES: &[&str] = &["Anti-Lag", "Chill", "Boost", "Image Sharpening", "Enhanced Sync", "Radeon Super Resolution", "Fluid Motion Frames"];

fn ok(r: ffi::ADLX_RESULT) -> bool { r == ffi::ADLX_RESULT_ADLX_OK }

/// ADLX's system interface (adlx-lite keeps the pointer private; `System` is that one pointer).
fn system(a: &Adlx) -> *mut ffi::IADLXSystem { unsafe { std::mem::transmute_copy(a.system()) } }

unsafe fn release(p: *mut c_void) {
    // Every ADLX interface starts with Acquire, Release.
    #[repr(C)] struct Head { _a: unsafe extern "C" fn(*mut c_void) -> i32, r: unsafe extern "C" fn(*mut c_void) -> i32 }
    if !p.is_null() { unsafe { ((**(p as *mut *const Head)).r)(p); } }
}

unsafe fn qi(p: *mut c_void, iid: &str) -> Option<*mut c_void> {
    #[repr(C)] struct Head {
        _a: unsafe extern "C" fn(*mut c_void) -> i32, _r: unsafe extern "C" fn(*mut c_void) -> i32,
        q: unsafe extern "C" fn(*mut c_void, *const u16, *mut *mut c_void) -> ffi::ADLX_RESULT,
    }
    let w: Vec<u16> = iid.encode_utf16().chain([0]).collect();
    let mut out = std::ptr::null_mut();
    (unsafe { ((**(p as *mut *const Head)).q)(p, w.as_ptr(), &mut out) } == 0 && !out.is_null()).then_some(out)
}

/// The GPU tuning services, released when dropped.
struct Services(*mut ffi::IADLXGPUTuningServices);
impl Drop for Services { fn drop(&mut self) { unsafe { release(self.0.cast()) } } }

fn services(a: &Adlx) -> Result<Services> {
    let s = system(a);
    let mut t = std::ptr::null_mut();
    if !ok(unsafe { ((*(*s).pVtbl).GetGPUTuningServices.unwrap())(s, &mut t) }) || t.is_null() { bail!("GPU tuning is not available"); }
    Ok(Services(t))
}

/// Power tuning of the GPU, if the driver offers it.
unsafe fn power(t: *mut ffi::IADLXGPUTuningServices, gpu: *mut ffi::IADLXGPU) -> Option<*mut ffi::IADLXManualPowerTuning> {
    unsafe {
        let v = &*(*t).pVtbl;
        let mut s = 0u8;
        if !ok((v.IsSupportedManualPowerTuning.unwrap())(t, gpu, &mut s)) || s == 0 { return None; }
        let mut p = std::ptr::null_mut();
        (ok((v.GetManualPowerTuning.unwrap())(t, gpu, &mut p)) && !p.is_null()).then(|| p.cast())
    }
}

/// Clock tuning of the GPU (the RDNA interface with min / max frequency), if offered.
unsafe fn clocks(t: *mut ffi::IADLXGPUTuningServices, gpu: *mut ffi::IADLXGPU) -> Option<*mut ffi::IADLXManualGraphicsTuning2> {
    unsafe {
        let v = &*(*t).pVtbl;
        let mut s = 0u8;
        if !ok((v.IsSupportedManualGFXTuning.unwrap())(t, gpu, &mut s)) || s == 0 { return None; }
        let mut p = std::ptr::null_mut();
        if !ok((v.GetManualGFXTuning.unwrap())(t, gpu, &mut p)) || p.is_null() { return None; }
        let two = qi(p.cast(), "IADLXManualGraphicsTuning2");
        release(p.cast());
        two.map(|x| x.cast())
    }
}

fn range(r: ffi::ADLX_IntRange, now: i32) -> Ranged { Ranged { min: r.minValue, max: r.maxValue, step: r.step.max(1), now } }

/// Reads the toggle `name` (supported and on?) or sets it when `set` is given.
unsafe fn toggle(a: &Adlx, name: &str, set: Option<bool>) -> Result<Option<bool>> {
    let s = a.system().get_3d_settings_services().map_err(|e| anyhow!("3D settings: {e:?}"))?;
    let t: *mut ffi::IADLX3DSettingsServices = s.as_raw();
    let gpu: *mut ffi::IADLXGPU = a.gpu().as_raw();
    unsafe {
        let v = &*(*t).pVtbl;
        // Each feature: get its interface, then IsSupported / IsEnabled / SetEnabled from its own vtable.
        macro_rules! feat {
            ($get:expr, $ty:ty) => {{
                let mut p: *mut $ty = std::ptr::null_mut();
                if !ok($get(&mut p)) || p.is_null() { return Ok(None); }
                let fv = &*(*p).pVtbl;
                let (mut sup, mut on) = (0u8, 0u8);
                let r = if !ok((fv.IsSupported.unwrap())(p, &mut sup)) || sup == 0 { Ok(None) }
                    else if let Some(want) = set {
                        if ok((fv.SetEnabled.unwrap())(p, want as u8)) { Ok(Some(want)) } else { Err(anyhow!("{name}: {REFUSED}")) }
                    } else if ok((fv.IsEnabled.unwrap())(p, &mut on)) { Ok(Some(on != 0)) } else { Ok(None) };
                release(p.cast());
                r
            }};
        }
        match name {
            "Anti-Lag" => feat!(|o: *mut *mut ffi::IADLX3DAntiLag| (v.GetAntiLag.unwrap())(t, gpu, o), ffi::IADLX3DAntiLag),
            "Chill" => feat!(|o: *mut *mut ffi::IADLX3DChill| (v.GetChill.unwrap())(t, gpu, o), ffi::IADLX3DChill),
            "Boost" => feat!(|o: *mut *mut ffi::IADLX3DBoost| (v.GetBoost.unwrap())(t, gpu, o), ffi::IADLX3DBoost),
            "Image Sharpening" => feat!(|o: *mut *mut ffi::IADLX3DImageSharpening| (v.GetImageSharpening.unwrap())(t, gpu, o), ffi::IADLX3DImageSharpening),
            "Enhanced Sync" => feat!(|o: *mut *mut ffi::IADLX3DEnhancedSync| (v.GetEnhancedSync.unwrap())(t, gpu, o), ffi::IADLX3DEnhancedSync),
            "Radeon Super Resolution" => feat!(|o: *mut *mut ffi::IADLX3DRadeonSuperResolution| (v.GetRadeonSuperResolution.unwrap())(t, o), ffi::IADLX3DRadeonSuperResolution),
            "Fluid Motion Frames" => {
                let Some(t1) = qi(t.cast(), "IADLX3DSettingsServices1") else { return Ok(None) };
                let t1: *mut ffi::IADLX3DSettingsServices1 = t1.cast();
                let r = (|| feat!(|o: *mut *mut ffi::IADLX3DAMDFluidMotionFrames| ((*(*t1).pVtbl).GetAMDFluidMotionFrames.unwrap())(t1, o), ffi::IADLX3DAMDFluidMotionFrames))();
                release(t1.cast());
                r
            }
            other => bail!("unknown feature {other:?} (one of: {})", FEATURES.join(", ")),
        }
    }
}

/// Everything the eGPU offers, as set now.
pub fn read(p: &GpuProfile) -> Result<Tuning> {
    let a = Adlx::open(p)?;
    let mut out = Tuning { gpu: a.gpu().name().unwrap_or("AMD GPU").to_string(), ..Default::default() };
    let gpu: *mut ffi::IADLXGPU = a.gpu().as_raw();
    if let Ok(s) = services(&a) {
        unsafe {
            let v = &*(*s.0).pVtbl;
            let mut f = 0u8;
            if ok((v.IsAtFactory.unwrap())(s.0, gpu, &mut f)) { out.at_factory = Some(f != 0); }
            let kinds: [(&str, Option<unsafe extern "C" fn(*mut ffi::IADLXGPUTuningServices, *mut ffi::IADLXGPU, *mut u8) -> ffi::ADLX_RESULT>); 6] = [
                ("auto", v.IsSupportedAutoTuning), ("preset", v.IsSupportedPresetTuning), ("clocks", v.IsSupportedManualGFXTuning),
                ("vram", v.IsSupportedManualVRAMTuning), ("fan", v.IsSupportedManualFanTuning), ("power", v.IsSupportedManualPowerTuning),
            ];
            for (n, fnp) in kinds {
                let mut b = 0u8;
                if let Some(f) = fnp { if ok(f(s.0, gpu, &mut b)) && b != 0 { out.offers.push(n.to_string()); } }
            }
            if let Some(pw) = power(s.0, gpu) {
                let pv = &*(*pw).pVtbl;
                let (mut r, mut now) = (std::mem::zeroed::<ffi::ADLX_IntRange>(), 0);
                if ok((pv.GetPowerLimitRange.unwrap())(pw, &mut r)) && ok((pv.GetPowerLimit.unwrap())(pw, &mut now)) { out.power = Some(range(r, now)); }
                release(pw.cast());
            }
            if let Some(c) = clocks(s.0, gpu) {
                let cv = &*(*c).pVtbl;
                let (mut r, mut now) = (std::mem::zeroed::<ffi::ADLX_IntRange>(), 0);
                if ok((cv.GetGPUMaxFrequencyRange.unwrap())(c, &mut r)) && ok((cv.GetGPUMaxFrequency.unwrap())(c, &mut now)) { out.max_mhz = Some(range(r, now)); }
                release(c.cast());
            }
        }
    }
    for n in FEATURES {
        if let Ok(Some(on)) = unsafe { toggle(&a, n, None) } { out.features.push(Toggle { name: n.to_string(), on }); }
    }
    Ok(out)
}

/// Pure: clamps a wanted value into the range and onto its step.
pub fn fit(r: &Ranged, want: i32) -> i32 {
    let v = want.clamp(r.min, r.max);
    r.min + (v - r.min) / r.step * r.step
}

/// Sets the power limit (%) and / or the maximum clock (MHz); returns what was done.
pub fn set(p: &GpuProfile, power_pct: Option<i32>, max_mhz: Option<i32>) -> Result<Vec<String>> {
    let a = Adlx::open(p)?;
    let s = services(&a)?;
    let gpu: *mut ffi::IADLXGPU = a.gpu().as_raw();
    let mut done = Vec::new();
    unsafe {
        if let Some(want) = power_pct {
            let pw = power(s.0, gpu).ok_or_else(|| anyhow!("power tuning is not available on this GPU"))?;
            let pv = &*(*pw).pVtbl;
            let mut r = std::mem::zeroed::<ffi::ADLX_IntRange>();
            let res = if ok((pv.GetPowerLimitRange.unwrap())(pw, &mut r)) {
                let v = fit(&range(r, 0), want);
                if ok((pv.SetPowerLimit.unwrap())(pw, v)) { Ok(format!("power limit {v:+} %")) } else { Err(anyhow!("power limit {v}: the driver refused")) }
            } else { Err(anyhow!("power limit range unavailable")) };
            release(pw.cast());
            done.push(res?);
        }
        if let Some(want) = max_mhz {
            let c = clocks(s.0, gpu).ok_or_else(|| anyhow!("clock tuning is not available on this GPU"))?;
            let cv = &*(*c).pVtbl;
            let mut r = std::mem::zeroed::<ffi::ADLX_IntRange>();
            let res = if ok((cv.GetGPUMaxFrequencyRange.unwrap())(c, &mut r)) {
                let v = fit(&range(r, 0), want);
                if ok((cv.SetGPUMaxFrequency.unwrap())(c, v)) { Ok(format!("max clock {v} MHz")) } else { Err(anyhow!("max clock {v}: the driver refused")) }
            } else { Err(anyhow!("clock range unavailable")) };
            release(c.cast());
            done.push(res?);
        }
    }
    Ok(done)
}

/// Puts the GPU's tuning back to the driver's factory values.
pub fn reset(p: &GpuProfile) -> Result<()> {
    let a = Adlx::open(p)?;
    let s = services(&a)?;
    let gpu: *mut ffi::IADLXGPU = a.gpu().as_raw();
    if !ok(unsafe { ((*(*s.0).pVtbl).ResetToFactory.unwrap())(s.0, gpu) }) { bail!("the driver refused the reset"); }
    Ok(())
}

/// Switches one Radeon feature on or off.
pub fn set_feature(p: &GpuProfile, name: &str, on: bool) -> Result<()> {
    let a = Adlx::open(p)?;
    match unsafe { toggle(&a, name, Some(on))? } {
        // Some drivers accept the call and keep the feature off: that is a refusal too.
        Some(_) if on && unsafe { toggle(&a, name, None) }.ok().flatten() == Some(false) => bail!("{name}: {REFUSED}"),
        Some(_) => Ok(()),
        None => bail!("{name} is not supported on {}", a.gpu().name().unwrap_or("this GPU")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refused_features_are_hidden() {
        let f = |n: &str| super::Toggle { name: n.into(), on: false };
        let mut t = super::Tuning { features: vec![f("Chill"), f("Fluid Motion Frames")], ..Default::default() };
        super::hide_refused(&mut t, &["Fluid Motion Frames".to_string()].into());
        assert_eq!(t.features, [f("Chill")]);
        assert_eq!(t.refused, ["Fluid Motion Frames"]);
    }

    #[test]
    fn values_fit_the_range_and_step() {
        let r = super::Ranged { min: 500, max: 2700, step: 10, now: 2700 };
        assert_eq!(super::fit(&r, 2155), 2150);
        assert_eq!(super::fit(&r, 9000), 2700);
        assert_eq!(super::fit(&r, 0), 500);
        let p = super::Ranged { min: -10, max: 15, step: 1, now: 0 };
        assert_eq!(super::fit(&p, -20), -10);
    }
}

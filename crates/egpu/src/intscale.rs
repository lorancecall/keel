//! Integer scaling mode for eGPU monitors (tray, user session, opt-in per monitor): run the
//! monitor at an exact divisor of its native mode (3840x2160 -> 1920x1080 = 2x) and let the GPU
//! scale it up with whole pixels. Everything changed is recorded first in
//! `%LOCALAPPDATA%\keel\integer-scaling.json` and put back on "off". Each display mode
//! change costs dwm memory it does not give back, so modes change once per toggle or attach.
//! Vendor scaling switches sit behind `Scaler`; the built-in panel / iGPU is never a candidate.
use crate::amd::{self, Scaling};
use crate::config::GpuProfile;
use crate::display::Mode;
use crate::gpu::{VENDOR_AMD, VENDOR_NVIDIA};
use anyhow::Result;
use std::path::PathBuf;

/// Exact-divisor modes of `native`, smallest factor (largest mode) first. For each factor the
/// refresh rate `hz` is used when that mode has it, else the highest one it has.
pub fn candidates(native: Mode, modes: &[Mode], hz: u32) -> Vec<(u32, Mode)> {
    let mut out = Vec::new();
    for k in 2..=4u32 {
        if native.w % k != 0 || native.h % k != 0 { continue; }
        let (w, h) = (native.w / k, native.h / k);
        let same: Vec<&Mode> = modes.iter().filter(|m| m.w == w && m.h == h).collect();
        if let Some(m) = same.iter().find(|m| m.hz == hz).or_else(|| same.iter().max_by_key(|m| m.hz)) {
            out.push((k, **m));
        }
    }
    out
}

/// The native mode: the largest the driver lists, at its highest refresh rate.
pub fn native_of(modes: &[Mode]) -> Option<Mode> {
    modes.iter().copied().max_by_key(|m| (m.w as u64 * m.h as u64, m.w, m.hz))
}

/// The mode to put back on "off": the mode before switching on if it was the native size (keeps
/// its refresh rate), else the native mode at its highest refresh rate.
/// Windows scale for the integer mode, so things keep their size: the native scale divided by the
/// factor, rounded down to a scale Windows offers, at least 100% (4K 200% -> 1080 100%).
pub fn scale_for_factor(native_pct: u32, factor: u32) -> u32 {
    let want = native_pct / factor.max(1);
    crate::display::DPI_STEPS.iter().copied().filter(|&s| s <= want).max().unwrap_or(100)
}

/// Native-mode scale: the scale in use if the monitor was at its native size, else the integer
/// mode's scale times the factor (it was switched to a lower mode by hand before).
pub fn native_scale(before_is_native: bool, before_pct: u32, factor: u32) -> u32 {
    if before_is_native { before_pct } else { before_pct * factor }
}

/// Font smoothing while integer scaling is on: ClearType's colored subpixel edges turn into
/// visible 2x2 fringes when pixels are doubled, so ClearType (2) becomes grayscale (1). Anything
/// else (off, already grayscale) stays as it is.
pub fn smoothing_while_on(before: Option<u32>) -> Option<u32> {
    (before == Some(2)).then_some(1)
}

/// One step of the game hand-over for a monitor with integer scaling on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameAction {
    /// Integer Scaling off; GPU Scaling and its mode stay, so non-divisor game modes fill the panel.
    IntegerOff,
    /// Back to the integer mode's desktop mode (and its Windows scale).
    SetMode(Mode),
    /// GPU Scaling + Integer Scaling on.
    IntegerOn,
}

/// True when `m` is the native size divided by a whole factor (1x-4x): Integer Scaling shows it
/// sharp and full-screen. Anything else (2560x1440 on 4K) would sit 1x in black borders.
pub fn exact_divisor(native: Mode, m: Mode) -> bool {
    (1..=4u32).any(|k| m.w * k == native.w && m.h * k == native.h)
}

/// Game hand-over for a monitor in integer scaling mode. `on` = mode on for this monitor,
/// `game` = a game runs (exit already debounced), `in_game` = the hand-over is active (set at game
/// start, kept across tray restarts), `integer` = Integer Scaling now (None = not supported).
/// While a game runs, only Integer Scaling follows the game's mode: on for exact divisors (1080p,
/// 720p stay sharp), off for anything else (GPU Scaling stays, so 1440p fills the panel). The
/// game's mode itself is never changed. After it exits: the integer mode's desktop mode (only if
/// the game left another one) and Integer Scaling on. Returns the steps and the new `in_game`.
/// The record (originals for "off") is never touched here.
pub fn game_step(on: bool, game: bool, in_game: bool, current: Option<Mode>, native: Mode, target: Mode,
                 integer: Option<bool>) -> (Vec<GameAction>, bool) {
    if !on { return (Vec::new(), false); }
    let mut a = Vec::new();
    if game {
        let Some(cur) = current else { return (a, true) };
        let want = exact_divisor(native, cur);
        if integer.unwrap_or(false) != want { a.push(if want { GameAction::IntegerOn } else { GameAction::IntegerOff }); }
        return (a, true);
    }
    if in_game {
        if current != Some(target) { a.push(GameAction::SetMode(target)); }
        if integer != Some(true) || current != Some(target) { a.push(GameAction::IntegerOn); }
    }
    (a, false)
}

pub fn restore_mode(native: Mode, modes: &[Mode], before: Mode) -> Mode {
    if before.w == native.w && before.h == native.h && modes.contains(&before) { before } else { native }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// GDI name when switched on (can change between attaches; the monitor name is the key).
    pub gdi: String,
    /// EDID monitor name (= the ADLX display name).
    pub monitor: String,
    pub vendor_id: u32,
    pub native: Mode,
    /// Mode in use before switching on.
    pub before: Mode,
    pub target: Mode,
    pub factor: u32,
    /// Vendor scaling settings before switching on.
    pub scaling: Scaling,
    /// Windows scale (%) before switching on, and the one for the native mode (put back on "off").
    #[serde(default)]
    pub scale_before: Option<u32>,
    #[serde(default)]
    pub scale_native: Option<u32>,
    /// Font smoothing type before switching on (2 = ClearType), put back on "off".
    #[serde(default)]
    pub smoothing_before: Option<u32>,
}

fn dir() -> PathBuf {
    crate::util::user_dir()
}
fn path() -> PathBuf { dir().join("integer-scaling.json") }

pub fn load() -> Vec<Record> {
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(r: &[Record]) -> Result<()> {
    std::fs::create_dir_all(dir())?;
    if r.is_empty() { let _ = std::fs::remove_file(path()); return Ok(()); }
    std::fs::write(path(), serde_json::to_string_pretty(r)?)?;
    Ok(())
}

/// Originals recorded by `keel egpu amd-scaling on`, if any: they predate the current values.
pub fn cli_record(monitor: &str) -> Option<Scaling> {
    let s = std::fs::read_to_string(dir().join("amd-scaling.json")).ok()?;
    serde_json::from_str::<Vec<Scaling>>(&s).ok()?.into_iter().find(|x| x.display == monitor)
}

/// Vendor scaling switches for one monitor on the eGPU.
pub trait Scaler {
    fn read(&self, monitor: &str) -> Result<Scaling>;
    /// GPU Scaling on, then Integer Scaling on. Log lines per step.
    fn enable(&self, monitor: &str) -> Result<Vec<String>>;
    /// Integer Scaling alone off (GPU Scaling and the scaling mode stay), for the game hand-over.
    fn integer_off(&self, monitor: &str) -> Result<Vec<String>>;
    fn restore(&self, saved: &Scaling) -> Result<Vec<String>>;
}

pub struct AmdScaler(pub GpuProfile);

impl Scaler for AmdScaler {
    fn read(&self, monitor: &str) -> Result<Scaling> {
        amd::scaling_status(&self.0)?.into_iter().find(|s| s.display == monitor)
            .ok_or_else(|| anyhow::anyhow!("ADLX does not list {monitor} on the eGPU"))
    }
    fn enable(&self, monitor: &str) -> Result<Vec<String>> { Ok(amd::scaling_on(&self.0, Some(monitor))?.1) }
    fn integer_off(&self, monitor: &str) -> Result<Vec<String>> { amd::scaling_write(&self.0, monitor, amd::ScalingWrite::Integer(false)) }
    fn restore(&self, saved: &Scaling) -> Result<Vec<String>> { amd::scaling_restore(&self.0, std::slice::from_ref(saved)) }
}

/// NVIDIA: integer scaling is one value of the target's scaling in NVAPI's display config
/// (nvdisp.rs). `scaling_mode` records that value; GPU Scaling and Integer Scaling are read from it.
/// The first set can renumber the GDI names (\\.\DISPLAYn) of the NVIDIA monitors, so the monitor
/// is looked up by name each time and callers look its GDI name up again afterwards.
pub struct NvidiaScaler;

fn nv_gdi(monitor: &str) -> Result<String> {
    crate::display::gdi_of(monitor).ok_or_else(|| anyhow::anyhow!("{monitor} is not an active external display"))
}

fn nv_set(monitor: &str, v: i32) -> Result<Vec<String>> {
    let gdi = nv_gdi(monitor)?;
    let before = crate::nvdisp::set_scaling(&gdi, v)?;
    Ok(if before == v { Vec::new() } else {
        vec![format!("{monitor}: NVIDIA scaling {} -> {}", crate::nvdisp::scaling_name(before), crate::nvdisp::scaling_name(v))]
    })
}

impl Scaler for NvidiaScaler {
    fn read(&self, monitor: &str) -> Result<Scaling> {
        let v = crate::nvdisp::scaling(&nv_gdi(monitor)?)?;
        Ok(Scaling { display: monitor.into(), gpu_scaling: Some(matches!(v, 2 | 3 | 5 | 8)), scaling_mode: Some(v),
                     integer_scaling: Some(v == crate::nvdisp::INTEGER) })
    }
    fn enable(&self, m: &str) -> Result<Vec<String>> { nv_set(m, crate::nvdisp::INTEGER) }
    fn integer_off(&self, m: &str) -> Result<Vec<String>> { nv_set(m, crate::nvdisp::GPU_ASPECT) }
    fn restore(&self, saved: &Scaling) -> Result<Vec<String>> {
        nv_set(&saved.display, saved.scaling_mode.unwrap_or(crate::nvdisp::DEFAULT))
    }
}

pub fn scaler_for(p: &GpuProfile) -> Option<Box<dyn Scaler>> {
    match p.vendor_id {
        VENDOR_AMD => Some(Box::new(AmdScaler(p.clone()))),
        VENDOR_NVIDIA => Some(Box::new(NvidiaScaler)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(w: u32, h: u32, hz: u32) -> Mode { Mode { w, h, hz } }

    #[test]
    fn divisors_of_4k() {
        let modes = vec![m(3840, 2160, 60), m(3840, 2160, 59), m(2560, 1440, 60), m(1920, 1080, 60), m(1920, 1080, 59),
                         m(1280, 720, 60), m(1280, 720, 50), m(960, 540, 60), m(1600, 900, 60)];
        let native = native_of(&modes).unwrap();
        assert_eq!(native, m(3840, 2160, 60));
        assert_eq!(candidates(native, &modes, 59), vec![(2, m(1920, 1080, 59)), (3, m(1280, 720, 60)), (4, m(960, 540, 60))]);
        assert_eq!(candidates(native, &modes, 60)[0], (2, m(1920, 1080, 60)));
    }

    #[test]
    fn no_candidates_without_exact_modes() {
        let modes = vec![m(2560, 1600, 60), m(1920, 1200, 60), m(1680, 1050, 60)];
        // 2560x1600 / 2 = 1280x800 is not listed; /3 and /4 do not divide evenly or are missing.
        assert!(candidates(native_of(&modes).unwrap(), &modes, 60).is_empty());
    }

    #[test]
    fn cleartype_becomes_grayscale_only() {
        assert_eq!(smoothing_while_on(Some(2)), Some(1));
        assert_eq!(smoothing_while_on(Some(1)), None);
        assert_eq!(smoothing_while_on(None), None);
    }

    #[test]
    fn scale_keeps_sizes() {
        assert_eq!(scale_for_factor(200, 2), 100);
        assert_eq!(scale_for_factor(300, 2), 150);
        assert_eq!(scale_for_factor(150, 2), 100); // 75% does not exist
        assert_eq!(scale_for_factor(300, 3), 100);
        assert_eq!(native_scale(true, 200, 2), 200);
        assert_eq!(native_scale(false, 100, 2), 200); // was at 1080 / 100% already
    }

    #[test]
    fn exact_divisors_of_4k() {
        let n = m(3840, 2160, 60);
        for ok in [m(3840, 2160, 60), m(1920, 1080, 144), m(1280, 720, 60), m(960, 540, 60)] { assert!(exact_divisor(n, ok), "{ok}"); }
        for no in [m(2560, 1440, 60), m(1600, 900, 60), m(1920, 1200, 60), m(3440, 1440, 60)] { assert!(!exact_divisor(n, no), "{no}"); }
    }

    #[test]
    fn game_follows_its_mode_with_integer_scaling_only() {
        let (n, t) = (m(3840, 2160, 60), m(1920, 1080, 60));
        let g = |cur, integer, in_game| game_step(true, true, in_game, Some(cur), n, t, integer);
        // Game at 1080p (or 720p): stays sharp, nothing to do.
        assert_eq!(g(t, Some(true), false), (vec![], true));
        assert_eq!(g(m(1280, 720, 60), Some(true), true), (vec![], true));
        // Game at 1440p: Integer Scaling off so GPU Scaling fills the panel; the mode is the game's.
        assert_eq!(g(m(2560, 1440, 60), Some(true), true), (vec![GameAction::IntegerOff], true));
        assert_eq!(g(m(2560, 1440, 60), Some(false), true), (vec![], true));
        // Back to an exact divisor in the game: Integer Scaling on again.
        assert_eq!(g(t, Some(false), true), (vec![GameAction::IntegerOn], true));
        // Native 4K in the game: no scaling happens; Integer Scaling on (driver may report None).
        assert_eq!(g(n, None, true), (vec![GameAction::IntegerOn], true));
        // Mode unknown during a switch: wait.
        assert_eq!(game_step(true, true, true, None, n, t, Some(true)), (vec![], true));
    }

    #[test]
    fn game_exit_restores_the_integer_desktop_once() {
        let (n, t) = (m(3840, 2160, 60), m(1920, 1080, 60));
        // Game left 1440p: back to 1080p, then Integer Scaling on.
        assert_eq!(game_step(true, false, true, Some(m(2560, 1440, 60)), n, t, Some(false)),
                   (vec![GameAction::SetMode(t), GameAction::IntegerOn], false));
        // Game restored 1080p itself: no mode change; Integer Scaling only if it is off.
        assert_eq!(game_step(true, false, true, Some(t), n, t, Some(false)), (vec![GameAction::IntegerOn], false));
        assert_eq!(game_step(true, false, true, Some(t), n, t, Some(true)), (vec![], false));
        // No game and no hand-over: a mode picked by hand is not fought.
        assert_eq!(game_step(true, false, false, Some(m(2560, 1440, 60)), n, t, Some(false)), (vec![], false));
        // Integer scaling mode off: nothing, and the hand-over is forgotten.
        assert_eq!(game_step(false, true, false, Some(t), n, t, Some(true)), (vec![], false));
        assert_eq!(game_step(false, false, true, Some(m(2560, 1440, 60)), n, t, Some(false)), (vec![], false));
    }

    #[test]
    fn restore_keeps_refresh_when_native_has_it() {
        let modes = vec![m(3840, 2160, 60), m(3840, 2160, 59), m(1920, 1080, 59)];
        assert_eq!(restore_mode(m(3840, 2160, 60), &modes, m(3840, 2160, 59)), m(3840, 2160, 59));
        // Already below native before switching on (1920x1080 saved earlier): back to native 60 Hz.
        assert_eq!(restore_mode(m(3840, 2160, 60), &modes, m(1920, 1080, 59)), m(3840, 2160, 60));
    }
}

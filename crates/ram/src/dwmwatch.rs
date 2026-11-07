//! Warns when dwm.exe holds too much system RAM (its private bytes). dwm can hold several GB
//! when the main display is the built-in screen and apps drawn on the iGPU are copied to an
//! eGPU's 4K monitor. With the eGPU monitor as main display it stays under
//! 1 GB. Restarting dwm gives it back. Pure decision logic here; the tray notifies.
use crate::observe::Sample;

/// Warn at this much private memory ...
pub const WARN: u64 = 1536 << 20;
/// ... and warn again only after it fell below this (e.g. dwm was restarted).
pub const REARM: u64 = 1200 << 20;

/// dwm's private bytes in this sample (summed if Windows runs more than one, e.g. two sessions).
pub fn private_bytes(s: &Sample) -> Option<u64> {
    let v: Vec<u64> = s.p.iter().filter(|p| p.n.eq_ignore_ascii_case("dwm.exe")).map(|p| p.pb).collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

/// The notification text when dwm crosses `WARN`; `warned` stays set until it drops below `REARM`.
pub fn check(s: &Sample, warned: &mut bool) -> Option<String> {
    let pb = private_bytes(s)?;
    if pb < REARM { *warned = false; }
    if pb < WARN || std::mem::replace(warned, true) { return None; }
    Some(format!("Desktop (dwm.exe) holds {:.1} GB of system RAM. Restart it from an admin prompt: taskkill /f /im dwm.exe",
        pb as f64 / (1u64 << 30) as f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::PRow;
    const M: u64 = 1 << 20;

    fn sample(name: &str, pb: u64) -> Sample {
        Sample { t: 0, free: None, adapters: vec![], k: vec![], p: vec![PRow { pid: 2768, ct: 1, n: name.into(), pb, ws: 0, h: 0, pws: 0, g: vec![] }] }
    }

    #[test]
    fn warns_once_and_again_after_dropping() {
        let mut w = false;
        assert_eq!(check(&sample("dwm.exe", 600 * M), &mut w), None);
        let m = check(&sample("dwm.exe", 4070 * M), &mut w).expect("warned");
        assert!(m.contains("4.0 GB of system RAM"), "{m}");
        assert_eq!(check(&sample("dwm.exe", 3000 * M), &mut w), None, "no repeat while still high");
        assert_eq!(check(&sample("dwm.exe", 1300 * M), &mut w), None);
        assert_eq!(check(&sample("dwm.exe", 1600 * M), &mut w), None, "1.3 GB does not re-arm");
        assert_eq!(check(&sample("dwm.exe", 400 * M), &mut w), None, "dwm restarted");
        assert!(check(&sample("dwm.exe", 1600 * M), &mut w).is_some());
    }

    #[test]
    fn ignores_other_processes() {
        let mut w = false;
        assert_eq!(check(&sample("chrome.exe", 4000 * M), &mut w), None);
        assert!(!w);
    }
}

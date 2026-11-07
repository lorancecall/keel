//! The AMD part of the External GPU page's Tuning card. Runs `keel egpu amd-tune` / `amd-feature`
//! hidden (ADLX's 3D settings only answer in the user session and must load the eGPU driver's own
//! ADLX, which the window keeps out of its process); the CLI remembers what was set for the
//! next attach.
use crate::{AmdFeat, EgpuData, MainWindow};
use keel_egpu::amdtune::Tuning;
use slint::{ComponentHandle, ModelRc, VecModel};

fn keel(args: &[&str]) -> Result<String, String> {
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("keel.exe"))).unwrap_or_else(|| "keel.exe".into());
    let o = keel_core::cmd::hidden(exe).args(args).output().map_err(|e| e.to_string())?;
    let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if o.status.success() { Ok(out) } else { Err(String::from_utf8_lossy(&o.stderr).trim().trim_start_matches("error: ").to_string()) }
}

fn show(ui: &MainWindow, t: Result<Tuning, String>) {
    let d = ui.global::<EgpuData>();
    d.set_amd_busy(false);
    match t {
        Err(e) => { d.set_amd_note(format!("Radeon settings unavailable: {e}").into()); d.set_amd_power_ok(false); d.set_amd_mhz_ok(false); }
        Ok(t) => {
            d.set_amd_power_ok(t.power.is_some());
            d.set_amd_mhz_ok(t.max_mhz.is_some());
            if let Some(p) = t.power { d.set_amd_power_min(p.min as f32); d.set_amd_power_max(p.max as f32); d.set_amd_power(p.now as f32); }
            if let Some(m) = t.max_mhz { d.set_amd_mhz_min(m.min as f32); d.set_amd_mhz_max(m.max as f32); d.set_amd_mhz(m.now as f32); }
            d.set_amd_note(if t.power.is_none() && t.max_mhz.is_none() {
                "This card's driver offers no power or clock tuning (common for mobile GPUs).".into()
            } else { Default::default() });
            d.set_amd_features(ModelRc::new(VecModel::from(t.features.into_iter().map(|f| AmdFeat { name: f.name.into(), on: f.on }).collect::<Vec<_>>())));
        }
    }
}

/// Reads the AMD eGPU's tuning and features (worker thread), when an AMD eGPU is attached.
pub fn load(ui: &MainWindow) {
    if !ui.global::<EgpuData>().get_amd() { return; }
    ui.global::<EgpuData>().set_amd_busy(true);
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let t = keel(&["egpu", "amd-tune", "--json"]).and_then(|s| serde_json::from_str::<Tuning>(&s).map_err(|e| e.to_string()));
        let _ = w.upgrade_in_event_loop(move |ui| show(&ui, t));
    });
}

/// Runs one change, shows its result in the window's status line, then reads everything again.
fn change(ui: &MainWindow, args: Vec<String>) {
    ui.global::<EgpuData>().set_amd_busy(true);
    let tuning = args.get(1).is_some_and(|a| a == "amd-tune");
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = keel(&a);
        let _ = w.upgrade_in_event_loop(move |ui| {
            if tuning && r.is_ok() { crate::EGPU.with_borrow_mut(crate::egpu_page::reset_peaks); }
            ui.set_status(match r { Ok(s) => s.lines().last().unwrap_or("Done").to_string().into(), Err(e) => format!("Not changed: {e}").into() });
            load(&ui);
        });
    });
}

pub fn wire(ui: &MainWindow) {
    let d = ui.global::<EgpuData>();
    let w = ui.as_weak();
    d.on_amd_load(move || if let Some(ui) = w.upgrade() { load(&ui); });
    let w = ui.as_weak();
    d.on_amd_apply(move || {
        let Some(ui) = w.upgrade() else { return };
        let d = ui.global::<EgpuData>();
        let mut args = vec!["egpu".to_string(), "amd-tune".into(), "set".into()];
        if d.get_amd_power_ok() { args.extend(["power".into(), format!("{}", d.get_amd_power().round() as i32)]); }
        if d.get_amd_mhz_ok() { args.extend(["max-mhz".into(), format!("{}", d.get_amd_mhz().round() as i32)]); }
        change(&ui, args);
    });
    let w = ui.as_weak();
    d.on_amd_reset(move || if let Some(ui) = w.upgrade() { change(&ui, vec!["egpu".into(), "amd-tune".into(), "reset".into()]); });
    let w = ui.as_weak();
    d.on_amd_feature(move |name, on| if let Some(ui) = w.upgrade() {
        change(&ui, vec!["egpu".into(), "amd-feature".into(), name.to_string(), if on { "on".into() } else { "off".into() }]);
    });
}

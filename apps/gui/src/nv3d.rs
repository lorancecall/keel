//! The NVIDIA part of the External GPU page's Tuning card: 3D settings on the driver's global
//! profile (`keel_egpu::nvdrs`). Read with `keel egpu nv-3d --json` (no admin needed), written by
//! the service (`NvSetting`, offered settings and values only).
use crate::{EgpuData, MainWindow};
use keel_egpu::nvdrs::{self, Current};
use slint::ComponentHandle;

const VSYNC: [u32; 3] = [0x6092_5292, 0x0841_6747, 0x4781_4940];
const PSTATE: [u32; 3] = [1, 0, 5];

fn show(ui: &MainWindow, r: Result<Vec<Current>, String>) {
    let d = ui.global::<EgpuData>();
    d.set_nv3d_busy(false);
    match r {
        Err(e) => { d.set_nv3d_ok(false); d.set_nv3d_note(format!("3D settings unavailable: {e}").into()); }
        Ok(v) => {
            let get = |id: u32| v.iter().find(|c| c.id == id).and_then(|c| c.value);
            d.set_nv_fps(get(nvdrs::FRL_FPS).unwrap_or(0) as f32);
            d.set_nv_lowlat(get(nvdrs::PRERENDER) == Some(1));
            d.set_nv_vsync(get(nvdrs::VSYNC).and_then(|x| VSYNC.iter().position(|y| *y == x)).unwrap_or(0) as i32);
            // The driver's default power mode is "optimal power".
            d.set_nv_pstate(get(nvdrs::PSTATE).and_then(|x| PSTATE.iter().position(|y| *y == x)).unwrap_or(2) as i32);
            d.set_nv3d_ok(true);
            d.set_nv3d_note(Default::default());
        }
    }
}

pub fn load(ui: &MainWindow) {
    if !ui.global::<EgpuData>().get_nvidia() { return; }
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("keel.exe"))).unwrap_or_else(|| "keel.exe".into());
        let r = keel_core::cmd::hidden(exe).args(["egpu", "nv-3d", "--json"]).output().map_err(|e| e.to_string())
            .and_then(|o| if o.status.success() { serde_json::from_slice::<Vec<Current>>(&o.stdout).map_err(|e| e.to_string()) }
                          else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) });
        let _ = w.upgrade_in_event_loop(move |ui| show(&ui, r));
    });
}

/// Asks the service to set one setting (`None`: the driver's default), then reads them again.
fn set(ui: &MainWindow, id: u32, value: Option<u32>) {
    ui.global::<EgpuData>().set_nv3d_busy(true);
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let r = keel_service::call(&keel_service::Request::NvSetting { id, value });
        let _ = w.upgrade_in_event_loop(move |ui| {
            ui.set_status(match r { Ok(r) => r.message.into(), Err(e) => format!("Not changed: {e:#}").into() });
            load(&ui);
        });
    });
}

pub fn wire(ui: &MainWindow) {
    let d = ui.global::<EgpuData>();
    let w = ui.as_weak();
    d.on_nv3d_load(move || if let Some(ui) = w.upgrade() { load(&ui); });
    let w = ui.as_weak();
    d.on_nv_fps_set(move || if let Some(ui) = w.upgrade() {
        let fps = ui.global::<EgpuData>().get_nv_fps().round() as u32;
        set(&ui, nvdrs::FRL_FPS, (fps > 0).then_some(fps));
    });
    let w = ui.as_weak();
    d.on_nv_lowlat_set(move || if let Some(ui) = w.upgrade() {
        let on = ui.global::<EgpuData>().get_nv_lowlat();
        set(&ui, nvdrs::PRERENDER, on.then_some(1));
    });
    let w = ui.as_weak();
    d.on_nv_vsync_set(move || if let Some(ui) = w.upgrade() {
        let i = ui.global::<EgpuData>().get_nv_vsync().clamp(0, 2) as usize;
        set(&ui, nvdrs::VSYNC, (i > 0).then_some(VSYNC[i]));
    });
    let w = ui.as_weak();
    d.on_nv_pstate_set(move || if let Some(ui) = w.upgrade() {
        let i = ui.global::<EgpuData>().get_nv_pstate().clamp(0, 2) as usize;
        set(&ui, nvdrs::PSTATE, (i < 2).then_some(PSTATE[i]));
    });
}

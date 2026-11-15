//! Debug-only visual fixture. No tray, service, filesystem, or system-setting calls.
use crate::{MainWindow, Pages, HomeData, VpnRow, relay_form};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

fn strings(values: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(values.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>()))
}
fn fill(ui: &MainWindow) {
    ui.set_edit_index(0);
    ui.set_new_name("Example VPN".into());
    ui.set_new_exe("C:\\Program Files\\Example VPN\\vpn.exe".into());
    ui.set_new_adapter("Example tunnel*".into());
    ui.set_new_kill(true);
    ui.set_relay_labels(strings(&["Select Ethernet adapter", "Ethernet", "USB Ethernet (not connected)"]));
    ui.set_relay_aliases(strings(&["", "Ethernet", "USB Ethernet"]));
    ui.set_new_relay_interface(1);
    ui.set_new_relay_hosts(strings(&["192.168.50.5", "10.10.0.8"]));
    ui.set_new_relay_host(SharedString::new());
    ui.set_relay_ack(false);
    ui.set_form_error(SharedString::new());
    ui.set_add_open(true);
}

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    ui.set_status("PREVIEW ONLY: no service or settings access".into());
    ui.set_page(ui.global::<Pages>().get_network());
    ui.global::<HomeData>().set_net_summary("Offline visual fixture".into());
    ui.global::<HomeData>().set_net_note("Mock data only. Save and apply is simulated.".into());
    ui.set_net_lans(strings(&["Any wired adapter", "Example Ethernet"]));
    ui.set_vpn_list(ModelRc::new(VecModel::from(vec![VpnRow {
        name: "Example VPN".into(), kind: "2 LAN relay hosts on Ethernet".into(), via: 0, kill: true, exe: "Example VPN program".into(),
    }])));
    fill(&ui);
    { let w = ui.as_weak(); ui.on_vpn_edit(move |_| { if let Some(ui) = w.upgrade() { fill(&ui); } }); }
    { let w = ui.as_weak(); ui.on_relay_add(move || {
        let Some(ui) = w.upgrade() else { return };
        let mut hosts: Vec<String> = ui.get_new_relay_hosts().iter().map(|h| h.to_string()).collect();
        match relay_form::add_host(&mut hosts, ui.get_new_relay_host().as_str()) {
            Ok(()) => { ui.set_new_relay_hosts(ModelRc::new(VecModel::from(hosts.into_iter().map(SharedString::from).collect::<Vec<_>>()))); ui.set_new_relay_host(SharedString::new()); ui.set_relay_ack(false); ui.set_form_error(SharedString::new()); }
            Err(e) => ui.set_form_error(e.into()),
        }
    }); }
    { let w = ui.as_weak(); ui.on_relay_remove(move |at| {
        let Some(ui) = w.upgrade() else { return };
        let mut hosts: Vec<SharedString> = ui.get_new_relay_hosts().iter().collect();
        if let Ok(at) = usize::try_from(at) { if at < hosts.len() { hosts.remove(at); } }
        ui.set_new_relay_hosts(ModelRc::new(VecModel::from(hosts))); ui.set_relay_ack(false);
    }); }
    { let w = ui.as_weak(); ui.on_vpn_add(move || {
        let Some(ui) = w.upgrade() else { return };
        if !ui.get_new_relay_host().trim().is_empty() { ui.set_form_error("Click Add IP or clear the pending LAN IP first".into()); return; }
        if ui.get_new_relay_hosts().row_count() > 0 && !ui.get_relay_ack() { ui.set_form_error("Confirm the LAN relay access warning before Done".into()); return; }
        ui.set_add_open(false); ui.set_net_dirty(true); ui.set_vpn_note("Preview edit retained in memory only".into());
    }); }
    { let w = ui.as_weak(); ui.on_net_save(move || {
        if let Some(ui) = w.upgrade() { ui.set_net_dirty(true); ui.set_status("Simulated save failure: edits retained (no service called)".into()); }
    }); }
    ui.run()
}

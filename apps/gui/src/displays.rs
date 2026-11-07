//! Displays page (ui/displays.slint). Reads the displays itself (cheap Win32 calls); changes go
//! where their safety net lives: mode changes and eGPU integer scaling to the tray (keep-or-go-back
//! box, integer scaling records; `TrayRequest`), the built-in screen's integer scaling straight to
//! the iGPU driver (ADLX, on a worker thread), the main display through Windows.
use crate::{DisplayCard, DisplaysData, MainWindow};
use keel_core::traycfg::TrayRequest;
use keel_egpu::{amd, display, intscale};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;

struct Slot { gdi: String, monitor: String, kind: i32, modes: Vec<display::Mode> }

thread_local! {
    static SLOTS: RefCell<Vec<Slot>> = Default::default();
    /// The iGPU's scaling switch: None until read, then (its display name, Integer Scaling or None
    /// when the driver does not offer it) or the error text.
    static IGPU: RefCell<Option<Result<(String, Option<bool>), String>>> = Default::default();
}

fn mode_text(m: display::Mode, native: Option<display::Mode>) -> String {
    let n = native.is_some_and(|n| n.w == m.w && n.h == m.h);
    format!("{} x {} @ {} Hz{}", m.w, m.h, m.hz, if n { "  (native)" } else { "" })
}

/// eGPU LUID and the running game from the service's status file.
fn egpu_and_game() -> (Option<u64>, Option<String>) {
    let Some(s) = std::fs::read_to_string(keel_egpu::util::status_path()).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else { return (None, None) };
    let egpus: Vec<&str> = s["egpus"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    let luid = s["adapters"].as_array().and_then(|a| a.iter().find(|x| x["name"].as_str().is_some_and(|n| egpus.contains(&n))))
        .and_then(|x| x["luid"].as_u64());
    (luid, s["game"].as_str().map(String::from))
}

/// Fills DisplaysData from the displays as they are now.
pub fn show(ui: &MainWindow) {
    let (cards, egpu) = cards();
    let d = ui.global::<DisplaysData>();
    d.set_note(if egpu { "Resolution changes ask to keep; they revert after 15 s." } else { "No eGPU attached." }.into());
    d.set_cards(ModelRc::new(VecModel::from(cards)));
}

/// One card per active display (built-in, eGPU monitors, others), remembered for the actions;
/// also whether an eGPU is attached. Used by the Displays page and the tray panel.
pub fn cards() -> (Vec<DisplayCard>, bool) {
    let (egpu, game) = egpu_and_game();
    let adapters = keel_egpu::gpu::adapters().unwrap_or_default();
    let pos = display::positions();
    let main = display::primary_of(&pos).map(String::from);
    let recs = intscale::load();
    let igpu = IGPU.with_borrow(|v| v.clone());
    let mut slots = Vec::new();
    let mut cards = Vec::new();
    let mut shown = display::active_displays();
    // Built-in first, then eGPU monitors, then the rest.
    shown.sort_by_key(|d| if d.internal { 0 } else if Some(d.adapter_luid) == egpu { 1 } else { 2 });
    for d in shown {
        let kind = if d.internal { 0 } else if Some(d.adapter_luid) == egpu { 1 } else { 2 };
        let modes = display::modes(&d.gdi_name);
        let native = intscale::native_of(&modes);
        let cur = display::current_mode(&d.gdi_name);
        let gpu = adapters.iter().find(|a| a.luid == d.adapter_luid).map_or("?".to_string(), |a| a.name.clone());
        let name = if d.monitor.is_empty() { d.gdi_name.rsplit('\\').next().unwrap_or(&d.gdi_name).to_string() } else { d.monitor.clone() };
        let first = native.and_then(|n| intscale::candidates(n, &modes, cur.map_or(60, |c| c.hz)).into_iter().next());
        let (int_on, int_ok, int_note) = match kind {
            1 => {
                let rec = recs.iter().find(|r| r.monitor == d.monitor);
                match (rec, first, &game) {
                    (Some(r), _, _) => (true, game.is_none(), format!("On: {} x {} at {}x on {} x {}.", r.target.w, r.target.h, r.factor, r.native.w, r.native.h)),
                    (None, _, Some(g)) => (false, false, format!("A game is running ({g}): switch it on after it exits.")),
                    (None, Some((k, m)), None) => (false, true, format!("Off. On: {} x {} at {k}x, sharp.", m.w, m.h)),
                    (None, None, None) => (false, false, "No exact-divisor mode.".into()),
                }
            }
            0 => match &igpu {
                None => (false, false, "Reading the iGPU driver...".into()),
                Some(Err(e)) => (false, false, format!("iGPU driver: {e}")),
                Some(Ok((_, None))) => (false, false, "The iGPU driver does not offer integer scaling.".into()),
                Some(Ok((_, Some(on)))) => {
                    let sharp: Vec<String> = native.map(|n| intscale::candidates(n, &modes, cur.map_or(60, |c| c.hz)).into_iter()
                        .map(|(k, m)| format!("{} x {} ({k}x)", m.w, m.h)).collect()).unwrap_or_default();
                    let panel = native.map_or(String::new(), |n| format!(" on the {} x {} panel", n.w, n.h));
                    (*on, true, match (sharp.is_empty(), *on) {
                        (true, _) => "No exact-divisor resolution.".into(),
                        (false, true) => format!("On: {} sharp{panel}, also from Legion Space.", sharp.join(", ")),
                        (false, false) => format!("Off. On: {} sharp{panel}.", sharp.join(", ")),
                    })
                }
            },
            _ => (false, false, "Built-in screen and eGPU monitors only.".into()),
        };
        let at = pos.iter().find(|(n, _, _)| *n == d.gdi_name).map(|(_, x, y)| format!(" · at {x}, {y}")).unwrap_or_default();
        cards.push(DisplayCard {
            name: name.into(),
            sub: format!("{gpu}{at}").into(),
            kind,
            main: main.as_deref() == Some(d.gdi_name.as_str()),
            modes: ModelRc::new(VecModel::from(modes.iter().map(|m| SharedString::from(mode_text(*m, native))).collect::<Vec<_>>())),
            mode_index: cur.and_then(|c| modes.iter().position(|m| *m == c)).map_or(-1, |i| i as i32),
            scale: display::scale(&d.gdi_name).map_or(String::new(), |s| format!("Windows scale {s}%")).into(),
            int_on, int_ok, int_note: int_note.into(),
        });
        slots.push(Slot { gdi: d.gdi_name, monitor: d.monitor, kind, modes });
    }
    SLOTS.with_borrow_mut(|s| *s = slots);
    (cards, egpu.is_some())
}

/// Reads the iGPU's scaling switch (ADLX) on a worker thread, then runs `done` on the UI thread.
pub fn read_igpu(done: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        let r = amd::igpu_profile().and_then(|p| amd::scaling_status(&p))
            .map(|v| v.into_iter().next().map(|s| (s.display, s.integer_scaling)).unwrap_or_default())
            .map_err(|e| format!("{e:#}"));
        let _ = slint::invoke_from_event_loop(move || { IGPU.with_borrow_mut(|v| *v = Some(r)); done(); });
    });
}

/// Reads the iGPU's scaling switch, then shows the page again.
pub fn load_igpu(ui: &MainWindow) {
    let w = ui.as_weak();
    read_igpu(move || if let Some(ui) = w.upgrade() { show(&ui); });
}

/// Integer scaling on / off for a card: eGPU monitors through the tray, the built-in screen
/// through the iGPU driver (worker thread). `done` gets the result text on the UI thread, after
/// the iGPU state was read again.
pub fn switch_integer(card: i32, on: bool, done: impl FnOnce(String) + Send + 'static) {
    let Some((monitor, kind)) = slot(card, |s| (s.monitor.clone(), s.kind)) else { return };
    if kind == 1 {
        let r = TrayRequest { t: 0, cmd: "integer".into(), monitor, on, mode: None }.send();
        done(match r { Ok(()) => "Sent to the tray.".into(), Err(e) => format!("Not sent: {e}") });
        return;
    }
    let Some(Ok((name, _))) = IGPU.with_borrow(|v| v.clone()) else { return };
    std::thread::spawn(move || {
        let r = amd::igpu_profile().and_then(|p| amd::scaling_write(&p, &name, amd::ScalingWrite::Integer(on)));
        if let Ok(lines) = &r { for l in lines { keel_core::log::user(format!("egpu: igpu-scaling: {l}")); } }
        let text = match r {
            Ok(l) if l.is_empty() => "No change needed.".to_string(),
            Ok(_) => format!("Built-in screen: integer scaling {}.", if on { "on" } else { "off" }),
            Err(e) => format!("Not switched: {e:#}"),
        };
        read_igpu(move || done(text));
    });
}

fn slot<R>(i: i32, f: impl FnOnce(&Slot) -> R) -> Option<R> { SLOTS.with_borrow(|s| s.get(i.max(0) as usize).map(f)) }

fn later(ui: &MainWindow, secs: u64) {
    let w = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_secs(secs), move || if let Some(ui) = w.upgrade() { show(&ui); });
}

pub fn set_mode(ui: &MainWindow, card: i32, index: i32) {
    let Some((monitor, m)) = slot(card, |s| (s.monitor.clone(), s.modes.get(index.max(0) as usize).copied())) else { return };
    let Some(m) = m else { return };
    let r = TrayRequest { t: 0, cmd: "mode".into(), monitor, on: false, mode: Some((m.w, m.h, m.hz)) }.send();
    ui.global::<DisplaysData>().set_result(match r {
        Ok(()) => "Confirm in the tray box, or it reverts.".into(),
        Err(e) => format!("Not sent: {e}").into(),
    });
    later(ui, 4);
}

pub fn integer(ui: &MainWindow, card: i32, on: bool) {
    let w = ui.as_weak();
    switch_integer(card, on, move |text| if let Some(ui) = w.upgrade() {
        ui.global::<DisplaysData>().set_result(text.into());
        show(&ui);
        later(&ui, 4);
    });
}

pub fn make_main(ui: &MainWindow, card: i32) {
    let Some(gdi) = slot(card, |s| s.gdi.clone()) else { return };
    let r = display::set_primary(&gdi);
    keel_core::log::user(format!("main window: main display -> {gdi} ({r:?})"));
    ui.global::<DisplaysData>().set_result(match r { Ok(()) => "Main display changed.".into(), Err(e) => format!("Not changed: {e:#}").into() });
    later(ui, 2);
}

//! External GPU page: keeps a history of the service's eGPU telemetry while the window is open
//! (one sample per refresh, up to an hour), turns it into chart paths, fills the gauges / bars,
//! and sends tuning changes to the service. The stress test runs `keel egpu stress` hidden and
//! reads its JSON lines.
use crate::{EgpuData, Gauge, Bar, MainWindow, ResultRow as Row};
use keel_egpu::telemetry::GpuTelemetry;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Chart box of the Path elements in egpu.slint.
const W: f64 = 600.0;
const H: f64 = 200.0;
const KEEP: Duration = Duration::from_secs(3600);
pub const RANGES: [u64; 4] = [60, 300, 900, 3600];

#[derive(Clone)]
pub struct Sample { pub t: Instant, pub power: Option<f32>, pub temp: Option<f32>, pub clock: Option<u32>, pub held: Vec<String> }

/// Highest values seen per GPU (history and stress tests), kept in `%LOCALAPPDATA%\keel\egpu-peaks.json`:
/// the bars show the current value against them, since a clock's real maximum is what the card
/// actually reaches under load (boost), not a number the driver always reports.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Peak { pub core_mhz: u32, pub mem_mhz: u32, pub fan_rpm: u32, pub power_w: u32 }

impl Peak {
    /// Raises the peaks to what `t` shows; true when one went up.
    pub fn update(&mut self, t: &GpuTelemetry) -> bool {
        let before = self.clone();
        if let Some(c) = t.clock_mhz { self.core_mhz = self.core_mhz.max(c); }
        if let Some(m) = t.mem_clock_mhz { self.mem_mhz = self.mem_mhz.max(m); }
        if let Some(r) = t.fan_rpm { self.fan_rpm = self.fan_rpm.max(r); }
        if let Some(w) = t.power_w { self.power_w = self.power_w.max(w.round() as u32); }
        *self != before
    }
}

/// Top of the power gauge and graph (pure; tested): the board's maximum, else 1.25 x the limit;
/// a GPU that reports neither (AMD without power tuning) is scaled to the highest draw measured
/// (a stress test finds it); 100 W until one is.
pub fn power_scale(power_max: Option<f32>, limit: Option<f32>, peak_w: u32) -> f32 {
    power_max.or(limit.map(|l| l * 1.25)).unwrap_or(if peak_w >= 60 { peak_w as f32 } else { 100.0 })
}

fn peaks_path() -> std::path::PathBuf { keel_core::paths::user_data_dir().join("egpu-peaks.json") }

/// New limits make the old highest values meaningless: start the record again.
pub fn reset_peaks(pg: &mut Page) {
    let _ = std::fs::remove_file(peaks_path());
    pg.peaks = Some(Default::default());
}

fn load_peaks() -> std::collections::HashMap<String, Peak> {
    std::fs::read_to_string(peaks_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

#[derive(Default)]
pub struct Page {
    pub peaks: Option<std::collections::HashMap<String, Peak>>,
    pub history: VecDeque<Sample>,
    /// Sliders were set from the current limits (not again while the user drags them).
    pub seeded: bool,
    pub stress: Option<crate::stress::Run>,
}

/// SVG path of `values` (x = time in the window ending at `now`, y = value / top) in the chart box.
/// Gaps (None) start a new sub-path.
pub fn path(samples: &[(Instant, Option<f64>)], now: Instant, span: Duration, top: f64) -> String {
    if top <= 0.0 { return String::new(); }
    let start = now.checked_sub(span).unwrap_or(now);
    let mut out = String::new();
    let mut pen_down = false;
    for (t, v) in samples {
        if *t < start { continue; }
        let Some(v) = v else { pen_down = false; continue };
        let x = W * (t.duration_since(start).as_secs_f64() / span.as_secs_f64());
        let y = H - H * (v / top).clamp(0.0, 1.0);
        out.push_str(&format!("{}{x:.1} {y:.1} ", if pen_down { "L " } else { "M " }));
        pen_down = true;
    }
    out.trim_end().to_string()
}

fn fmt_opt<T: std::fmt::Display>(v: Option<T>, unit: &str) -> String { v.map_or("-".into(), |v| format!("{v}{unit}")) }

/// Fills EgpuData from the service's status (`telemetry` None = no eGPU or no service).
pub fn show(ui: &MainWindow, pg: &mut Page, alive: bool, name: &str, vendor: Option<u64>, t: Option<&GpuTelemetry>) {
    let d = ui.global::<EgpuData>();
    let now = Instant::now();
    d.set_attached(alive && t.is_some());
    d.set_nvidia(vendor == Some(keel_egpu::gpu::VENDOR_NVIDIA as u64));
    d.set_amd(vendor == Some(keel_egpu::gpu::VENDOR_AMD as u64));
    d.set_name(if !alive { "Keel service not running".into() } else if t.is_some() { name.into() } else { "No eGPU".into() });
    let Some(t) = t.filter(|_| alive) else { return };

    pg.history.push_back(Sample { t: now, power: t.power_w, temp: t.temp_c.or(t.hotspot_c), clock: t.clock_mhz, held: t.held_back.clone() });
    while pg.history.front().is_some_and(|s| now.duration_since(s.t) > KEEP) { pg.history.pop_front(); }

    let held: Vec<&str> = t.held_back.iter().map(String::as_str).filter(|h| *h != "idle").collect();
    d.set_sub(if held.is_empty() { SharedString::new() } else { format!("clock held back by {}", held.join(", ")).into() });

    // Peaks of this GPU (saved when one goes up).
    let peaks = pg.peaks.get_or_insert_with(load_peaks);
    let peak = peaks.entry(name.to_string()).or_default();
    if peak.update(t) {
        let _ = std::fs::create_dir_all(keel_core::paths::user_data_dir());
        let _ = std::fs::write(peaks_path(), serde_json::to_string_pretty(&*peaks).unwrap_or_default());
    }
    let peak = peaks[name].clone();

    // Gauges.
    let limit = t.power_limit_w.filter(|l| *l > 0.0);
    let hot = t.hotspot_c.is_some();
    let temp = t.hotspot_c.or(t.temp_c);
    let pscale = power_scale(t.power_max_w, limit, peak.power_w);
    // Without a maximum from the card, the scale is the highest draw seen: a stress test sets it.
    let unknown = t.power_max_w.is_none() && limit.is_none() && peak.power_w < 60;
    d.set_power_unknown(unknown);
    let gauges = vec![
        Gauge { label: "Power".into(), text: fmt_opt(t.power_w.map(|w| w.round() as i32), "").into(),
                // Without a maximum from the card, the scale is the highest draw seen: a stress test sets it.
                sub: limit.map_or("W".into(), |l| format!("limit {l:.0} W")).into(),
                lo: "0".into(), hi: if unknown { "?".into() } else { format!("{pscale:.0} W").into() },
                frac: match (t.power_w, limit) { (Some(w), Some(l)) => w / l, (Some(w), None) => (w / pscale).min(1.0), _ => 0.0 }, tone: 0,
                warn: matches!((t.power_w, limit), (Some(w), Some(l)) if w >= l * 0.95) },
        Gauge { label: if hot { "Hotspot".into() } else { "Temperature".into() }, text: fmt_opt(temp.map(|c| c.round() as i32), "°").into(),
                sub: match (hot, t.temp_c) { (true, Some(e)) => format!("edge {e:.0}°"), _ => "°C".into() }.into(),
                frac: temp.map_or(0.0, |c| c / 100.0), tone: 1, lo: "0".into(), hi: "100°".into(),
                warn: temp.is_some_and(|c| c >= if hot { 95.0 } else { 83.0 }) },
        Gauge { label: "Usage".into(), text: fmt_opt(t.usage_pct.map(|u| u.round() as i32), "%").into(), sub: "GPU".into(),
                frac: t.usage_pct.map_or(0.0, |u| u / 100.0), tone: 3, warn: false, lo: "0".into(), hi: "100%".into() },
    ];
    d.set_gauges(ModelRc::new(VecModel::from(gauges)));

    d.set_bars(ModelRc::new(VecModel::from(bars(t, &peak))));

    // History graph: each series on its own scale.
    let span = Duration::from_secs(RANGES[d.get_range().clamp(0, 3) as usize]);
    let ptop = pscale as f64;
    let ctop = t.clock_max_supported_mhz.unwrap_or(0).max(peak.core_mhz).max(500) as f64;
    let h: Vec<Sample> = pg.history.iter().cloned().collect();
    let series = |f: &dyn Fn(&Sample) -> Option<f64>, top: f64| path(&h.iter().map(|s| (s.t, f(s))).collect::<Vec<_>>(), now, span, top);
    d.set_power_path(series(&|s| s.power.map(f64::from), ptop).into());
    d.set_temp_path(series(&|s| s.temp.map(f64::from), 100.0).into());
    d.set_clock_path(series(&|s| s.clock.map(f64::from), ctop).into());
    d.set_limit_path(limit.map_or(String::new(), |l| { let y = H - H * (l as f64 / ptop).min(1.0); format!("M 0 {y:.1} L {W} {y:.1}") }).into());
    let since = pg.history.front().map_or(0, |s| now.duration_since(s.t).as_secs());
    // Power and temperature use the gauges' scales; the clock the highest seen.
    d.set_range_note(format!("Same scales as the gauges · clock to {ctop:.0} MHz{} · last {} min",
        if limit.is_some() { " · flat line: power limit" } else { "" }, since / 60).into());

    // Tuning (NVIDIA through the service; AMD later).
    let nvidia = d.get_nvidia();
    d.set_can_tune(nvidia && t.source == "nvml" && t.power_max_w.is_some());
    d.set_tune_note(if nvidia { "Update the Keel service to see the limits.".into() }
                    else { SharedString::new() });
    let mem_max = t.mem_clock_max_supported_mhz.filter(|_| nvidia);
    d.set_can_mem(mem_max.is_some());
    if let Some(m) = mem_max { d.set_mem_max(m as f32); }
    d.set_mem_note(if nvidia && mem_max.is_none() { "Memory clock: fixed on this GPU (RTX 30 and newer only).".into() } else { SharedString::new() });
    if let (Some(lo), Some(hi)) = (t.power_min_w, t.power_max_w) {
        d.set_power_min(lo);
        d.set_power_max(hi);
    }
    if let Some(def) = t.power_default_w { d.set_power_default(def); }
    if let Some(m) = t.clock_max_supported_mhz { d.set_clock_max(m as f32); }
    if !pg.seeded && d.get_can_tune() {
        d.set_power_limit(limit.unwrap_or(d.get_power_default()));
        d.set_clock_cap(t.clock_cap_mhz.map_or(d.get_clock_max(), |c| c as f32));
        d.set_mem_cap(t.mem_clock_cap_mhz.map_or(d.get_mem_max(), |c| c as f32));
        pg.seeded = true;
    }
}

/// The bars: clocks and fan against the highest values seen on this GPU, VRAM against its size
/// (red from 90 %). Pure; tested.
pub fn bars(t: &GpuTelemetry, peak: &Peak) -> Vec<Bar> {
    let mut v = Vec::new();
    let of = |now: u32, top: u32| if top == 0 { 0.0 } else { now as f32 / top as f32 };
    if let Some(c) = t.clock_mhz {
        let cap = t.clock_cap_mhz.map_or(String::new(), |m| format!(" · ceiling {m}"));
        v.push(Bar { label: "Core clock".into(), text: format!("{c} MHz · peak {}{cap}", peak.core_mhz).into(), frac: of(c, peak.core_mhz), warn: false });
    }
    if let Some(m) = t.mem_clock_mhz {
        v.push(Bar { label: "Memory clock".into(), text: format!("{m} MHz · peak {}", peak.mem_mhz).into(), frac: of(m, peak.mem_mhz), warn: false });
    }
    if let (Some(u), Some(total)) = (t.vram_used_mb, t.vram_total_mb) {
        let f = u as f32 / total.max(1) as f32;
        v.push(Bar { label: "VRAM".into(), text: format!("{:.1} / {:.0} GB", u as f64 / 1024.0, total as f64 / 1024.0).into(), frac: f, warn: f >= 0.9 });
    }
    match (t.fan_pct, t.fan_rpm) {
        (Some(p), _) => v.push(Bar { label: "Fan".into(), text: format!("{p} %").into(), frac: p as f32 / 100.0, warn: false }),
        // 0 rpm and never more on this GPU: the driver does not report this board's fan (the
        // RX 7600M XT eGPU read 0 rpm at 79 °C under full load). Seen spinning before: stopped.
        (None, Some(0)) if peak.fan_rpm == 0 => v.push(Bar { label: "Fan".into(), text: "no reading (the driver reports 0 rpm)".into(), frac: 0.0, warn: false }),
        (None, Some(0)) => v.push(Bar { label: "Fan".into(), text: format!("stopped · peak {} rpm", peak.fan_rpm).into(), frac: 0.0, warn: false }),
        (None, Some(r)) => v.push(Bar { label: "Fan".into(), text: format!("{r} rpm · peak {}", peak.fan_rpm).into(), frac: of(r, peak.fan_rpm), warn: false }),
        _ => {}
    }
    v
}

/// Stress test progress and, once it is over, its summary from the history taken meanwhile.
pub fn stress_tick(ui: &MainWindow, pg: &mut Page) {
    let d = ui.global::<EgpuData>();
    let Some(run) = pg.stress.as_mut() else { d.set_stress_running(false); return };
    match run.finished() {
        None => {
            let e = run.started.elapsed().as_secs();
            d.set_stress_running(true);
            d.set_stress_progress(run.progress());
            d.set_stress_status(format!("Running {}:{:02} / {}:{:02}", e / 60, e % 60, run.secs / 60, run.secs % 60).into());
        }
        Some(r) => {
            let since = run.started;
            let secs = since.elapsed().as_secs().min(run.secs);
            let points: Vec<crate::stress::Point> = pg.history.iter().filter(|s| s.t >= since + Duration::from_secs(2))
                .map(|s| crate::stress::Point { power: s.power, temp: s.temp, clock: s.clock, held: s.held.clone() }).collect();
            let (p, c, _) = limits(ui);
            d.set_stress_results(rows(crate::stress::summary(&points, secs, p.map(|w| w as f32), c, r.is_ok())));
            d.set_stress_status(match r { Ok(()) => format!("Finished at {}: see Report.", chrono::Local::now().format("%H:%M")),
                                          Err(e) => format!("The stress test stopped: {e}") }.into());
            d.set_stress_running(false);
            pg.stress = None;
        }
    }
}

/// The values the sliders ask for: None = driver default / no ceiling.
pub fn limits(ui: &MainWindow) -> (Option<u32>, Option<u32>, Option<u32>) {
    let d = ui.global::<EgpuData>();
    let p = d.get_power_limit().round();
    let c = d.get_clock_cap().round();
    let m = d.get_mem_cap().round();
    let power = ((p - d.get_power_default()).abs() >= 1.0).then_some(p as u32);
    let clock = (c < d.get_clock_max() - 1.0).then_some(c as u32);
    let mem = (d.get_can_mem() && m < d.get_mem_max() - 1.0).then_some(m as u32);
    (power, clock, mem)
}

pub fn rows(v: Vec<(String, String)>) -> ModelRc<Row> {
    ModelRc::new(VecModel::from(v.into_iter().map(|(l, t)| Row { label: l.into(), text: t.into() }).collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn power_scale_without_a_limit_follows_the_draw_seen() {
        assert_eq!(super::power_scale(Some(330.0), Some(250.0), 0), 330.0);
        assert_eq!(super::power_scale(None, Some(200.0), 0), 250.0);
        assert_eq!(super::power_scale(None, None, 0), 100.0);
        assert_eq!(super::power_scale(None, None, 101), 101.0, "7600M XT: the stress test's maximum");
    }

    use super::*;

    #[test]
    fn path_maps_time_and_value_and_breaks_on_gaps() {
        let now = Instant::now();
        let s = |secs: u64, v: Option<f64>| (now - Duration::from_secs(secs), v);
        let p = path(&[s(120, Some(1.0)), s(60, Some(50.0)), s(30, None), s(0, Some(100.0))], now, Duration::from_secs(60), 100.0);
        // 120 s ago is outside a 1-minute window; 60 s ago is x=0; after the gap a new sub-path.
        assert_eq!(p, "M 0.0 100.0 M 600.0 0.0");
        assert_eq!(path(&[s(0, Some(10.0))], now, Duration::from_secs(60), 0.0), "");
    }

    #[test]
    fn bars_use_the_highest_values_seen() {
        let mut p = Peak::default();
        let t = |c, m, r| GpuTelemetry { clock_mhz: Some(c), mem_clock_mhz: Some(m), fan_rpm: Some(r), vram_used_mb: Some(7500), vram_total_mb: Some(8176), ..Default::default() };
        assert!(p.update(&t(2450, 2250, 2100)));
        assert!(!p.update(&t(76, 61, 0)));
        let b = bars(&t(76, 61, 0), &p);
        assert_eq!(b[0].text, "76 MHz · peak 2450");
        assert!((b[0].frac - 76.0 / 2450.0).abs() < 1e-6);
        assert!(b[2].warn); // 7.3 of 8 GB VRAM
        assert_eq!(b[3].text, "stopped · peak 2100 rpm");
        assert_eq!(bars(&t(76, 61, 0), &Peak::default())[3].text, "no reading (the driver reports 0 rpm)");
    }
}

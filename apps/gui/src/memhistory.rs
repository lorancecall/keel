//! Memory history page (ui/memhistory.slint): findings of the leak heuristic over the tray's
//! 5-minute samples plus the processes the user watches, a detail chart of the selected one with
//! its trend line and display / power events, and the log drawer (service and tray logs).
//! Samples load on a worker thread; ignoring, selecting and filtering work on what is loaded.
use crate::{MainWindow, Marker, MemHistData, TrackRow};
use keel_core::traycfg::Prefs;
use keel_ram::leak::{Level, MB};
use keel_ram::observe::{self, Track};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;

/// The range picker: 0 this session, 1 the session before, 2 last 24 hours, 3 last 7 days.
pub const RANGES: i32 = 4;
const W: f64 = 600.0;
const H: f64 = 200.0;

/// What one load found.
#[derive(Default)]
pub struct Loaded {
    tracks: Vec<Track>,
    /// Display / power / device events: unix seconds and text.
    events: Vec<(i64, String)>,
    samples: usize,
    start: i64,
    end: i64,
    /// "This session (since Thu 19:45)" and the like, for the note.
    what: String,
    /// Running process names, largest first (the Watch picker).
    running: Vec<String>,
}

thread_local! {
    static STATE: RefCell<Loaded> = Default::default();
    /// Rows on screen -> index in `Loaded::tracks` (ignored ones are left out).
    static SHOWN: RefCell<Vec<usize>> = Default::default();
}

/// Worker thread: samples, verdicts and events of one range of the picker.
/// A session is one run of the tray (from logon, or from a Keel update): each start begins a new
/// one and the one before stays as "Previous session".
pub fn load(range: i32) -> Loaded {
    let sessions = observe::sessions();
    let (samples, what) = match range {
        0 | 1 => match sessions.len().checked_sub(1 + range as usize).map(|i| observe::load_file(&sessions[i])) {
            Some(v) => {
                let since = v.first().map_or(String::new(), |s| local(s.t, true));
                (v, format!("{} (since {since})", if range == 0 { "This session" } else { "Previous session" }))
            }
            None => (Vec::new(), if range == 0 { "This session".into() } else { "No previous session kept".into() }),
        },
        2 => (observe::load(24.0), "Last 24 hours".into()),
        _ => (observe::load(168.0), "Last 7 days".into()),
    };
    let (start, end) = (samples.first().map_or(0, |s| s.t), samples.last().map_or(0, |s| s.t));
    let hours = (chrono::Utc::now().timestamp() - start) as f64 / 3600.0 + 1.0;
    let events = keel_ram::events::load(hours).into_iter().flat_map(|r| r.items).filter(|i| i.t >= start && i.t <= end)
        .map(|i| (i.t, if i.detail.is_empty() { i.kind } else { format!("{}: {}", i.kind, i.detail) })).collect();
    let tracks = observe::tracks(&samples, &Prefs::load().watched, LARGEST);
    let mut ps = keel_ram::procs::snapshot().unwrap_or_default();
    ps.sort_by(|a, b| b.private_ws.cmp(&a.private_ws));
    let mut seen = std::collections::HashSet::new();
    let running = ps.into_iter().filter(|p| p.pid > 4 && seen.insert(p.name.to_lowercase())).take(40).map(|p| p.name).collect();
    Loaded { tracks, events, samples: samples.len(), start, end, what, running }
}

/// The key `Prefs::ignored` keeps: the pool tag for kernel pool rows, else the process name.
fn ignore_key(t: &Track) -> String {
    if t.pid == 0 { t.metric.split_once(':').map_or(t.metric.clone(), |(_, tag)| format!("pool:{tag}")) } else { t.name.to_lowercase() }
}

fn amount(unit: &str, v: f64) -> String {
    if unit != "bytes" { return format!("{v:.0}"); }
    if v.abs() >= 1024.0 * MB { format!("{:.2} GB", v / 1024.0 / MB) } else { format!("{:.0} MB", v / MB) }
}

fn signed(unit: &str, v: f64) -> String { format!("{}{}", if v >= 0.0 { "+" } else { "-" }, amount(unit, v.abs())) }

fn local(t: i64, with_day: bool) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(t, 0).single()
        .map(|d| d.format(if with_day { "%a %H:%M" } else { "%H:%M" }).to_string()).unwrap_or_default()
}

/// Not in the latest sample: the process closed or its adapter left (the eGPU was unplugged).
fn ended(t: &Track, end: i64) -> bool { t.t.last().is_some_and(|&x| x < end - 60) }

/// The status column: 0 likely leak, 1 rising, 2 watching (3 ended is set by the caller).
fn status(t: &Track) -> i32 {
    match (t.verdict.map(|v| v.level), t.early, t.top) {
        (Some(Level::Leak), false, _) => 0, (Some(_), false, _) => 1, (Some(_), true, _) => 4, (None, _, false) => 2, (None, _, true) => 5,
    }
}

/// Largest processes now shown even without growth, so a new session is not an empty page.
const LARGEST: usize = 6;

/// Value range of a series with a little room above and below (flat series get a band).
fn range(v: &[f64]) -> (f64, f64) {
    let lo = v.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if !lo.is_finite() { return (0.0, 1.0); }
    let pad = ((hi - lo) * 0.08).max(hi.abs() * 0.02).max(1.0);
    ((lo - pad).max(0.0), hi + pad)
}

/// SVG path of a series in the 600 x 200 chart box (pure).
pub fn path(t: &[i64], v: &[f64], (t0, t1): (i64, i64), (lo, hi): (f64, f64)) -> String {
    if t1 <= t0 || hi <= lo { return String::new(); }
    let mut out = String::new();
    for (i, (x, y)) in t.iter().zip(v).enumerate() {
        let px = W * (x - t0) as f64 / (t1 - t0) as f64;
        let py = H - H * ((y - lo) / (hi - lo)).clamp(0.0, 1.0);
        out.push_str(&format!("{}{px:.1} {py:.1} ", if i == 0 { "M " } else { "L " }));
    }
    out.trim_end().to_string()
}

/// Least-squares line of the series as a path (the trend the verdict's per-hour figure comes from).
fn trend(t: &[i64], v: &[f64], span: (i64, i64), r: (f64, f64)) -> String {
    let n = t.len().min(v.len());
    if n < 2 { return String::new(); }
    let (mt, mv) = (t.iter().map(|&x| x as f64).sum::<f64>() / n as f64, v.iter().sum::<f64>() / n as f64);
    let (sxy, sxx) = t.iter().zip(v).fold((0.0, 0.0), |(a, b), (&x, &y)| (a + (x as f64 - mt) * (y - mv), b + (x as f64 - mt).powi(2)));
    if sxx == 0.0 { return String::new(); }
    let k = sxy / sxx;
    let at = |x: i64| mv + k * (x as f64 - mt);
    path(&[t[0], t[n - 1]], &[at(t[0]), at(t[n - 1])], span, r)
}

/// A loaded result arrived: keep it and show it.
pub fn show(ui: &MainWindow, l: Loaded) {
    STATE.with_borrow_mut(|s| *s = l);
    render(ui);
}

/// Fills MemHistData from what is loaded and the user's ignore / watch lists.
pub fn render(ui: &MainWindow) {
    let d = ui.global::<MemHistData>();
    let p = Prefs::load();
    STATE.with_borrow(|s| {
        let mut shown: Vec<usize> = (0..s.tracks.len()).filter(|&i| !p.ignored.contains(&ignore_key(&s.tracks[i]))).collect();
        // Rows still in the latest sample first; the rest ended (process closed, eGPU unplugged).
        shown.sort_by_key(|&i| ended(&s.tracks[i], s.end));
        let rows: Vec<TrackRow> = shown.iter().map(|&i| {
            let t = &s.tracks[i];
            let span = (t.t.first().copied().unwrap_or(0), t.t.last().copied().unwrap_or(0));
            let growth = match t.verdict {
                Some(v) => format!("{}/h", signed(t.unit, v.per_hour)),
                None => signed(t.unit, t.v.last().unwrap_or(&0.0) - t.v.first().unwrap_or(&0.0)),
            };
            TrackRow {
                name: if t.pid == 0 { t.metric.split_once(':').map_or("kernel pool".into(), |(_, tag)| format!("pool {tag}")).into() } else { t.name.clone().into() },
                what: t.label.clone().into(),
                now: amount(t.unit, t.last).into(),
                growth: growth.into(),
                since: local(span.0, s.end - s.start > 20 * 3600).into(),
                status: if ended(t, s.end) { 3 } else { status(t) },
                ended: format!("Ended {}", local(span.1, s.end - s.start > 20 * 3600)).into(),
                spark: path(&t.t, &t.v, span, range(&t.v)).into(),
                watched: p.watched.iter().any(|w| w.eq_ignore_ascii_case(&t.name)),
                app: t.pid != 0,
            }
        }).collect();
        let count = |st: i32| rows.iter().filter(|r| r.status == st).count() as i32;
        d.set_leaks(count(0));
        d.set_rising(count(1));
        d.set_early(count(4));
        d.set_running(ModelRc::new(VecModel::from(s.running.iter().map(|n| slint::SharedString::from(n.as_str())).collect::<Vec<_>>())));
        d.set_watching(rows.iter().filter(|r| r.watched).count() as i32);
        d.set_ignored((s.tracks.len() - shown.len()) as i32);
        let hours = (s.end - s.start) as f64 / 3600.0;
        d.set_note(if s.samples == 0 {
            format!("{}: no samples yet (one every {}). Early signs after 20 min, verdicts after 2 h.",
                    s.what, if p.fast_samples { "minute" } else { "5 minutes" }).into()
        } else if rows.is_empty() {
            format!("{}: nothing growing ({} samples, {hours:.1} h).", s.what, s.samples).into()
        } else {
            format!("{}: {} samples, {hours:.1} h, last {}. Growth per hour; verdicts after 2 h.",
                    s.what, s.samples, local(s.end, false)).into()
        });
        let sel = d.get_selected();
        if sel >= rows.len() as i32 { d.set_selected(if rows.is_empty() { -1 } else { 0 }); }
        if sel < 0 && !rows.is_empty() { d.set_selected(0); }
        d.set_rows(ModelRc::new(VecModel::from(rows)));
        SHOWN.with_borrow_mut(|v| *v = shown);
    });
    free(ui);
    detail(ui);
}

/// The FREE NOW card.
fn free(ui: &MainWindow) {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_err() { return; }
    let gb = |b: u64| b as f64 / 1073741824.0;
    let d = ui.global::<MemHistData>();
    d.set_free(format!("{:.1} GB", gb(m.ullAvailPhys)).into());
    d.set_free_sub(format!("of {:.1} GB, {}% used", gb(m.ullTotalPhys), m.dwMemoryLoad).into());
}

/// The track behind a row on screen.
fn with_track<R>(row: i32, f: impl FnOnce(&Track) -> R) -> Option<R> {
    let i = SHOWN.with_borrow(|v| v.get(row.max(0) as usize).copied())?;
    STATE.with_borrow(|s| s.tracks.get(i).map(f))
}

/// The detail chart of the selected row.
pub fn detail(ui: &MainWindow) {
    let d = ui.global::<MemHistData>();
    let row = d.get_selected();
    let (events, end) = STATE.with_borrow(|s| (s.events.clone(), s.end));
    let done = with_track(row, |t| {
        let span = (t.t.first().copied().unwrap_or(0), t.t.last().copied().unwrap_or(0));
        let r = range(&t.v);
        let long = span.1 - span.0 > 20 * 3600;
        d.set_d_title(if t.pid == 0 { format!("Kernel pool · {}", t.label) } else { format!("{} (pid {}) · {}", t.name, t.pid, t.label) }.into());
        let now = if ended(t, end) {
            format!("Last {} at {} (ended: {})", amount(t.unit, t.last), local(span.1, long),
                    if t.metric.starts_with("gpu") { "process closed or its GPU unplugged" } else { "process closed" })
        } else { format!("Now {}", amount(t.unit, t.last)) };
        d.set_d_sub(match t.verdict {
            Some(v) => format!("{now} · {}/h · lows +{} in {:.1} h · R² {:.2} · up in {:.0}% of samples",
                               signed(t.unit, v.per_hour), amount(t.unit, v.floor_growth), v.span_h, v.r2, v.rising * 100.0),
            None => format!("{now} · {} in {:.1} h · no verdict",
                            signed(t.unit, t.v.last().unwrap_or(&0.0) - t.v.first().unwrap_or(&0.0)), (span.1 - span.0) as f64 / 3600.0),
        }.into());
        d.set_d_path(path(&t.t, &t.v, span, r).into());
        d.set_d_trend(if t.verdict.is_some() { trend(&t.t, &t.v, span, r) } else { String::new() }.into());
        d.set_d_top(amount(t.unit, r.1).into());
        d.set_d_bottom(amount(t.unit, r.0).into());
        d.set_d_start(local(span.0, long).into());
        d.set_d_end(local(span.1, long).into());
        let marks: Vec<Marker> = events.iter().filter(|(x, _)| *x >= span.0 && *x <= span.1 && span.1 > span.0)
            .map(|(x, label)| Marker { x: ((x - span.0) as f64 / (span.1 - span.0) as f64) as f32, label: format!("{} {label}", local(*x, long)).into() })
            .collect();
        d.set_d_markers(ModelRc::new(VecModel::from(marks)));
    });
    if done.is_none() {
        for f in [MemHistData::set_d_title, MemHistData::set_d_sub, MemHistData::set_d_path, MemHistData::set_d_trend] { f(&d, Default::default()); }
        d.set_d_markers(ModelRc::new(VecModel::from(Vec::<Marker>::new())));
    }
}

/// Watch / unwatch the row's process. Returns true when the page must reload (new series).
pub fn toggle_watch(row: i32) -> bool {
    let Some(name) = with_track(row, |t| (t.pid != 0).then(|| t.name.clone())).flatten() else { return false };
    let mut p = Prefs::load();
    let before = p.watched.len();
    p.watched.retain(|w| !w.eq_ignore_ascii_case(&name));
    if p.watched.len() == before { p.watched.push(name); }
    p.save().is_ok()
}

pub fn ignore(ui: &MainWindow, row: i32) {
    let Some(key) = with_track(row, ignore_key) else { return };
    let mut p = Prefs::load();
    if !p.ignored.contains(&key) { p.ignored.push(key); }
    let _ = p.save();
    render(ui);
}

pub fn clear_ignored(ui: &MainWindow) {
    let mut p = Prefs::load();
    p.ignored.clear();
    let _ = p.save();
    render(ui);
}

/// The process name of a row (Show in Apps).
pub fn process_name(row: i32) -> Option<String> { with_track(row, |t| (t.pid != 0).then(|| t.name.clone())).flatten() }

// ---------------------------------------------------------------- log drawer

/// Which feature a log line belongs to (0 all, 1 memory, 2 eGPU, 3 network, 4 service).
fn kind_of(msg: &str) -> i32 {
    match msg.split(':').next().unwrap_or("") {
        "ram" | "dwm" => 1,
        "egpu" => 2,
        "net" => 3,
        _ => 4,
    }
}

/// Splits a log's text into lines that start with a timestamp. Two writers can land on one line
/// (`...pid 8008)2026-11-05 18:51:49 ram: ...`), so a timestamp inside a line starts a new one.
pub fn split_lines(text: &str) -> Vec<String> {
    let is_stamp = |s: &[u8]| s.len() >= 20 && s[4] == b'-' && s[7] == b'-' && s[10] == b' ' && s[13] == b':' && s[16] == b':' && s[19] == b' '
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18].iter().all(|&i| s[i].is_ascii_digit());
    let mut out = Vec::new();
    for l in text.lines() {
        let b = l.as_bytes();
        let mut start = 0;
        for i in 1..b.len() {
            if l.is_char_boundary(i) && is_stamp(&b[i..]) {
                out.push(l[start..i].to_string());
                start = i;
            }
        }
        if !l[start..].trim().is_empty() { out.push(l[start..].to_string()); }
    }
    out
}

/// Lines of the service and tray logs (and their rotated `.1` files), oldest first, filtered.
pub fn log_lines(filter: i32, find: &str) -> (Vec<String>, String) {
    let mut all: Vec<(String, &str)> = Vec::new();
    let mut read = Vec::new();
    for (path, who) in [(keel_core::log::service_log_path(), "service"), (keel_core::log::user_log_path(), "tray")] {
        for p in [{ let mut o = path.as_os_str().to_owned(); o.push(".1"); std::path::PathBuf::from(o) }, path.clone()] {
            if let Ok(t) = std::fs::read_to_string(&p) {
                all.extend(split_lines(&t).into_iter().map(|l| (l, who)));
                read.push(p.display().to_string());
            }
        }
    }
    // Stable by timestamp: the two logs interleave in time.
    all.sort_by(|a, b| a.0.get(..19).cmp(&b.0.get(..19)));
    let find = find.trim().to_lowercase();
    let v: Vec<String> = all.into_iter().filter(|(l, _)| {
        let msg = l.get(20..).unwrap_or("");
        (filter == 0 || kind_of(msg) == filter) && (find.is_empty() || l.to_lowercase().contains(&find))
    }).map(|(l, who)| if who == "tray" { format!("{l}  [tray]") } else { l }).collect();
    (v, read.join(" · "))
}

pub fn show_log(ui: &MainWindow) {
    let d = ui.global::<MemHistData>();
    let (lines, files) = log_lines(d.get_log_filter(), &d.get_log_find());
    let n = lines.len();
    // The newest 1500 lines, newest first (a TextEdit with thousands of lines gets slow).
    let keep: Vec<&str> = lines[n.saturating_sub(1500)..].iter().rev().map(String::as_str).collect();
    d.set_log_text(keep.join("\n").into());
    d.set_log_note(if files.is_empty() { "No log files yet.".into() }
                   else { format!("newest first · {} of {n} lines · {files}", keep.len()).into() });
}

pub fn open_log_folder() {
    let _ = keel_core::cmd::windowed("explorer.exe").arg(keel_core::log::service_log_path().parent().unwrap_or(std::path::Path::new("C:\\"))).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glued_log_lines_are_split() {
        let v = split_lines("2026-11-05 18:51:49 egpu: guard: started (pid 1)2026-11-05 18:51:49 ram: started\n\n2026-11-05 18:52:00 net: ok");
        assert_eq!(v, ["2026-11-05 18:51:49 egpu: guard: started (pid 1)", "2026-11-05 18:51:49 ram: started", "2026-11-05 18:52:00 net: ok"]);
        assert_eq!(kind_of("ram: started"), 1);
        assert_eq!(kind_of("service: egpu: on"), 4);
    }

    #[test]
    fn paths_fit_the_box() {
        let p = path(&[0, 50, 100], &[10.0, 20.0, 30.0], (0, 100), (10.0, 30.0));
        assert_eq!(p, "M 0.0 200.0 L 300.0 100.0 L 600.0 0.0");
        assert_eq!(trend(&[0, 50, 100], &[10.0, 20.0, 30.0], (0, 100), (10.0, 30.0)), "M 0.0 200.0 L 600.0 0.0");
        assert_eq!(path(&[5], &[1.0], (5, 5), (0.0, 1.0)), "");
    }
}

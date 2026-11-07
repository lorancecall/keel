//! Display / power / device events for `watch`, so `leaks` can attribute dwm growth to them.
//! A hidden top-level window receives WM_DISPLAYCHANGE, WM_POWERBROADCAST and WM_DEVICECHANGE
//! (broadcasts do not reach message-only windows); its timers drive the 5-minute samples too, so
//! there is no polling loop. Each burst of events gets one GPU-memory snapshot of dwm / csrss
//! 15 s after it settles. TDRs (System log, Display 4101) are read at each sample.
use crate::observe;
use crate::{gpu, procs};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// One thing that happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub t: i64,
    /// display-mode | display-added | display-removed | adapter-added | adapter-removed | sleep | resume | tdr
    pub kind: String,
    pub detail: String,
}

/// dwm / csrss GPU memory on one adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comp {
    pub pid: u32,
    pub ct: i64,
    pub n: String,
    pub a: String,
    pub d: u64,
    pub s: u64,
}

/// A burst of events and the compositor snapshot taken after it settled (`t`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub t: i64,
    pub items: Vec<Item>,
    pub comp: Vec<Comp>,
    /// Kernel pool tags at the same moment (driver memory, e.g. the AMD display driver's DAL3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pool: Vec<crate::pool::Tag>,
}

/// Change of one kernel pool tag across an event burst.
#[derive(Debug, Clone, Serialize)]
pub struct PoolDelta {
    pub t: i64,
    pub what: String,
    pub tag: String,
    pub nonpaged: i64,
    pub paged: i64,
    /// Latest nonpaged value minus the pre-event value.
    pub still_held: Option<i64>,
}

/// Pure: per event burst, pool tags whose paged or nonpaged use moved by at least `min` bytes.
pub fn pool_deltas(samples: &[observe::Sample], records: &[Record], min: u64) -> Vec<PoolDelta> {
    let mut line: HashMap<String, Vec<(i64, u64, u64)>> = HashMap::new();
    for s in samples { for t in &s.k { line.entry(t.tag.clone()).or_default().push((s.t, t.paged, t.nonpaged)); } }
    for r in records { for t in &r.pool { line.entry(t.tag.clone()).or_default().push((r.t, t.paged, t.nonpaged)); } }
    for v in line.values_mut() { v.sort_by_key(|x| x.0); }
    let mut out = Vec::new();
    for r in records {
        let start = r.items.iter().map(|i| i.t).min().unwrap_or(r.t);
        let what = r.items.iter().map(|i| format!("{}: {}", i.kind, i.detail)).collect::<Vec<_>>().join("; ");
        for t in r.pool.iter().filter(|t| t.tag != "*ALL") {
            let Some(l) = line.get(&t.tag) else { continue };
            let Some(&(_, bp, bn)) = l.iter().rev().find(|x| x.0 < start) else { continue };
            let (dp, dn) = (t.paged as i64 - bp as i64, t.nonpaged as i64 - bn as i64);
            if dp.unsigned_abs() < min && dn.unsigned_abs() < min { continue; }
            let later = l.iter().rev().find(|x| x.0 > r.t).map(|x| x.2 as i64 - bn as i64);
            out.push(PoolDelta { t: start, what: what.clone(), tag: t.tag.clone(), nonpaged: dn, paged: dp, still_held: later });
        }
    }
    out.sort_by_key(|d| d.t);
    out
}

/// An attached display: `\\.\DISPLAY5` on an adapter, with its current mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Display { pub device: String, pub adapter: String, pub mode: String }

/// What changed between two display lists (pure).
pub fn display_changes(old: &[Display], new: &[Display], t: i64) -> Vec<Item> {
    let mut v = Vec::new();
    for n in new {
        match old.iter().find(|o| o.device == n.device) {
            None => v.push(Item { t, kind: "display-added".into(), detail: format!("{} {} on {}", n.device, n.mode, n.adapter) }),
            Some(o) if o.mode != n.mode || o.adapter != n.adapter =>
                v.push(Item { t, kind: "display-mode".into(), detail: format!("{} {} -> {} on {}", n.device, o.mode, n.mode, n.adapter) }),
            _ => {}
        }
    }
    for o in old.iter().filter(|o| !new.iter().any(|n| n.device == o.device)) {
        v.push(Item { t, kind: "display-removed".into(), detail: format!("{} {} on {}", o.device, o.mode, o.adapter) });
    }
    v
}

/// Adapters (key, name) that appeared or disappeared (pure; eGPU attach / detach).
pub fn adapter_changes(old: &[(String, String)], new: &[(String, String)], t: i64) -> Vec<Item> {
    let mut v: Vec<Item> = new.iter().filter(|n| !old.iter().any(|o| o.0 == n.0))
        .map(|(k, n)| Item { t, kind: "adapter-added".into(), detail: format!("{n} ({k})") }).collect();
    v.extend(old.iter().filter(|o| !new.iter().any(|n| n.0 == o.0))
        .map(|(k, n)| Item { t, kind: "adapter-removed".into(), detail: format!("{n} ({k})") }));
    v
}

/// (EventRecordID, unix time) of each event in `wevtutil qe /f:xml` output (pure).
pub fn parse_events_xml(xml: &str) -> Vec<(u64, i64)> {
    xml.split("<Event ").skip(1).filter_map(|e| {
        let id = e.split("<EventRecordID>").nth(1)?.split('<').next()?.trim().parse().ok()?;
        let ts = e.split("SystemTime='").nth(1).or_else(|| e.split("SystemTime=\"").nth(1))?;
        let ts = &ts[..ts.find(['\'', '"'])?];
        Some((id, chrono::DateTime::parse_from_rfc3339(ts).ok()?.timestamp()))
    }).collect()
}

/// Per event burst and compositor adapter: change of dedicated / shared memory against the last
/// observation before the burst, and how much of the dedicated rise is still held at the end.
#[derive(Debug, Clone, Serialize)]
pub struct Delta {
    pub t: i64,
    pub what: String,
    pub name: String,
    pub adapter: String,
    pub before_d: u64,
    pub after_d: u64,
    pub d: i64,
    pub s: i64,
    /// Latest dedicated value minus the pre-event value (None when nothing was seen later).
    pub still_held: Option<i64>,
}

/// Pure: samples + event records -> per-event deltas, oldest first.
pub fn deltas(samples: &[observe::Sample], records: &[Record]) -> Vec<Delta> {
    // Timeline per (pid, ct, adapter): (t, dedicated, shared), from samples and event snapshots.
    let mut line: HashMap<(u32, i64, String), Vec<(i64, u64, u64)>> = HashMap::new();
    let mut names: HashMap<(u32, i64), String> = HashMap::new();
    for s in samples {
        for p in s.p.iter().filter(|p| is_compositor(&p.n)) {
            names.insert((p.pid, p.ct), p.n.clone());
            for g in &p.g { line.entry((p.pid, p.ct, g.a.clone())).or_default().push((s.t, g.d, g.s)); }
        }
    }
    for r in records {
        for c in &r.comp {
            names.insert((c.pid, c.ct), c.n.clone());
            line.entry((c.pid, c.ct, c.a.clone())).or_default().push((r.t, c.d, c.s));
        }
    }
    for v in line.values_mut() { v.sort_by_key(|x| x.0); }
    let mut out = Vec::new();
    for r in records {
        let start = r.items.iter().map(|i| i.t).min().unwrap_or(r.t);
        let what = r.items.iter().map(|i| format!("{}: {}", i.kind, i.detail)).collect::<Vec<_>>().join("; ");
        for c in &r.comp {
            let Some(l) = line.get(&(c.pid, c.ct, c.a.clone())) else { continue };
            let Some(&(_, bd, bs)) = l.iter().rev().find(|x| x.0 < start) else { continue };
            let later = l.iter().rev().find(|x| x.0 > r.t).map(|x| x.1 as i64 - bd as i64);
            out.push(Delta {
                t: start, what: what.clone(), name: names.get(&(c.pid, c.ct)).cloned().unwrap_or_default(), adapter: c.a.clone(),
                before_d: bd, after_d: c.d, d: c.d as i64 - bd as i64, s: c.s as i64 - bs as i64, still_held: later,
            });
        }
    }
    out.sort_by_key(|d| d.t);
    out
}

pub fn is_compositor(name: &str) -> bool { matches!(name.to_lowercase().as_str(), "dwm.exe" | "csrss.exe") }

pub fn dir() -> PathBuf { observe::dir().with_file_name("events") }

pub fn store(r: &Record) -> anyhow::Result<()> {
    use std::io::Write;
    let d = dir();
    std::fs::create_dir_all(&d)?;
    let path = d.join(format!("{}.jsonl", chrono::Local::now().format("%Y%m%d")));
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(r)?)?;
    observe::prune(&d);
    Ok(())
}

/// Event records from the last `hours`.
pub fn load(hours: f64) -> Vec<Record> { observe::load_lines(&dir(), hours, |r: &Record| r.t) }

// ---- Win32 side ----

fn wide(b: &[u16]) -> String { String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]) }

/// Displays attached to the desktop now, with mode and adapter name.
pub fn displays() -> Vec<Display> {
    use windows::Win32::Graphics::Gdi::*;
    let mut v = Vec::new();
    for i in 0.. {
        let mut dd = DISPLAY_DEVICEW { cb: size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
        if !unsafe { EnumDisplayDevicesW(windows::core::PCWSTR::null(), i, &mut dd, 0) }.as_bool() { break; }
        if dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == DISPLAY_DEVICE_STATE_FLAGS(0) { continue; }
        let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
        let name = windows::core::PCWSTR(dd.DeviceName.as_ptr());
        let mode = if unsafe { EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool() {
            format!("{}x{}@{}", dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency)
        } else { "?".into() };
        v.push(Display { device: wide(&dd.DeviceName), adapter: wide(&dd.DeviceString), mode });
    }
    v
}

/// New TDRs (Display 4101) in the System log since the ids already seen.
fn new_tdrs(seen: &mut HashSet<u64>) -> Vec<Item> {
    let q = "/q:*[System[Provider[@Name='Display'] and (EventID=4101) and TimeCreated[timediff(@SystemTime) <= 1800000]]]";
    let Ok(o) = crate::cmd::hidden("wevtutil.exe").args(["qe", "System", q, "/c:20", "/rd:true", "/f:xml"]).output() else { return vec![] };
    parse_events_xml(&String::from_utf8_lossy(&o.stdout)).into_iter().filter(|(id, _)| seen.insert(*id))
        .map(|(id, t)| Item { t, kind: "tdr".into(), detail: format!("Display 4101, record {id}") }).collect()
}

fn compositor_snapshot() -> Vec<Comp> {
    let Ok(ps) = procs::snapshot() else { return vec![] };
    let comp: HashMap<u32, (i64, String)> = ps.into_iter().filter(|p| is_compositor(&p.name)).map(|p| (p.pid, (p.create_time, p.name))).collect();
    let mut v: Vec<Comp> = gpu::process_memory().into_iter().filter_map(|((pid, a), m)| {
        let (ct, n) = comp.get(&pid)?.clone();
        (m.dedicated + m.shared > 0).then_some(Comp { pid, ct, n, a, d: m.dedicated, s: m.shared })
    }).collect();
    v.sort_by(|x, y| (x.pid, &x.a).cmp(&(y.pid, &y.a)));
    v
}

struct State {
    displays: Vec<Display>,
    adapters: Vec<(String, String)>,
    pending: Vec<Item>,
    tdr_seen: HashSet<u64>,
    first_tdr_check: bool,
    last_sample: std::time::Instant,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
const TIMER_SAMPLE: usize = 1;
const TIMER_SETTLE: usize = 2;
const SETTLE_MS: u32 = 15_000;

fn now() -> i64 { chrono::Utc::now().timestamp() }

/// Compares displays / adapters with the last known sets and queues what changed.
fn check_sets(st: &mut State) -> bool {
    let t = now();
    let (d, a) = (displays(), gpu::adapters());
    let mut items = display_changes(&st.displays, &d, t);
    items.extend(adapter_changes(&st.adapters, &a, t));
    st.displays = d;
    st.adapters = a;
    let any = !items.is_empty();
    st.pending.extend(items);
    any
}

fn flush(st: &mut State) {
    if st.pending.is_empty() { return; }
    let r = Record { t: now(), items: std::mem::take(&mut st.pending), comp: compositor_snapshot(), pool: crate::pool::top(observe::POOL_MIN) };
    for i in &r.items { println!("{} event {}: {}", chrono::Local::now().format("%H:%M:%S"), i.kind, i.detail); }
    if let Err(e) = store(&r) { eprintln!("event store failed: {e}"); }
}

fn sample(st: &mut State) -> Option<observe::Sample> {
    let s = match observe::take().and_then(|s| observe::store(&s, keel_core::traycfg::Prefs::load().keep_memory_history).map(|_| s)) {
        Ok(s) => { println!("{} sample: {} processes", chrono::Local::now().format("%H:%M"), s.p.len()); Some(s) }
        Err(e) => { eprintln!("{} sample failed: {e}", chrono::Local::now().format("%H:%M")); None }
    };
    // Fallback for missed broadcasts, plus TDRs; the first check only learns the existing ids.
    check_sets(st);
    let tdrs = new_tdrs(&mut st.tdr_seen);
    if !std::mem::replace(&mut st.first_tdr_check, false) { st.pending.extend(tdrs); }
    flush(st);
    observe::trim_self();
    s
}

unsafe extern "system" fn wndproc(hwnd: windows::Win32::Foundation::HWND, msg: u32, wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, KillTimer, SetTimer};
    const WM_DISPLAYCHANGE: u32 = 0x007E;
    const WM_POWERBROADCAST: u32 = 0x0218;
    const WM_DEVICECHANGE: u32 = 0x0219;
    const WM_TIMER: u32 = 0x0113;
    const DBT_DEVNODES_CHANGED: usize = 0x0007;
    // A message delivered while this thread already holds the state (e.g. pumped during a
    // sample) is not ours to handle again: let Windows have it.
    let Ok(mut guard) = STATE.try_lock() else { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) } };
    let Some(st) = guard.as_mut() else { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) } };
    let settle_soon = |hwnd| unsafe { SetTimer(Some(hwnd), TIMER_SETTLE, SETTLE_MS, None); };
    match msg {
        WM_DISPLAYCHANGE => { check_sets(st); settle_soon(hwnd); }
        WM_DEVICECHANGE if wp.0 == DBT_DEVNODES_CHANGED => { if check_sets(st) { settle_soon(hwnd); } }
        WM_POWERBROADCAST => {
            let kind = match wp.0 { 0x4 => Some("sleep"), 0x7 | 0x12 => Some("resume"), _ => None };
            if let Some(k) = kind {
                if k == "resume" && st.pending.iter().any(|i| i.kind == "resume") { return windows::Win32::Foundation::LRESULT(1); }
                st.pending.push(Item { t: now(), kind: k.into(), detail: String::new() });
                if k == "resume" { settle_soon(hwnd); }
            }
            return windows::Win32::Foundation::LRESULT(1);
        }
        WM_TIMER if wp.0 == TIMER_SETTLE => { let _ = unsafe { KillTimer(Some(hwnd), TIMER_SETTLE) }; flush(st); observe::trim_self(); }
        WM_TIMER if wp.0 == TIMER_SAMPLE => {
            let every = if keel_core::traycfg::Prefs::load().fast_samples { observe::FAST_INTERVAL_SECS } else { observe::INTERVAL_SECS };
            // A little slack so a 5-minute interval is not stretched to 6 by timer jitter.
            if st.last_sample.elapsed().as_secs() + 5 < every { drop(guard); return unsafe { DefWindowProcW(hwnd, msg, wp, lp) }; }
            st.last_sample = std::time::Instant::now();
            let s = sample(st);
            drop(guard);
            if let Some(s) = s { run_after_sample(&s); } // outside the lock: the hook may pump messages back into this proc
            return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
        }
        _ => {}
    }
    drop(guard);
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// `watch`: hidden window + message loop; samples every 5 minutes, events as they come.
/// Called on the watch thread with each stored sample (the tray checks dwm's system RAM).
static AFTER_SAMPLE: Mutex<Option<fn(&observe::Sample)>> = Mutex::new(None);

pub fn set_after_sample(f: fn(&observe::Sample)) { *AFTER_SAMPLE.lock().unwrap() = Some(f); }

fn run_after_sample(s: &observe::Sample) {
    let after = *AFTER_SAMPLE.lock().unwrap();
    if let Some(f) = after { f(s); }
}

/// Creates the hidden event window and the sample timer on the calling thread and takes the
/// first sample. The caller must pump messages on this thread (`watch` does; so does the tray).
pub fn start() -> anyhow::Result<()> {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::w;
    *STATE.lock().unwrap() = Some(State { displays: displays(), adapters: gpu::adapters(), pending: vec![], tdr_seen: HashSet::new(), first_tdr_check: true,
                                          last_sample: std::time::Instant::now() });
    unsafe {
        let inst = GetModuleHandleW(None)?;
        let class = w!("KeelWatch");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: class, ..Default::default() };
        if RegisterClassW(&wc) == 0 { anyhow::bail!("RegisterClassW failed"); }
        // Top-level but never shown: broadcasts reach it, the taskbar never sees it.
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class, w!("Keel watch"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(inst.into()), None)?;
        let s = STATE.lock().unwrap().as_mut().and_then(sample);
        if let Some(s) = s { run_after_sample(&s); }
        // Every minute; a sample is taken when the user's interval (1 or 5 minutes) is due.
        SetTimer(Some(hwnd), TIMER_SAMPLE, (observe::FAST_INTERVAL_SECS * 1000) as u32, None);
    }
    Ok(())
}

/// `watch` (console): the event window plus a plain message loop.
pub fn watch() -> anyhow::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    start()?;
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::{GRow, PRow, Sample};

    fn disp(dev: &str, mode: &str, ad: &str) -> Display { Display { device: dev.into(), adapter: ad.into(), mode: mode.into() } }

    #[test]
    fn describes_display_and_adapter_changes() {
        let old = vec![disp(r"\\.\DISPLAY1", "1280x800@60", "AMD Radeon Graphics"), disp(r"\\.\DISPLAY5", "3840x2160@60", "AMD Radeon RX 7600M XT")];
        let new = vec![disp(r"\\.\DISPLAY1", "1280x800@60", "AMD Radeon Graphics"), disp(r"\\.\DISPLAY5", "3840x2160@50", "AMD Radeon RX 7600M XT")];
        let c = display_changes(&old, &new, 9);
        assert_eq!(c, vec![Item { t: 9, kind: "display-mode".into(), detail: r"\\.\DISPLAY5 3840x2160@60 -> 3840x2160@50 on AMD Radeon RX 7600M XT".into() }]);
        assert_eq!(display_changes(&old, &old[..1], 1)[0].kind, "display-removed");
        assert!(display_changes(&old, &old, 1).is_empty());
        let a0 = vec![("k1".to_string(), "iGPU".to_string())];
        let a1 = vec![("k1".to_string(), "iGPU".to_string()), ("k2".to_string(), "eGPU".to_string())];
        assert_eq!(adapter_changes(&a0, &a1, 1)[0].kind, "adapter-added");
        assert_eq!(adapter_changes(&a1, &a0, 1)[0].detail, "eGPU (k2)");
    }

    #[test]
    fn parses_tdr_events() {
        let xml = "<Event xmlns='x'><System><Provider Name='Display'/><EventID>4101</EventID><TimeCreated SystemTime='2026-11-04T14:00:05.1234567Z'/><EventRecordID>777</EventRecordID></System></Event><Event xmlns='x'><System><TimeCreated SystemTime='2026-11-04T15:00:00.0000000Z'/><EventRecordID>778</EventRecordID></System></Event>";
        let v = parse_events_xml(xml);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], (777, chrono::DateTime::parse_from_rfc3339("2026-11-04T14:00:05Z").unwrap().timestamp()));
        assert!(parse_events_xml("").is_empty());
    }

    /// A measurement: 60 -> 50 -> 60 Hz on the eGPU raised dwm's dedicated VRAM
    /// 1065 -> 1178 -> 1289 MB and it was still there later; shared stayed at 2745 MB.
    #[test]
    fn per_event_deltas_and_what_stays_held() {
        const M: u64 = 1 << 20;
        let a = "00000000_00019333".to_string();
        let tag = |np: u64| vec![crate::pool::Tag { tag: "DAL3".into(), paged: 0, nonpaged: np * M }];
        let s = |t: i64, d: u64, np: u64| Sample { t, free: None, adapters: vec![], p: vec![PRow { pid: 2636, ct: 1, n: "dwm.exe".into(), pb: 0, ws: 0, h: 0, pws: 0,
            g: vec![GRow { a: a.clone(), d: d * M, s: 2745 * M, c: 0 }] }], k: tag(np) };
        let rec = |t: i64, d: u64, np: u64, detail: &str| Record { t, items: vec![Item { t: t - 15, kind: "display-mode".into(), detail: detail.into() }],
            comp: vec![Comp { pid: 2636, ct: 1, n: "dwm.exe".into(), a: a.clone(), d: d * M, s: 2745 * M }], pool: tag(np) };
        let samples = vec![s(0, 1065, 420), s(600, 1289, 440)];
        let records = vec![rec(100, 1178, 430, "60 -> 50"), rec(200, 1289, 440, "50 -> 60")];
        let d = deltas(&samples, &records);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].d / M as i64, d[1].d / M as i64), (113, 111));
        assert!(d.iter().all(|x| x.s == 0), "shared memory did not move");
        assert_eq!(d[0].still_held, Some(224 * M as i64));
        assert_eq!(d[1].still_held, Some(111 * M as i64));
        let p = pool_deltas(&samples, &records, 4 << 20);
        assert_eq!(p.iter().map(|x| (x.tag.as_str(), x.nonpaged / M as i64)).collect::<Vec<_>>(), [("DAL3", 10), ("DAL3", 10)]);
        assert_eq!(p[0].still_held, Some(20 * M as i64));
    }
}

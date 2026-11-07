//! Observation: a compact sample of every sizeable process every 5 minutes (memory,
//! handles, GPU memory per adapter) appended to daily JSON-lines files, and the `leaks` report.
use crate::leak::{self, Level, Rule, Verdict};
use crate::{gpu, procs};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

/// Samples come this often by default ...
pub const INTERVAL_SECS: u64 = 300;
/// ... or every minute with the user's "sample every minute" (Memory history).
pub const FAST_INTERVAL_SECS: u64 = 60;
const KEEP_DAYS: i64 = 7;
/// A process is recorded when any of these is reached (keeps a sample around 10-20 KB).
const MIN_BYTES: u64 = 16 << 20;
const MIN_HANDLES: u32 = 1000;
/// Kernel pool tags at or above this are recorded.
pub const POOL_MIN: u64 = 8 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GRow {
    /// Adapter key (LUID, see `gpu::luid_key`).
    pub a: String,
    pub d: u64,
    pub s: u64,
    pub c: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PRow {
    pub pid: u32,
    /// Process create time (FILETIME ticks): pid + ct identifies one process across samples.
    pub ct: i64,
    pub n: String,
    /// Private bytes (commit), working set, handle count.
    pub pb: u64,
    pub ws: u64,
    pub h: u32,
    /// Private working set: what Task Manager's Memory column shows (0 in samples before 0.1.10).
    #[serde(default)]
    pub pws: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub g: Vec<GRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// Unix seconds.
    pub t: i64,
    /// Adapter key -> name.
    pub adapters: Vec<(String, String)>,
    pub p: Vec<PRow>,
    /// Kernel pool tags using >= 8 MB, plus `*ALL` totals (driver memory no process owns).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub k: Vec<crate::pool::Tag>,
    /// Free physical memory (bytes) at the sample: Home's memory graph starts from these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free: Option<u64>,
}

pub fn dir() -> PathBuf {
    crate::paths::user_data_dir().join("samples")
}

/// Takes one sample now.
pub fn take() -> anyhow::Result<Sample> {
    let ps = procs::snapshot()?;
    let gmem = gpu::process_memory();
    let mut by_pid: HashMap<u32, Vec<GRow>> = HashMap::new();
    for ((pid, a), m) in gmem {
        if m.dedicated + m.shared + m.committed > 0 {
            by_pid.entry(pid).or_default().push(GRow { a, d: m.dedicated, s: m.shared, c: m.committed });
        }
    }
    let p = ps.into_iter().filter_map(|p| {
        let mut g = by_pid.remove(&p.pid).unwrap_or_default();
        g.sort_by(|x, y| x.a.cmp(&y.a));
        let gmax = g.iter().map(|r| r.c.max(r.d + r.s)).max().unwrap_or(0);
        (p.private_bytes >= MIN_BYTES || p.handles >= MIN_HANDLES || gmax >= MIN_BYTES)
            .then(|| PRow { pid: p.pid, ct: p.create_time, n: p.name, pb: p.private_bytes, ws: p.working_set, h: p.handles, pws: p.private_ws, g })
    }).collect();
    Ok(Sample { t: chrono::Utc::now().timestamp(), adapters: gpu::adapters(), p, k: crate::pool::top(POOL_MIN), free: free_memory() })
}

/// Free physical memory now.
pub fn free_memory() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { GlobalMemoryStatusEx(&mut m) }.ok().map(|_| m.ullAvailPhys)
}

/// Free memory over this session's samples (unix seconds, bytes), oldest first; empty before the
/// first sample that records it.
pub fn session_free() -> Vec<(i64, u64)> {
    sessions().last().map(|p| load_file(p).into_iter().filter_map(|s| s.free.map(|f| (s.t, f))).collect()).unwrap_or_default()
}

/// This process's session file (`session-YYYYMMDD-HHMMSS.jsonl`, named when it first stores).
static SESSION: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Appends a sample to this session's file (a session = one run of the tray, i.e. one logon).
/// Without the user's "keep memory history" (`keep_all` false) only this session and the one
/// before it are kept; with it, sessions of the last week.
pub fn store(s: &Sample, keep_all: bool) -> anyhow::Result<PathBuf> {
    let d = dir();
    std::fs::create_dir_all(&d)?;
    let path = SESSION.get_or_init(|| d.join(format!("session-{}.jsonl", chrono::Local::now().format("%Y%m%d-%H%M%S")))).clone();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(s)?)?;
    prune(&d);
    prune_sessions(&d, if keep_all { None } else { Some(2) });
    Ok(path)
}

/// Store into the newest existing session instead of starting one (`keel ram sample`).
pub fn join_latest_session() {
    if let Some(p) = sessions().pop() { let _ = SESSION.set(p); }
}

/// Session files in `d`, oldest first.
pub fn sessions_in(d: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("session-") && n.ends_with(".jsonl"))).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Session files of the samples folder, oldest first.
pub fn sessions() -> Vec<PathBuf> { sessions_in(&dir()) }

/// Keeps the newest `keep` sessions (None: those of the last week).
fn prune_sessions(d: &std::path::Path, keep: Option<usize>) {
    let v = sessions_in(d);
    let cutoff = format!("session-{}", (chrono::Local::now() - chrono::Duration::days(KEEP_DAYS)).format("%Y%m%d"));
    for (i, p) in v.iter().enumerate() {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let old = match keep { Some(k) => i + k < v.len(), None => name < cutoff.as_str() };
        if old && Some(p) != SESSION.get() { let _ = std::fs::remove_file(p); }
    }
}

/// All samples of one file, oldest first.
pub fn load_file(p: &std::path::Path) -> Vec<Sample> {
    let mut v: Vec<Sample> = std::fs::read_to_string(p).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    v.sort_by_key(|s| s.t);
    v
}

/// Drops `YYYYMMDD.jsonl` files older than a week from `d`.
pub fn prune(d: &std::path::Path) {
    let cutoff = (chrono::Local::now() - chrono::Duration::days(KEEP_DAYS)).format("%Y%m%d").to_string();
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".jsonl") {
            if stem.len() == 8 && stem.chars().all(|c| c.is_ascii_digit()) && *stem < *cutoff.as_str() { let _ = std::fs::remove_file(e.path()); }
        }
    }
}

/// JSON lines of type `T` from the daily files in `d` within the last `hours`, oldest first.
pub fn load_lines<T: serde::de::DeserializeOwned>(d: &std::path::Path, hours: f64, time: impl Fn(&T) -> i64) -> Vec<T> {
    let since = chrono::Utc::now().timestamp() - (hours * 3600.0) as i64;
    let mut files: Vec<PathBuf> = std::fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect()).unwrap_or_default();
    files.sort();
    let mut v: Vec<T> = files.iter().flat_map(|f| std::fs::read_to_string(f).unwrap_or_default()
        .lines().filter_map(|l| serde_json::from_str::<T>(l).ok()).collect::<Vec<_>>())
        .filter(|s| time(s) >= since).collect();
    v.sort_by_key(|s| time(s));
    v
}

/// Samples from the last `hours`, oldest first.
pub fn load(hours: f64) -> Vec<Sample> { load_lines(&dir(), hours, |s: &Sample| s.t) }

/// Lowers this process's own footprint between samples (`watch`).
pub fn trim_self() {
    use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe { let _ = K32EmptyWorkingSet(GetCurrentProcess()); }
}

/// One finding of the report.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub pid: u32,
    pub name: String,
    /// memory | handles | gpu:<adapter> | pool-paged:<tag> | pool-nonpaged:<tag>
    pub metric: String,
    pub unit: &'static str,
    pub last: f64,
    pub verdict: Verdict,
}

type Key = (u32, i64);

/// Per-process series of every metric: metric -> (hours since first sample, value).
pub fn series(samples: &[Sample]) -> HashMap<Key, (String, HashMap<String, (Vec<f64>, Vec<f64>)>)> {
    let mut out: HashMap<Key, (String, HashMap<String, (Vec<f64>, Vec<f64>)>)> = HashMap::new();
    let Some(t0) = samples.first().map(|s| s.t) else { return out };
    for s in samples {
        let h = (s.t - t0) as f64 / 3600.0;
        for p in &s.p {
            let e = out.entry((p.pid, p.ct)).or_insert_with(|| (p.n.clone(), HashMap::new()));
            let mut push = |k: String, v: f64| { let x = e.1.entry(k).or_default(); x.0.push(h); x.1.push(v); };
            // Memory as Task Manager shows it (private working set: what the process holds in RAM).
            // Private bytes stay in the samples (the dwm alert uses them) but are not judged: paged-out
            // commit does not take RAM and made the page disagree with Task Manager.
            if p.pws > 0 { push("memory".into(), p.pws as f64); }
            push("handles".into(), p.h as f64);
            // GPU memory: the process's committed allocations. Its "Dedicated Usage" also counts
            // surfaces it only opened (dwm: 3 GB against 0.43 GB committed).
            for g in &p.g { if g.c > 0 { push(format!("gpu:{}", g.a), g.c as f64); } }
        }
        // Kernel pool (driver memory) as a pseudo process, pid 0.
        if !s.k.is_empty() {
            let e = out.entry(KERNEL).or_insert_with(|| ("kernel pool".into(), HashMap::new()));
            for t in &s.k {
                for (kind, v) in [("pool-nonpaged", t.nonpaged), ("pool-paged", t.paged)] {
                    if v == 0 { continue; }
                    let x = e.1.entry(format!("{kind}:{}", t.tag)).or_default();
                    x.0.push(h);
                    x.1.push(v as f64);
                }
            }
        }
    }
    out
}

/// Series key of the kernel pool pseudo process.
pub const KERNEL: Key = (0, -1);

fn rule_for(metric: &str) -> (&'static Rule, &'static str) {
    if metric == "handles" { (&leak::HANDLES, "handles") }
    else if metric.starts_with("gpu") { (&leak::GPU, "bytes") }
    else if metric.starts_with("pool") { (&leak::POOL, "bytes") }
    else { (&leak::MEMORY, "bytes") }
}

/// Leak / suspect findings, leaks first, then by growth rate.
pub fn findings(samples: &[Sample]) -> Vec<Finding> {
    let mut v = Vec::new();
    for ((pid, _), (name, metrics)) in series(samples) {
        for (metric, (t, val)) in metrics {
            let (rule, unit) = rule_for(&metric);
            if let Some(verdict) = leak::judge(&t, &val, rule) {
                v.push(Finding { pid, name: name.clone(), metric, unit, last: *val.last().unwrap_or(&0.0), verdict });
            }
        }
    }
    v.sort_by(|a, b| (a.verdict.level != Level::Leak).cmp(&(b.verdict.level != Level::Leak))
        .then(b.verdict.per_hour.partial_cmp(&a.verdict.per_hour).unwrap_or(std::cmp::Ordering::Equal)));
    v
}

/// One row of the main window's Memory history: a finding, or a process the user watches, with
/// its series so the page can draw it.
#[derive(Debug, Clone)]
pub struct Track {
    pub pid: u32,
    pub name: String,
    pub metric: String,
    /// Readable metric (`memory (as Task Manager)`, `GPU memory on <adapter>`, `pool-nonpaged DAL3 (AMD ...)`).
    pub label: String,
    pub unit: &'static str,
    pub last: f64,
    /// None: watched by the user or among the largest now, no verdict.
    pub verdict: Option<Verdict>,
    /// The verdict comes from the early rule (less than 2 h of samples): a sign, not a finding.
    pub early: bool,
    /// Shown because it is among the largest processes in the latest sample (not watched).
    pub top: bool,
    /// Unix seconds and value of each point.
    pub t: Vec<i64>,
    pub v: Vec<f64>,
}

/// Early signs: the same test over at least 20 minutes and 4 samples, before a verdict is possible.
fn early_rule(r: &Rule) -> Rule { Rule { min_span_h: 1.0 / 3.0, min_points: 4, ..*r } }

/// Findings (and early signs while the window is shorter than a verdict needs), the memory of
/// every process named in `watched` and of the `top` largest processes now (largest instance per
/// name): leaks, rising, early signs, watched, largest; by growth within each.
pub fn tracks(samples: &[Sample], watched: &[String], top: usize) -> Vec<Track> {
    let t0 = samples.first().map_or(0, |s| s.t);
    let mut largest: Vec<&PRow> = samples.last().map(|s| s.p.iter().filter(|p| p.pws > 0).collect()).unwrap_or_default();
    largest.sort_by(|a, b| b.pws.cmp(&a.pws));
    let largest: std::collections::HashSet<Key> = largest.iter().take(top).map(|p| (p.pid, p.ct)).collect();
    let mut v: Vec<Track> = Vec::new();
    for (key, (name, metrics)) in series(samples) {
        let watch = watched.iter().any(|w| w.eq_ignore_ascii_case(&name));
        let big = largest.contains(&key);
        for (metric, (t, val)) in metrics {
            let (rule, unit) = rule_for(&metric);
            let mut early = false;
            let mut verdict = leak::judge(&t, &val, rule);
            let span = t.last().unwrap_or(&0.0) - t.first().unwrap_or(&0.0);
            if verdict.is_none() && span < rule.min_span_h {
                verdict = leak::judge(&t, &val, &early_rule(rule));
                early = verdict.is_some();
            }
            if verdict.is_none() && !((watch || big) && metric == "memory") { continue; }
            v.push(Track { pid: key.0, label: label(samples, &metric), name: name.clone(), metric, unit, last: *val.last().unwrap_or(&0.0),
                           verdict, early, top: big && !watch, t: t.iter().map(|h| t0 + (h * 3600.0).round() as i64).collect(), v: val });
        }
    }
    // One row per name without a verdict (the largest instance).
    v.sort_by(|a, b| b.last.total_cmp(&a.last));
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| x.verdict.is_some() || seen.insert(x.name.to_lowercase()));
    let rank = |x: &Track| match (x.verdict.map(|v| v.level), x.early, x.top) {
        (Some(Level::Leak), false, _) => 0, (Some(_), false, _) => 1, (Some(_), true, _) => 2, (None, _, false) => 3, (None, _, true) => 4,
    };
    v.sort_by(|a, b| rank(a).cmp(&rank(b))
        .then(b.verdict.map_or(0.0, |v| v.per_hour).total_cmp(&a.verdict.map_or(0.0, |v| v.per_hour)))
        .then(b.last.total_cmp(&a.last)));
    v
}

fn adapter_name(samples: &[Sample], key: &str) -> String {
    samples.iter().rev().flat_map(|s| s.adapters.iter()).find(|(k, _)| k == key).map(|(_, n)| n.clone())
        .unwrap_or_else(|| format!("adapter {key}"))
}

fn label(samples: &[Sample], metric: &str) -> String {
    match metric.split_once(':') {
        Some((m, tag)) if m.starts_with("pool") => match crate::pool::owner(tag) {
            Some(o) => format!("{m} {tag} ({o})"),
            None => format!("{m} {tag}"),
        },
        Some((_, a)) => format!("GPU memory on {}", adapter_name(samples, a)),
        None if metric == "memory" => "memory (as Task Manager)".into(),
        None => metric.to_string(),
    }
}

fn amount(unit: &str, v: f64) -> String {
    if unit == "bytes" { format!("{:.0} MB", v / leak::MB) } else { format!("{v:.0}") }
}

/// Top GPU-memory users per adapter in the latest sample, with their change over the window.
/// Per row: pid, name, committed GPU memory, change since first seen.
pub fn gpu_top(samples: &[Sample], n: usize) -> Vec<(String, Vec<(u32, String, u64, i64)>)> {
    let Some(last) = samples.last() else { return vec![] };
    let first_seen: HashMap<(Key, String), u64> = samples.iter().rev().flat_map(|s| s.p.iter()
        .flat_map(|p| p.g.iter().map(move |g| (((p.pid, p.ct), g.a.clone()), g.c)))).collect();
    let mut per: HashMap<String, Vec<(u32, String, u64, i64)>> = HashMap::new();
    for p in &last.p {
        for g in p.g.iter().filter(|g| g.c >= 1 << 20) {
            let start = first_seen.get(&((p.pid, p.ct), g.a.clone())).copied().unwrap_or(g.c);
            per.entry(g.a.clone()).or_default().push((p.pid, p.n.clone(), g.c, g.c as i64 - start as i64));
        }
    }
    let mut out: Vec<_> = per.into_iter().map(|(a, mut v)| {
        v.sort_by(|x, y| y.2.cmp(&x.2));
        v.truncate(n);
        (adapter_name(samples, &a), v)
    }).collect();
    out.sort_by(|a, b| b.1.iter().map(|x| x.2).sum::<u64>().cmp(&a.1.iter().map(|x| x.2).sum::<u64>()));
    out
}

/// Prints the `leaks` report. `family_of` maps live pids to their family name.
pub fn print_report(samples: &[Sample], records: &[crate::events::Record], hours: f64, family_of: &HashMap<u32, String>) {
    print_findings(samples, hours, family_of);
    let ds = crate::events::deltas(samples, records);
    println!("\ndisplay / power / driver events: {} in the window (dwm / csrss GPU memory change per event and adapter, MB)", records.len());
    if records.is_empty() {
        println!("  none recorded yet (`keel ram watch` records them as they happen)");
        return;
    }
    println!("  {:<8} {:<10} {:<24} {:>13} {:>8} {:>8} {:>11}  EVENT", "TIME", "PROCESS", "ADAPTER", "DEDIC BEFORE>AFTER", "", "SHARED", "STILL HELD");
    let mb = |v: i64| v / (1 << 20);
    for d in &ds {
        let t = chrono::DateTime::from_timestamp(d.t, 0).map(|x| x.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default();
        println!("  {:<8} {:<10} {:<24} {:>6}>{:<6} {:>+8} {:>+8} {:>11}  {}", t, crate::scan::clip(&d.name, 10),
            crate::scan::clip(&adapter_name(samples, &d.adapter), 24), d.before_d >> 20, d.after_d >> 20, mb(d.d), mb(d.s),
            d.still_held.map(|h| format!("{:+}", mb(h))).unwrap_or_else(|| "-".into()), crate::scan::clip(&d.what, 70));
    }
    let mut by: HashMap<(String, String), (i64, usize)> = HashMap::new();
    for d in ds.iter().filter(|d| d.what.contains("display-mode")) {
        let e = by.entry((d.name.clone(), d.adapter.clone())).or_default();
        e.0 += d.d;
        e.1 += 1;
    }
    for ((name, a), (sum, n)) in by {
        println!("  {name} on {}: {n} mode changes, {:+} MB dedicated in total", adapter_name(samples, &a), mb(sum));
    }
    let pd = crate::events::pool_deltas(samples, records, 4 << 20);
    if !pd.is_empty() {
        println!("\n  kernel pool per event (driver memory in system RAM, MB; >= 4 MB moves only):");
        println!("  {:<8} {:<6} {:<36} {:>9} {:>7} {:>11}  EVENT", "TIME", "TAG", "OWNER", "NONPAGED", "PAGED", "STILL HELD");
        for d in &pd {
            let t = chrono::DateTime::from_timestamp(d.t, 0).map(|x| x.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default();
            println!("  {:<8} {:<6} {:<36} {:>+9} {:>+7} {:>11}  {}", t, d.tag, crate::scan::clip(crate::pool::owner(&d.tag).unwrap_or("-"), 36),
                mb(d.nonpaged), mb(d.paged), d.still_held.map(|h| format!("{:+}", mb(h))).unwrap_or_else(|| "-".into()), crate::scan::clip(&d.what, 60));
        }
    }
}

fn print_findings(samples: &[Sample], hours: f64, family_of: &HashMap<u32, String>) {
    let span = match (samples.first(), samples.last()) { (Some(a), Some(b)) => (b.t - a.t) as f64 / 3600.0, _ => 0.0 };
    println!("{} samples over {:.1} h (window {hours} h; samples every {} min in {})", samples.len(), span, INTERVAL_SECS / 60, dir().display());
    let fs = findings(samples);
    if span < leak::MEMORY.min_span_h {
        println!("not enough history for a verdict yet (needs >= {} h of samples); keep `keel ram watch` running (the Keel tray hosts it).", leak::MEMORY.min_span_h);
    } else if fs.is_empty() {
        println!("no process shows sustained growth.");
    } else {
        println!("\n{:<8} {:>6} {:<24} {:<20} {:<40} {:>10} {:>10} {:>5} {:>6}", "VERDICT", "PID", "NAME", "FAMILY", "METRIC", "PER HOUR", "NOW", "R2", "RISING");
        for f in fs.iter().take(30) {
            println!("{:<8} {:>6} {:<24} {:<20} {:<40} {:>10} {:>10} {:>5.2} {:>5.0}%",
                if f.verdict.level == Level::Leak { "LEAK" } else { "suspect" }, f.pid, crate::scan::clip(&f.name, 24),
                crate::scan::clip(family_of.get(&f.pid).map(String::as_str).unwrap_or("-"), 20), crate::scan::clip(&label(samples, &f.metric), 40),
                amount(f.unit, f.verdict.per_hour), amount(f.unit, f.last), f.verdict.r2, f.verdict.rising * 100.0);
        }
    }
    // dwm / csrss memory is mostly other apps' surfaces: show who holds GPU memory on each adapter.
    let comp = samples.last().map(|s| s.p.iter().any(|p| matches!(p.n.to_lowercase().as_str(), "dwm.exe" | "csrss.exe"))).unwrap_or(false);
    if comp {
        println!("\nGPU memory by adapter (latest sample; dwm.exe composes other apps' surfaces, so look at who sits next to it):");
        for (adapter, rows) in gpu_top(samples, 8) {
            println!("  {adapter}");
            println!("    {:>6} {:<26} {:<20} {:>12} {:>11}", "PID", "NAME", "FAMILY", "COMMITTED MB", "CHANGE MB");
            for (pid, name, c, delta) in rows {
                println!("    {:>6} {:<26} {:<20} {:>12} {:>+11}", pid, crate::scan::clip(&name, 26),
                    crate::scan::clip(family_of.get(&pid).map(String::as_str).unwrap_or("-"), 20), c >> 20, delta >> 20);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: i64, pb: u64, gd: u64) -> Sample {
        Sample { t, free: None, adapters: vec![("00000000_00019333".into(), "AMD Radeon RX 7600M XT".into())], p: vec![
            PRow { pid: 10, ct: 1, n: "dwm.exe".into(), pb, ws: 0, h: 900, pws: pb, g: vec![GRow { a: "00000000_00019333".into(), d: gd, s: 0, c: gd }] },
            PRow { pid: 20, ct: 5, n: "steady.exe".into(), pb: 300 << 20, ws: 0, h: 500, pws: 300 << 20, g: vec![] },
        ], k: vec![crate::pool::Tag { tag: "DAL3".into(), paged: 0, nonpaged: (400 + (t as u64 / 300) * 2) << 20 }] }
    }

    #[test]
    fn kernel_pool_tags_are_judged_like_a_process() {
        let samples: Vec<Sample> = (0..48).map(|i| sample(i * 300, 1000 << 20, 500 << 20)).collect(); // DAL3 +2 MB / 5 min
        let f = findings(&samples);
        let pool = f.iter().find(|x| x.metric == "pool-nonpaged:DAL3").expect("DAL3 growth found");
        assert_eq!((pool.pid, pool.name.as_str()), (0, "kernel pool"));
        assert_eq!(label(&samples, &pool.metric), "pool-nonpaged DAL3 (AMD display driver (amdkmdag.sys))");
    }

    #[test]
    fn finds_growing_process_and_metrics_from_samples() {
        let samples: Vec<Sample> = (0..48).map(|i| sample(i * 300, (1000 + i as u64 * 10) << 20, (500 + i as u64 * 4) << 20)).collect();
        let f = findings(&samples);
        let metrics: Vec<_> = f.iter().map(|x| (x.name.as_str(), x.metric.as_str())).collect();
        assert!(metrics.contains(&("dwm.exe", "memory")), "{metrics:?}");
        assert!(metrics.contains(&("dwm.exe", "gpu:00000000_00019333")));
        assert!(!metrics.iter().any(|(n, _)| *n == "steady.exe"));
        assert_eq!(label(&samples, "gpu:00000000_00019333"), "GPU memory on AMD Radeon RX 7600M XT");
        let top = gpu_top(&samples, 5);
        assert_eq!(top[0].1[0].3, (47 * 4) << 20, "change over the window");
    }

    #[test]
    fn a_reused_pid_is_a_different_process() {
        let mut samples: Vec<Sample> = (0..48).map(|i| sample(i * 300, 1000 << 20, 0)).collect();
        for (i, s) in samples.iter_mut().enumerate().skip(24) { s.p[0].ct = 2; s.p[0].pws = (2000 + i as u64) << 20; }
        assert!(findings(&samples).iter().all(|f| f.metric != "memory"), "the jump comes from a new process, not growth");
    }

    #[test]
    fn tracks_put_leaks_first_and_add_watched_processes() {
        let samples: Vec<Sample> = (0..48).map(|i| sample(1000 + i * 300, (1000 + i as u64 * 10) << 20, 500 << 20)).collect();
        let t = tracks(&samples, &["STEADY.exe".into()], 0);
        assert_eq!(t.last().map(|x| (x.name.as_str(), x.verdict.is_none())), Some(("steady.exe", true)));
        assert!(t[0].verdict.is_some());
        assert_eq!((t[0].t[0], t[0].t[47]), (1000, 1000 + 47 * 300), "unix seconds of each point");
        assert!(tracks(&samples, &[], 0).iter().all(|x| x.name != "steady.exe"));
        assert!(tracks(&samples, &[], 5).iter().any(|x| x.name == "steady.exe" && x.top), "largest now");
    }

    #[test]
    fn early_signs_after_twenty_minutes() {
        let samples: Vec<Sample> = (0..31).map(|i| sample(i * 60, (1000 + i as u64 * 10) << 20, 0)).collect(); // 1-min samples
        let t = tracks(&samples, &[], 0);
        let m = t.iter().find(|x| x.name == "dwm.exe" && x.metric == "memory").expect("early sign");
        assert!(m.early && m.verdict.is_some());
        assert!(findings(&samples).iter().all(|f| f.metric != "memory"), "no verdict before 2 h");
    }

    #[test]
    fn only_two_sessions_stay_without_the_keep_setting() {
        let d = std::env::temp_dir().join(format!("keel-sessions-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for n in ["session-20261104-080000", "session-20261105-070000", "session-20261105-194550", "20261105"] {
            std::fs::write(d.join(format!("{n}.jsonl")), "").unwrap();
        }
        prune_sessions(&d, Some(2));
        let left: Vec<String> = sessions_in(&d).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["session-20261105-070000.jsonl", "session-20261105-194550.jsonl"]);
        assert!(d.join("20261105.jsonl").exists(), "day files follow the week rule");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn sample_lines_round_trip() {
        let s = sample(1, 2, 3);
        let back: Sample = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.p[0].g[0].d, 3);
        assert!(!serde_json::to_string(&s.p[1]).unwrap().contains("\"g\""));
    }
}

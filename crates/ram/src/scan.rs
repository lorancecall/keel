//! `scan`: top memory users with what launched them and their network endpoints (read-only).
use crate::net::{self, Endpoint};
use crate::paths::{self, normalize_path};
use crate::sources::{self, Launcher};
use crate::family::Family;
use crate::{deny, inventory, procs};
use std::collections::HashMap;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

/// Exes that host other code; matching launchers by their path alone would blame them for
/// every task / Run value that starts them, so they get origins only from the SCM.
pub const GENERIC_HOSTS: &[&str] = &[
    "svchost.exe", "rundll32.exe", "dllhost.exe", "conhost.exe", "cmd.exe", "powershell.exe", "pwsh.exe",
    "wscript.exe", "cscript.exe", "mshta.exe", "msiexec.exe", "taskhostw.exe", "explorer.exe", "wsl.exe",
];

#[derive(Clone, Copy, PartialEq)]
pub enum SortBy { Private, WorkingSet }

#[derive(serde::Serialize)]
pub struct Memory { pub total: u64, pub available: u64, pub commit_used: u64, pub commit_limit: u64, pub load_pct: u32 }

#[derive(serde::Serialize)]
pub struct ProcReport {
    pub pid: u32,
    pub name: String,
    pub path: Option<String>,
    /// query = opened with limited rights, nt = kernel image name only, denied / kernel = no path.
    pub path_source: &'static str,
    pub working_set: u64,
    pub private_working_set: u64,
    pub private_bytes: u64,
    pub cpu_seconds: f64,
    pub threads: u32,
    pub handles: u32,
    pub session: u32,
    pub parent_pid: u32,
    /// None when the parent pid no longer exists (or was reused by a newer process).
    pub parent_name: Option<String>,
    /// First ancestor with a different exe name (e.g. the app that owns a WebView2 helper).
    pub owner: Option<String>,
    pub services: Vec<String>,
    /// Name of the family (vendor / product suite) this process belongs to.
    pub family: Option<String>,
    pub protected: Option<&'static str>,
    pub origins: Vec<Launcher>,
    /// Launchers of `owner`, when this process has none of its own.
    pub owner_origins: Vec<Launcher>,
    pub endpoints: Vec<Endpoint>,
    pub notes: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct ImageTotal { pub name: String, pub count: usize, pub private_bytes: u64, pub working_set: u64 }

#[derive(serde::Serialize)]
pub struct Report {
    pub generated: String,
    pub memory: Option<Memory>,
    pub process_count: usize,
    pub sort: &'static str,
    pub top: Vec<ProcReport>,
    pub by_image: Vec<ImageTotal>,
    pub launchers_scanned: usize,
    /// Everything grouped by vendor / product suite, largest memory first.
    pub families: Vec<Family>,
}

/// Launchers that start this exe: services running in the pid first, then path matches
/// (skipped for generic hosts). `exe_norm` is the normalized exe path, if known.
pub fn match_origins(exe_norm: Option<&str>, running_services: &[String], launchers: &[Launcher]) -> Vec<Launcher> {
    let mut out: Vec<Launcher> = Vec::new();
    for s in running_services {
        if let Some(l) = launchers.iter().find(|l| l.kind == "service" && l.name.eq_ignore_ascii_case(s)) {
            out.push(l.clone());
        }
    }
    let Some(exe) = exe_norm else { return out };
    if GENERIC_HOSTS.contains(&paths::file_name(exe)) { return out; }
    for l in launchers {
        if l.exe.as_deref() == Some(exe) && !out.iter().any(|o| o.kind == l.kind && o.location == l.location && o.name == l.name) {
            out.push(l.clone());
        }
    }
    out
}

/// Ancestors, nearest first, stopping where a pid was reused (parent younger than child).
pub fn ancestors<'a>(p: &'a procs::Proc, pids: &HashMap<u32, &'a procs::Proc>) -> Vec<&'a procs::Proc> {
    let mut v = Vec::new();
    let mut cur = p;
    while v.len() < 16 {
        match pids.get(&cur.ppid) {
            Some(q) if q.pid != cur.pid && q.pid != 0 && q.create_time <= cur.create_time => { v.push(*q); cur = q; }
            _ => break,
        }
    }
    v
}

fn memory() -> Option<Memory> {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { GlobalMemoryStatusEx(&mut m).ok()?; }
    Some(Memory {
        total: m.ullTotalPhys, available: m.ullAvailPhys,
        commit_used: m.ullTotalPageFile - m.ullAvailPageFile, commit_limit: m.ullTotalPageFile, load_pct: m.dwMemoryLoad,
    })
}

/// Sums memory per exe name, largest private bytes first.
pub fn by_image(ps: &[procs::Proc]) -> Vec<ImageTotal> {
    let mut m: HashMap<String, ImageTotal> = HashMap::new();
    for p in ps {
        let e = m.entry(p.name.to_lowercase()).or_insert_with(|| ImageTotal { name: p.name.clone(), count: 0, private_bytes: 0, working_set: 0 });
        e.count += 1;
        e.private_bytes += p.private_bytes;
        e.working_set += p.working_set;
    }
    let mut v: Vec<_> = m.into_values().collect();
    v.sort_by(|a, b| b.private_bytes.cmp(&a.private_bytes));
    v
}

pub fn run(top: usize, sort: SortBy, sizes: bool) -> anyhow::Result<Report> {
    stage("start");
    let mut ps = procs::snapshot()?;
    let all = ps.clone();
    let pids = procs::by_pid(&all);
    match sort {
        SortBy::Private => ps.sort_by(|a, b| b.private_bytes.cmp(&a.private_bytes)),
        SortBy::WorkingSet => ps.sort_by(|a, b| b.working_set.cmp(&a.working_set)),
    }
    let launchers = sources::all();
    stage("launchers");
    let (svc, active) = sources::scm_active();
    let mut eps = net::endpoints();
    let devmap = procs::device_map();
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let paths_by_pid: HashMap<u32, (Option<String>, &'static str)> = all.iter().map(|p| (p.pid, procs::image_path(p.pid, &devmap))).collect();
    let exe_by_pid: HashMap<u32, String> = paths_by_pid.iter()
        .filter_map(|(pid, (p, _))| p.as_ref().filter(|x| !x.starts_with(r"\Device\")).map(|x| (*pid, x.clone()))).collect();
    stage("scm, net, exe paths");
    let families = inventory::families(&all, &exe_by_pid, &launchers, &active, sizes);
    let family_of: HashMap<u32, String> = families.iter()
        .flat_map(|f| f.members.iter().filter_map(|m| m.pid).map(|pid| (pid, f.name.clone()))).collect();

    let mut out = Vec::new();
    for p in ps.iter().take(top) {
        let (path, path_source) = paths_by_pid.get(&p.pid).cloned().unwrap_or((None, "denied"));
        let norm = path.as_deref().filter(|x| !x.starts_with(r"\Device\")).map(|x| normalize_path(x, &sysroot));
        let services = svc.get(&p.pid).cloned().unwrap_or_default();
        let chain = ancestors(p, &pids);
        let parent = chain.first();
        let owner = chain.iter().find(|q| !q.name.eq_ignore_ascii_case(&p.name)).copied();
        let origins = match_origins(norm.as_deref(), &services, &launchers);
        let owner_origins = match owner {
            Some(o) if origins.is_empty() && !GENERIC_HOSTS.contains(&o.name.to_lowercase().as_str()) => {
                let op = exe_by_pid.get(&o.pid).cloned();
                let on = op.as_deref().filter(|x| !x.starts_with(r"\Device\")).map(|x| normalize_path(x, &sysroot));
                match_origins(on.as_deref(), svc.get(&o.pid).map(Vec::as_slice).unwrap_or(&[]), &launchers)
            }
            _ => vec![],
        };
        let mut notes = Vec::new();
        if path_source == "nt" { notes.push("not openable without admin; path from kernel image name".into()); }
        if path_source == "denied" { notes.push("path unavailable without admin".into()); }
        if norm.as_deref().is_some_and(|n| GENERIC_HOSTS.contains(&paths::file_name(n))) && services.is_empty() {
            notes.push("generic host: launcher not resolved by path".into());
        }
        out.push(ProcReport {
            pid: p.pid,
            name: p.name.clone(),
            protected: deny::protected(&p.name, path.as_deref()),
            origins,
            owner_origins,
            owner: owner.map(|o| format!("{} {}", o.pid, o.name)),
            path,
            path_source,
            working_set: p.working_set,
            private_working_set: p.private_ws,
            private_bytes: p.private_bytes,
            cpu_seconds: p.cpu_100ns as f64 / 1e7,
            threads: p.threads,
            handles: p.handles,
            session: p.session,
            parent_pid: p.ppid,
            parent_name: parent.map(|q| q.name.clone()),
            services,
            family: family_of.get(&p.pid).cloned(),
            endpoints: eps.remove(&p.pid).unwrap_or_default(),
            notes,
        });
    }
    Ok(Report {
        generated: chrono::Local::now().to_rfc3339(),
        memory: memory(),
        process_count: all.len(),
        sort: if sort == SortBy::Private { "private_bytes" } else { "working_set" },
        top: out,
        by_image: by_image(&all).into_iter().take(15).collect(),
        launchers_scanned: launchers.len(),
        families,
    })
}

/// Prints the time since the previous stage when KEEL_TIMING is set (stderr).
pub fn stage(name: &str) {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    if std::env::var_os("KEEL_TIMING").is_none() { return; }
    let mut l = LAST.lock().unwrap();
    let now = std::time::Instant::now();
    if let Some(t) = *l { eprintln!("[timing] {name}: {:.2}s", (now - t).as_secs_f64()); }
    *l = Some(now);
}

pub fn mb(b: u64) -> String { format!("{:.0}", b as f64 / 1048576.0) }
fn gb(b: u64) -> String { format!("{:.1}", b as f64 / 1073741824.0) }

/// Short origin label for the table, e.g. `svc:WSearch(auto)`, `run:OneDrive`, `task:\X\Y`.
pub fn origin_label(l: &Launcher) -> String {
    let off = if l.enabled == Some(false) { "-off" } else { "" };
    match l.kind {
        "service" => format!("svc:{}({}){off}", l.name, l.start.as_deref().unwrap_or("?")),
        "task" => format!("task:{}{off}", l.location),
        k => format!("{k}:{}{off}", l.name),
    }
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}~", s.chars().take(n - 1).collect::<String>()) }
}

pub fn print(r: &Report) {
    if let Some(m) = &r.memory {
        println!("RAM {} GB total, {} GB available ({}% used); commit {} / {} GB; {} processes; {} launchers scanned",
            gb(m.total), gb(m.available), m.load_pct, gb(m.commit_used), gb(m.commit_limit), r.process_count, r.launchers_scanned);
    }
    println!("top {} by {}  (MB; PRIV = private bytes, WS = working set; '!' = protected, never quarantined)\n", r.top.len(), r.sort);
    println!("{:>6} {:<26} {:>6} {:>6} {:>8} {:>4}  {:<22} {:>4}  {:<20} ORIGIN", "PID", "NAME", "PRIV", "WS", "CPU(s)", "SES", "OWNER (parent app)", "NET", "FAMILY");
    for p in &r.top {
        let mark = if p.protected.is_some() { "!" } else { " " };
        let parent = p.owner.clone().unwrap_or_else(|| format!("{} {}", p.parent_pid, p.parent_name.as_deref().unwrap_or("(gone)")));
        let mut origin: Vec<String> = p.origins.iter().map(origin_label).collect();
        let extra: Vec<&String> = p.services.iter().filter(|s| !p.origins.iter().any(|o| o.kind == "service" && &o.name == *s)).collect();
        origin.extend(extra.into_iter().map(|s| format!("svc:{s}")));
        if origin.is_empty() && !p.owner_origins.is_empty() {
            origin.push(format!("via owner: {}", p.owner_origins.iter().map(origin_label).collect::<Vec<_>>().join(" ")));
        }
        if origin.is_empty() { origin.push(if p.path.is_none() { "?".into() } else { "-".into() }); }
        let mut olabel = origin.join(" ");
        if let Some(why) = p.protected { olabel = format!("[{why}] {olabel}"); }
        println!("{:>6} {}{:<25} {:>6} {:>6} {:>8.0} {:>4}  {:<22} {:>4}  {:<20} {}",
            p.pid, mark, clip(&p.name, 25), mb(p.private_bytes), mb(p.working_set), p.cpu_seconds, p.session,
            clip(&parent, 22), p.endpoints.len(), clip(p.family.as_deref().unwrap_or("-"), 20), clip(&olabel, 80));
    }
    println!("\nby exe name (all processes):");
    println!("{:<30} {:>5} {:>7} {:>7}", "NAME", "COUNT", "PRIV", "WS");
    for t in &r.by_image {
        println!("{:<30} {:>5} {:>7} {:>7}", clip(&t.name, 30), t.count, mb(t.private_bytes), mb(t.working_set));
    }
    print_families(&r.families, 20);
}

/// Families worth a look: running, starting by themselves, or using >= 100 MB of disk.
pub fn notable(f: &Family) -> bool { f.processes > 0 || f.auto_start > 0 || f.disk_bytes >= 100 << 20 }

fn count(f: &Family, k: &str) -> usize { f.counts.get(k).copied().unwrap_or(0) }

pub fn print_families(fams: &[Family], n: usize) {
    println!("
families (vendor / product suite; '!' = protected; AUTO = starts by itself; '>' = size walk cut short; `keel ram family <name>` for members):");
    println!("{:<32} {:>5} {:>7} {:>7} {:>7} {:>5} {:>4} {:>4} {:>4} {:>4} {:>5}",
        "FAMILY", "PROCS", "PRIV", "WS", "DISK GB", "AUTO", "SVC", "DRV", "RUN", "TASK", "PROD");
    for f in fams.iter().filter(|f| notable(f)).take(n) {
        let mark = if f.protected.is_some() { "!" } else { " " };
        println!("{mark}{:<31} {:>5} {:>7} {:>7} {:>7} {:>5} {:>4} {:>4} {:>4} {:>4} {:>5}",
            clip(&f.name, 31), f.processes, mb(f.private_bytes), mb(f.working_set),
            format!("{}{}", if f.disk_partial { ">" } else { "" }, gb(f.disk_bytes)), f.auto_start,
            count(f, "service"), count(f, "driver"), count(f, "run") + count(f, "runonce") + count(f, "startup"), count(f, "task"), count(f, "product"));
    }
}

/// Every member of the families whose name or key contains `query` (case-insensitive).
pub fn print_family(fams: &[Family], query: &str) -> bool {
    let q = query.to_lowercase();
    let hits: Vec<&Family> = fams.iter().filter(|f| f.name.to_lowercase().contains(&q) || f.key.contains(&q)).collect();
    for f in &hits {
        println!("{}{}  ({} processes, {} MB private, {} GB disk, {} auto-start)",
            f.name, f.protected.map(|p| format!(" [protected: {p}]")).unwrap_or_default(), f.processes, mb(f.private_bytes), gb(f.disk_bytes), f.auto_start);
        if !f.uninstallers.is_empty() {
            println!("  uninstallers (vendor package manager first):");
            for u in f.uninstallers.iter().take(5) { println!("    {}", clip(u, 160)); }
            if f.uninstallers.len() > 5 { println!("    ... {} more", f.uninstallers.len() - 5); }
        }
        let mut last = "";
        let mut shown = 0;
        for m in &f.members {
            if m.kind != last {
                let plural = if m.kind.ends_with('s') { format!("{}es", m.kind) } else { format!("{}s", m.kind) };
                println!("  {plural} ({}):", count(f, m.kind));
                last = m.kind;
                shown = 0;
            }
            shown += 1;
            if m.kind == "product" && shown > 15 { if shown == 16 { println!("    ... (all in --json)"); } continue; }
            let mut tags = Vec::new();
            if let Some(pid) = m.pid { tags.push(format!("pid {pid}")); }
            if let Some(s) = &m.start { tags.push(s.clone()); }
            if m.enabled == Some(false) && m.start.as_deref() != Some("disabled") { tags.push("disabled".into()); }
            if m.running && m.kind != "process" { tags.push("running".into()); }
            if m.hidden { tags.push("hidden".into()); }
            if m.bytes > 0 { tags.push(format!("{}{} MB", if m.size_partial { ">" } else { "" }, mb(m.bytes))); }
            println!("    {:<40} {:<28} {}", clip(&m.name, 40), tags.join(", "), clip(m.path.as_deref().unwrap_or(""), 110));
        }
        println!();
    }
    !hits.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(kind: &'static str, name: &str, exe: &str) -> Launcher {
        Launcher { kind, location: format!("loc\\{name}"), name: name.into(), command: exe.into(), enabled: Some(true),
            start: None, disable: sources::disable_hint(kind), dll: None, exe: Some(exe.into()) }
    }

    #[test]
    fn matches_by_path_and_running_service() {
        let ls = vec![
            l("run", "Foo", r"c:\apps\foo.exe"),
            l("task", "FooUpdate", r"c:\apps\foo.exe"),
            l("service", "FooSvc", r"c:\apps\foosvc.exe"),
            l("service", "Netman", r"c:\windows\system32\svchost.exe"),
            l("task", "Rundll", r"c:\windows\system32\svchost.exe"),
        ];
        let o = match_origins(Some(r"c:\apps\foo.exe"), &[], &ls);
        assert_eq!(o.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["Foo", "FooUpdate"]);
        let o = match_origins(Some(r"c:\windows\system32\svchost.exe"), &["netman".into()], &ls);
        assert_eq!(o.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["Netman"]);
        let o = match_origins(Some(r"c:\apps\foosvc.exe"), &["FooSvc".into()], &ls);
        assert_eq!(o.len(), 1, "running service and its ImagePath match are the same launcher");
        assert!(match_origins(None, &[], &ls).is_empty());
    }

    fn pr(pid: u32, ppid: u32, name: &str, t: i64) -> procs::Proc {
        procs::Proc { pid, ppid, name: name.into(), working_set: 0, private_ws: 0, private_bytes: 0, cpu_100ns: 0,
            create_time: t, threads: 0, handles: 0, session: 1 }
    }

    #[test]
    fn walks_ancestors_until_pid_reuse() {
        let all = vec![pr(1, 0, "explorer.exe", 5), pr(2, 1, "app.exe", 10), pr(3, 2, "webview.exe", 20), pr(4, 3, "webview.exe", 30), pr(9, 50, "x.exe", 1), pr(50, 1, "new.exe", 40)];
        let m = procs::by_pid(&all);
        let names = |p: &procs::Proc| ancestors(p, &m).iter().map(|q| q.pid).collect::<Vec<_>>();
        assert_eq!(names(&all[3]), [3, 2, 1]);
        assert_eq!(names(&all[4]), Vec::<u32>::new(), "pid 50 is younger than its 'child': reused pid");
    }

    #[test]
    fn labels_origins() {
        let mut s = l("service", "WSearch", "x");
        s.start = Some("auto".into());
        assert_eq!(origin_label(&s), "svc:WSearch(auto)");
        let mut r = l("run", "OneDrive", "x");
        r.enabled = Some(false);
        assert_eq!(origin_label(&r), "run:OneDrive-off");
        assert_eq!(clip("abcdef", 4), "abc~");
    }
}

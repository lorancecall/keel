//! Collects every family member candidate (processes, services, drivers, Run/Startup items, tasks,
//! installed products, vendor folders) and hands them to `family::group`. Read-only.
use crate::family::{self, Family, Item, Member};
use crate::paths::{self, command_exe, normalize_path};
use crate::procs::Proc;
use crate::scan::GENERIC_HOSTS;
use crate::sources::{self, Launcher, Product};
use crate::fileinfo;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Program roots whose first-level folders are vendor / product folders (raw and normalized).
pub fn roots() -> Vec<(PathBuf, String)> {
    let sysroot = std::env::var("SystemRoot").unwrap_or_default();
    ["ProgramFiles", "ProgramFiles(x86)", "ProgramData", "LOCALAPPDATA", "APPDATA"].iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|p| { let n = normalize_path(&p, &sysroot); (PathBuf::from(p), n.trim_end_matches('\\').to_string()) })
        .collect()
}

/// First-level vendor folders under the roots (also inside `Common Files` / `Programs`).
fn vendor_dirs(roots: &[(PathBuf, String)]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = roots.iter().map(|r| r.0.clone()).collect();
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() || ft.is_symlink() { continue; }
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name == "common files" || name == "programs" { stack.push(e.path()); } else { out.push(e.path()); }
        }
    }
    out
}

fn is_generic(norm: &str) -> bool { GENERIC_HOSTS.contains(&paths::file_name(norm)) }

fn launcher_item(l: &Launcher, active: &HashSet<String>) -> Item {
    let mut m = Member::new(l.kind, l.name.clone());
    m.path = Some(match l.kind { "task" | "service" | "driver" => l.command.clone(), _ => format!("{} = {}", l.location, l.command) });
    m.start = l.start.clone();
    m.location = Some(l.location.clone());
    m.enabled = l.enabled;
    m.running = matches!(l.kind, "service" | "driver") && active.contains(&l.name.to_lowercase());
    let mut paths: Vec<String> = Vec::new();
    match (&l.exe, &l.dll) {
        (_, Some(dll)) => paths.push(dll.clone()),
        (Some(exe), None) if !is_generic(exe) => paths.push(exe.clone()),
        _ => {}
    }
    Item { member: m, paths, publisher: None }
}

/// Directory paths get a dummy child so `folder_root` sees them as "inside" the folder.
fn dir_probe(dir: &str, sysroot: &str) -> Option<String> {
    let d = dir.trim().trim_matches('"').trim_end_matches('\\');
    (!d.is_empty()).then(|| format!(r"{}\_", normalize_path(d, sysroot)))
}

fn product_item(p: &Product, sysroot: &str) -> Item {
    let mut m = Member::new("product", p.name.clone());
    m.hidden = p.system_component;
    m.bytes = p.estimated_kb as u64 * 1024;
    m.path = Some(p.key.clone());
    let un = if p.quiet_uninstall.is_empty() { &p.uninstall } else { &p.quiet_uninstall };
    if !un.is_empty() { m.uninstall = Some(un.clone()); }
    let mut paths = Vec::new();
    if let Some(d) = dir_probe(&p.install_location, sysroot) { paths.push(d); }
    for c in [&p.uninstall, &p.display_icon] {
        if let Some(e) = command_exe(c, &sources::env_lookup) { if !is_generic(&e) { paths.push(e); } }
    }
    Item { member: m, paths, publisher: (!p.publisher.is_empty()).then(|| p.publisher.clone()) }
}

/// Fills in folder sizes, only where it matters: families that are not protected and own more
/// than folders (walking every AppData cache would take minutes). Leftover-heavy families (most
/// auto-start items) go first; 8 walkers in parallel, 8 s per folder, 25 s overall. A cut walk is
/// reported as a lower bound (`size_partial`).
fn measure_folders(fams: &mut [Family]) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    const PER_FOLDER: Duration = Duration::from_secs(8);
    let overall = Instant::now() + Duration::from_secs(25);
    let mut order: Vec<usize> = (0..fams.len())
        .filter(|&i| fams[i].protected.is_none() && fams[i].members.iter().any(|m| m.kind != "folder")).collect();
    order.sort_by_key(|&i| std::cmp::Reverse((fams[i].auto_start, fams[i].processes)));
    let jobs: Vec<(usize, usize, String)> = order.iter()
        .flat_map(|&fi| fams[fi].members.iter().enumerate()
            .filter(|(_, m)| m.kind == "folder")
            .filter_map(move |(mi, m)| m.path.clone().map(|p| (fi, mi, p))))
        .collect();
    let next = AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<(usize, usize, u64, bool)>> = std::sync::Mutex::new(Vec::new());
    let timing = std::env::var_os("KEEL_TIMING").is_some();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((fi, mi, p)) = jobs.get(i) else { break };
                let t = Instant::now();
                let (bytes, complete) = if t >= overall { (0, false) } else { fileinfo::dir_size(Path::new(p), (t + PER_FOLDER).min(overall)) };
                if timing && t.elapsed().as_secs_f64() > 1.0 {
                    eprintln!("[timing]   {p}: {:.1}s, {} MB{}", t.elapsed().as_secs_f64(), bytes >> 20, if complete { "" } else { " (cut)" });
                }
                results.lock().unwrap().push((*fi, *mi, bytes, complete));
            });
        }
    });
    for (fi, mi, bytes, complete) in results.into_inner().unwrap() {
        let f = &mut fams[fi];
        f.members[mi].bytes = bytes;
        f.members[mi].size_partial = !complete;
        f.disk_partial |= !complete;
        f.disk_bytes += bytes;
    }
    fams.sort_by(|a, b| b.memory.cmp(&a.memory).then(b.private_bytes.cmp(&a.private_bytes)).then(b.disk_bytes.cmp(&a.disk_bytes)));
}

/// Builds families from a process snapshot (with exe paths by pid), launchers and products.
/// `sizes` walks the vendor folders for their disk size (a few seconds on a full disk).
pub fn families(procs: &[Proc], exe_by_pid: &HashMap<u32, String>, launchers: &[Launcher], active: &HashSet<String>, sizes: bool) -> Vec<Family> {
    let sysroot = std::env::var("SystemRoot").unwrap_or_default();
    let roots = roots();
    let norm_roots: Vec<String> = roots.iter().map(|r| r.1.clone()).collect();
    let mut items = Vec::new();
    for p in procs {
        let Some(exe) = exe_by_pid.get(&p.pid) else { continue };
        let mut m = Member::new("process", p.name.clone());
        m.pid = Some(p.pid);
        m.path = Some(exe.clone());
        m.running = true;
        m.bytes = p.private_bytes;
        items.push(Item { member: m, paths: vec![normalize_path(exe, &sysroot)], publisher: None });
    }
    items.extend(launchers.iter().map(|l| launcher_item(l, active)));
    crate::scan::stage("inventory: processes + launchers");
    items.extend(sources::products().iter().map(|p| product_item(p, &sysroot)));
    for d in vendor_dirs(&roots) {
        let mut m = Member::new("folder", d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        m.path = Some(d.display().to_string());
        let probe = dir_probe(&d.display().to_string(), &sysroot).unwrap_or_default();
        items.push(Item { member: m, paths: vec![probe], publisher: None });
    }
    crate::scan::stage("inventory: products + folders");
    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    let mut company = |p: &str| -> Option<String> {
        cache.entry(p.to_string()).or_insert_with(|| if Path::new(p).exists() { fileinfo::company_name(p) } else { None }).clone()
    };
    let ws: HashMap<u32, (u64, u64)> = procs.iter().map(|p| (p.pid, (p.working_set, p.private_ws))).collect();
    let fams = family::group(items, &norm_roots, &mut company, &|pid| ws.get(&pid).copied().unwrap_or((0, 0)));
    crate::scan::stage("inventory: grouping (CompanyName lookups)");
    let mut fams = fams;
    if sizes { measure_folders(&mut fams); }
    crate::scan::stage("inventory: folder sizes");
    fams
}

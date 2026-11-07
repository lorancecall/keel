//! Families: everything one vendor / product suite put on the machine, bundled so it can be judged
//! (and later disabled or removed) as one unit. Items are linked by vendor keys that come from
//! Uninstall `Publisher`, the exe / driver `CompanyName`, and the install-folder root name; keys
//! that co-occur (a Publisher whose uninstaller lives in folder X, a file in folder X signed with
//! CompanyName Y) are merged with union-find. Names alone never link anything (NisSrv.exe is
//! Microsoft Defender, not NI).
use std::collections::{BTreeMap, HashMap};

/// Folder names under a program root that hold many vendors (no family of their own).
const SHARED_DIRS: &[&str] = &[
    "package cache", "microsoft", "microsoft shared", "packages", "temp", "tmp", "microsoft.net", "installshield installation information",
    "uninstall information", "reference assemblies", "msbuild", "modifiablewindowsapps", "dotnet", "common",
    "usoshared", "usoprivate", "softwaredistribution", "crashdumps", "d3dscache", "connecteddevicesplatform",
    "comms", "regid.1991-06.com.microsoft", "ssh", "nuget", "desktop.ini", "internet explorer", "windowsapps",
    "microsoft_corporation", "cache", "caches", "logs",
];

/// Folder levels that are containers, not vendors: look one level deeper.
const DESCEND: &[&str] = &["common files", "programs"];

/// Legal suffixes dropped from vendor names.
const SUFFIXES: &[&str] = &[
    "inc", "incorporated", "corp", "corporation", "co", "company", "ltd", "limited", "llc", "gmbh", "ag", "sa",
    "srl", "bv", "plc", "the", "kk", "pte", "pty",
];

/// Different spellings of one vendor (after normalization).
const ALIASES: &[(&str, &str)] = &[
    ("advanced micro devices", "amd"), ("ati technologies", "amd"), ("ati", "amd"),
    ("nvidia", "nvidia"), ("logi", "logitech"),
];

/// Vendors whose legal entities differ by region or division ("Lenovo PC HK", "Lenovo Group"):
/// any key that starts with one of these words is that vendor.
const VENDOR_PREFIXES: &[(&str, &str)] = &[
    ("lenovo", "lenovo"), ("nvidia", "nvidia"), ("advanced micro devices", "amd"), ("ati technologies", "amd"),
    ("intel", "intel"), ("realtek", "realtek"), ("google", "google"), ("logitech", "logitech"),
    ("autodesk", "autodesk"), ("adobe", "adobe"), ("national instruments", "national instruments"),
];

/// Comparable vendor key: lowercase words, punctuation and legal suffixes dropped, aliases applied;
/// `windows ...` folders are Microsoft. `microsoft <product>` folders (OneDrive, Office, Edge) stay
/// their own family so they can be judged apart from Windows itself.
pub fn vendor_key(raw: &str) -> String {
    let cleaned: String = raw.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' }).collect();
    let words: Vec<&str> = cleaned.split_whitespace().filter(|w| !SUFFIXES.contains(w)).collect();
    let k = words.join(" ");
    if let Some((_, to)) = ALIASES.iter().find(|(from, _)| *from == k) { return to.to_string(); }
    if let Some((_, to)) = VENDOR_PREFIXES.iter().find(|(p, _)| k == *p || k.strip_prefix(p).is_some_and(|r| r.starts_with(' '))) {
        return to.to_string();
    }
    if k == "windows" || k.starts_with("windows ") { return "microsoft".into(); }
    k
}

/// The vendor folder a normalized path lives in (`c:\program files\national instruments`) and its
/// key, when it is under one of `roots` (normalized, no trailing `\`). WindowsApps packages use the
/// package name before the first `_`; Steam games their own folder.
pub fn folder_root(norm: &str, roots: &[String]) -> Option<(String, String)> {
    let root = roots.iter().filter(|r| norm.len() > r.len() + 1 && norm.starts_with(r.as_str()) && norm.as_bytes()[r.len()] == b'\\')
        .max_by_key(|r| r.len())?;
    let comps: Vec<&str> = norm[root.len() + 1..].split('\\').collect();
    let mut i = 0;
    while i + 1 < comps.len() && DESCEND.contains(&comps[i]) { i += 1; }
    if comps[i] == "windowsapps" && i + 2 < comps.len() {
        let pkg = comps[i + 1];
        let key = vendor_key(pkg.split('_').next().unwrap_or(pkg));
        return (!key.is_empty()).then(|| (format!(r"{root}\{}\{}", comps[..=i].join("\\"), pkg), key));
    }
    if comps[i] == "steam" && comps.get(i + 1) == Some(&"steamapps") && comps.get(i + 2) == Some(&"common") && i + 4 < comps.len() {
        let game = comps[i + 3];
        return Some((format!(r"{root}\{}", comps[..=i + 3].join("\\")), vendor_key(game)));
    }
    if i + 1 >= comps.len() { return None; } // a file directly in the root
    let name = comps[i];
    if SHARED_DIRS.contains(&name) || name.starts_with('{') || name.starts_with('.') { return None; }
    let key = vendor_key(name);
    if key.is_empty() { return None; }
    Some((format!(r"{root}\{}", comps[..=i].join("\\")), key))
}

/// The original-case spelling of a vendor folder's name inside a raw path (WindowsApps packages
/// without their `_version_arch__id` tail).
pub fn raw_label(raw: &str, dir_norm: &str) -> Option<String> {
    let last = dir_norm.rsplit('\\').next()?;
    let seg = raw.split(['\\', '/']).find(|s| s.to_lowercase() == last)?;
    Some(seg.split('_').next().unwrap_or(seg).to_string())
}

struct Uf(HashMap<String, String>);

impl Uf {
    fn find(&mut self, k: &str) -> String {
        let p = self.0.get(k).cloned().unwrap_or_else(|| k.to_string());
        if p == k { return p; }
        let r = self.find(&p);
        self.0.insert(k.to_string(), r.clone());
        r
    }
    fn union(&mut self, a: &str, b: &str) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.0.insert(hi, lo);
        }
    }
}

/// One thing a family owns. `bytes` = private bytes for a process, disk size for a folder,
/// estimated size for a product.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Member {
    pub kind: &'static str,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub running: bool,
    pub bytes: u64,
    /// Product hidden from Apps & Features (`SystemComponent=1`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uninstall: Option<String>,
    /// Folder size walk was cut short: `bytes` is a lower bound.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub size_partial: bool,
    /// Where the launcher lives: Run key, task path (`\Vendor\Task`), service key, shortcut file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl Member {
    pub fn new(kind: &'static str, name: impl Into<String>) -> Self {
        Member { kind, name: name.into(), path: None, pid: None, start: None, enabled: None, running: false, bytes: 0, hidden: false, uninstall: None, size_partial: false, location: None }
    }
    /// Starts by itself: auto/boot/system services and drivers, enabled Run/Startup items and tasks.
    pub fn auto_start(&self) -> bool {
        match self.kind {
            "service" | "driver" => matches!(self.start.as_deref(), Some("auto" | "boot" | "system")),
            "run" | "startup" | "task" => self.enabled != Some(false),
            _ => false,
        }
    }
}

/// Input to grouping: a member, the normalized paths that locate it, and a vendor name it declares.
pub struct Item {
    pub member: Member,
    pub paths: Vec<String>,
    pub publisher: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Family {
    pub key: String,
    pub name: String,
    pub protected: Option<&'static str>,
    pub processes: usize,
    pub private_bytes: u64,
    pub working_set: u64,
    /// Private working set of its processes: what Task Manager's Memory column adds up to.
    pub memory: u64,
    pub disk_bytes: u64,
    /// Some folder walks were cut short: `disk_bytes` is a lower bound.
    pub disk_partial: bool,
    pub auto_start: usize,
    pub counts: BTreeMap<&'static str, usize>,
    /// Visible uninstallers, vendor package managers first (cleanup prefers them).
    pub uninstallers: Vec<String>,
    pub members: Vec<Member>,
}

/// Vendor families that are never disabled or removed as a unit.
pub fn protected_vendor(key: &str) -> Option<&'static str> {
    match key {
        "microsoft" => Some("windows / microsoft"),
        "lenovo" | "legion" | "legionspace" => Some("lenovo"),
        "amd" => Some("amd"),
        "nvidia" => Some("nvidia"),
        "keel" => Some("keel"),
        _ => None,
    }
}

/// Is this uninstall command a vendor package manager (preferred over per-package uninstallers)?
pub fn is_package_manager(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    ["nipkg", "nipackagemanager", "package manager", "packagemanager"].iter().any(|k| c.contains(k))
}

/// Groups items into families. `company` returns a file's CompanyName (cached by the caller);
/// `ws` gives a process's working set by pid for the totals.
/// `ws(pid)`: working set and private working set of a process.
pub fn group(items: Vec<Item>, roots: &[String], company: &mut dyn FnMut(&str) -> Option<String>, ws: &dyn Fn(u32) -> (u64, u64)) -> Vec<Family> {
    let mut uf = Uf(HashMap::new());
    // Display-name votes per key: publishers weigh 3, CompanyNames 1; folder names only as fallback.
    let mut labels: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    let mut item_keys: Vec<Option<String>> = Vec::with_capacity(items.len());
    for it in &items {
        let mut key = None;
        let pubk = it.publisher.as_deref().map(vendor_key).filter(|k| !k.is_empty());
        if let (Some(k), Some(p)) = (&pubk, &it.publisher) { labels.entry(k.clone()).or_default().push((p.clone(), 3)); key = Some(k.clone()); }
        if it.member.kind == "folder" {
            if let Some((_, rk)) = it.paths.first().and_then(|p| folder_root(p, roots)) {
                labels.entry(rk).or_default().push((it.member.name.clone(), 0));
            }
        } else if it.member.kind == "process" {
            // Original-case folder name (paths are lowercased), e.g. "Microsoft.WindowsNotepad".
            if let (Some(raw), Some((dir, rk))) = (&it.member.path, it.paths.first().and_then(|p| folder_root(p, roots))) {
                if let Some(l) = raw_label(raw, &dir) { labels.entry(rk).or_default().push((l, 0)); }
            }
        }
        for p in &it.paths {
            let root = folder_root(p, roots);
            let comp = if it.member.kind == "folder" || it.member.kind == "product" { None } else { company(p) };
            let ck = comp.as_deref().map(vendor_key).filter(|k| !k.is_empty());
            if let (Some(k), Some(c)) = (&ck, &comp) { labels.entry(k.clone()).or_default().push((c.clone(), 1)); }
            if let Some((_, rk)) = &root {
                labels.entry(rk.clone()).or_default();
                if let Some(k) = &pubk { if k != "microsoft" && k != rk { uf.union(k, rk); } }
                if let Some(c) = &ck { if c != "microsoft" && c != rk { uf.union(c, rk); } }
            }
            if key.is_none() { key = root.map(|r| r.1).or(ck); }
        }
        item_keys.push(key);
    }
    let mut fams: HashMap<String, Family> = HashMap::new();
    for (it, key) in items.into_iter().zip(item_keys) {
        let Some(k) = key else { continue };
        let root = uf.find(&k);
        let f = fams.entry(root.clone()).or_insert_with(|| Family {
            key: root.clone(), name: String::new(), protected: None, processes: 0, private_bytes: 0, working_set: 0, memory: 0,
            disk_bytes: 0, disk_partial: false, auto_start: 0, counts: BTreeMap::new(), uninstallers: vec![], members: vec![],
        });
        let m = it.member;
        *f.counts.entry(m.kind).or_default() += 1;
        if m.auto_start() { f.auto_start += 1; }
        match m.kind {
            "process" => { f.processes += 1; f.private_bytes += m.bytes; if let Some(pid) = m.pid { let (w, p) = ws(pid); f.working_set += w; f.memory += p; } }
            "folder" => f.disk_bytes += m.bytes,
            "product" if !m.hidden => if let Some(u) = &m.uninstall { f.uninstallers.push(format!("{}: {u}", m.name)); },
            _ => {}
        }
        f.members.push(m);
    }
    // Name: the most frequent label among all keys merged into the family.
    let all_keys: Vec<String> = labels.keys().cloned().collect();
    let mut votes: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for k in all_keys {
        let r = uf.find(&k);
        let v = votes.entry(r).or_default();
        for (l, w) in &labels[&k] {
            let e = v.entry(l.clone()).or_default();
            *e += w * 1000 + 1; // weight first, then plain count (folder names only break ties)
        }
    }
    let mut out: Vec<Family> = fams.into_values().map(|mut f| {
        f.name = votes.get(&f.key).and_then(|v| v.iter().max_by(|a, b| a.1.cmp(b.1).then(a.0.len().cmp(&b.0.len()))).map(|(n, _)| n.clone()))
            .unwrap_or_else(|| f.key.clone());
        f.protected = protected_vendor(&f.key).or_else(|| labels.keys().filter(|k| uf.find(k) == f.key).find_map(|k| protected_vendor(k)));
        f.uninstallers.sort_by_key(|u| !is_package_manager(u));
        f.members.sort_by(|a, b| a.kind.cmp(b.kind).then(b.bytes.cmp(&a.bytes)).then(a.name.cmp(&b.name)));
        f
    }).collect();
    out.sort_by(|a, b| b.memory.cmp(&a.memory).then(b.private_bytes.cmp(&a.private_bytes)).then(b.disk_bytes.cmp(&a.disk_bytes)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<String> {
        vec![r"c:\program files".into(), r"c:\program files (x86)".into(), r"c:\programdata".into(), r"c:\users\me\appdata\local".into()]
    }

    #[test]
    fn normalizes_vendor_names() {
        assert_eq!(vendor_key("National Instruments Corporation"), "national instruments");
        assert_eq!(vendor_key("NVIDIA Corporation"), "nvidia");
        assert_eq!(vendor_key("Advanced Micro Devices, Inc."), "amd");
        assert_eq!(vendor_key("Microsoft Corporation"), "microsoft");
        assert_eq!(vendor_key("Windows Defender"), "microsoft");
        assert_eq!(vendor_key("Autodesk, Inc."), "autodesk");
        assert_eq!(vendor_key("KOEI TECMO GAMES CO., LTD."), "koei tecmo games");
        assert_eq!(vendor_key("Lenovo PC HK Limited"), "lenovo");
        assert_eq!(vendor_key("Lenovo Group Ltd."), "lenovo");
        assert_eq!(vendor_key("Lenovoish Apps"), "lenovoish apps");
        assert_eq!(vendor_key("Microsoft OneDrive"), "microsoft onedrive");
    }

    #[test]
    fn finds_vendor_folder_roots() {
        let r = roots();
        // Office, ink / TabTip and many vendors' plug-ins share this folder: it is nobody's family.
        assert_eq!(folder_root(r"c:\program files\common files\microsoft shared\clicktorun\officeclicktorun.exe", &r), None);
        assert_eq!(folder_root(r"c:\program files\common files\microsoft shared\ink\tabtip.exe", &r), None);
        assert_eq!(folder_root(r"c:\program files\national instruments\shared\x.exe", &r).unwrap(),
            (r"c:\program files\national instruments".into(), "national instruments".into()));
        assert_eq!(folder_root(r"c:\program files (x86)\common files\national instruments\a\b.dll", &r).unwrap().1, "national instruments");
        assert_eq!(folder_root(r"c:\users\me\appdata\local\programs\foo app\foo.exe", &r).unwrap().1, "foo app");
        assert_eq!(folder_root(r"c:\program files\windowsapps\claude_4.1.0.0_x64__abc\app\claude.exe", &r).unwrap().1, "claude");
        assert_eq!(folder_root(r"c:\program files (x86)\steam\steamapps\common\hades\x.exe", &r).unwrap().1, "hades");
        assert_eq!(folder_root(r"c:\programdata\microsoft\windows defender\platform\4.18\nissrv.exe", &r), None);
        assert_eq!(folder_root(r"c:\programdata\package cache\{guid}\setup.exe", &r), None);
        assert_eq!(folder_root(r"c:\windows\system32\drivers\nipalk.sys", &r), None);
        assert_eq!(folder_root(r"c:\program files\x.exe", &r), None);
        assert_eq!(raw_label(r"C:\Program Files\WindowsApps\Microsoft.WindowsNotepad_11.2_x64__8w\Notepad.exe",
            r"c:\program files\windowsapps\microsoft.windowsnotepad_11.2_x64__8w").as_deref(), Some("Microsoft.WindowsNotepad"));
    }

    fn item(kind: &'static str, name: &str, path: &str, publisher: Option<&str>) -> Item {
        let mut m = Member::new(kind, name);
        m.path = Some(path.into());
        Item { member: m, paths: if path.is_empty() { vec![] } else { vec![path.into()] }, publisher: publisher.map(Into::into) }
    }

    /// The National Instruments leftovers on this machine, in miniature.
    #[test]
    fn groups_national_instruments_without_name_prefix_matching() {
        let mut svc = item("service", "mxssvr", r"c:\program files (x86)\national instruments\shared\mxs\mxssvr.exe", None);
        svc.member.start = Some("auto".into());
        let mut drv = item("driver", "NIPALK", r"c:\windows\system32\drivers\nipalk.sys", None);
        drv.member.start = Some("boot".into());
        let mut pkg = item("product", "NI Package Manager", r"c:\program files\national instruments\ni package manager\nipackagemanager.exe", Some("NI"));
        pkg.member.uninstall = Some(r#""C:\Program Files\National Instruments\NI Package Manager\NIPackageManager.exe" remove ni-package-manager"#.into());
        let mut hidden = item("product", "NI-DAQmx ADE Support", "", Some("National Instruments"));
        hidden.member.hidden = true;
        let mut folder = item("folder", "National Instruments", r"c:\programdata\national instruments\x", None);
        folder.member.bytes = 11 << 30;
        let items = vec![
            svc, drv, pkg, hidden, folder,
            item("process", "NisSrv.exe", r"c:\programdata\microsoft\windows defender\platform\4.18\nissrv.exe", None),
            item("process", "chrome.exe", r"c:\program files\google\chrome\application\chrome.exe", None),
        ];
        let mut company = |p: &str| -> Option<String> {
            if p.contains("nipalk") || p.contains("mxssvr") { Some("National Instruments Corporation".into()) }
            else if p.contains("nissrv") { Some("Microsoft Corporation".into()) }
            else if p.contains("chrome") { Some("Google LLC".into()) }
            else { None }
        };
        let fams = group(items, &roots(), &mut company, &|_| (0, 0));
        let ni = fams.iter().find(|f| f.name == "National Instruments").expect("one NI family");
        let kinds: Vec<_> = ni.members.iter().map(|m| (m.kind, m.name.as_str())).collect();
        assert_eq!(kinds.len(), 5, "{kinds:?}");
        assert!(!kinds.iter().any(|(_, n)| *n == "NisSrv.exe"));
        assert_eq!(ni.auto_start, 2);
        assert_eq!(ni.disk_bytes, 11 << 30);
        assert!(ni.uninstallers[0].contains("NIPackageManager.exe"));
        let ms = fams.iter().find(|f| f.key == "microsoft").unwrap();
        assert_eq!(ms.protected, Some("windows / microsoft"));
        assert!(fams.iter().any(|f| f.key == "google" && f.members.len() == 1));
    }
}

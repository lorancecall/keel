//! `cleanup <family>`: writes a disable script and its undo script for one family. Keel itself
//! changes nothing: the user reviews and runs the scripts in an admin PowerShell.
//!
//! Order: the vendor's own uninstaller first (listed, never run here), then
//! *disable* leftovers: services -> Disabled (+ stop), scheduled tasks -> disabled, Run / Startup
//! items -> StartupApproved off. Every touched registry key and task is backed up before the first
//! change, and the script stops if any backup fails. Drivers are never touched.
use crate::family::{Family, Member};

/// One reversible change.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Action {
    /// Set start type Disabled; stop it if running. Undo: `reg import` of the service key.
    DisableService { name: String, start: String, running: bool },
    /// `schtasks /change /disable`. Undo: `/enable`.
    DisableTask { path: String },
    /// StartupApproved value -> 03 (off). Undo: the backed-up bytes (or no value).
    StartupOff { approved_key: String, value: String },
}

#[derive(Debug, serde::Serialize)]
pub struct Plan {
    pub family: String,
    pub uninstallers: Vec<String>,
    pub actions: Vec<Action>,
    /// Members left alone, with the reason (drivers, already off, processes without a service, ...).
    pub untouched: Vec<String>,
}

/// StartupApproved key for a Run key / Startup folder (Explorer's "Startup apps" switch).
pub fn approved_key(m: &Member) -> Option<String> {
    let loc = m.location.as_deref().filter(|l| !l.is_empty())?;
    let base = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
    match m.kind {
        "run" => {
            let hive = loc.split('\\').next()?;
            let sub = if loc.to_lowercase().contains(r"\wow6432node\") { "Run32" } else { "Run" };
            Some(format!(r"{hive}\{base}\{sub}"))
        }
        "startup" => {
            let pd = std::env::var("ProgramData").unwrap_or_default().to_lowercase();
            let common = !pd.is_empty() && loc.to_lowercase().starts_with(&pd);
            Some(format!(r"{}\{base}\StartupFolder", if common { "HKLM" } else { "HKCU" }))
        }
        _ => None,
    }
}

/// What disabling this family would do. Protected families get no actions.
pub fn plan(f: &Family) -> Plan {
    let mut actions = Vec::new();
    let mut untouched = Vec::new();
    if let Some(why) = f.protected {
        untouched.push(format!("whole family: protected ({why})"));
        return Plan { family: f.name.clone(), uninstallers: vec![], actions, untouched };
    }
    for m in &f.members {
        match m.kind {
            "service" => match m.start.as_deref() {
                Some("disabled") => untouched.push(format!("service {}: already disabled", m.name)),
                Some("boot" | "system") => untouched.push(format!("service {}: boot/system start, left alone", m.name)),
                Some(s) => actions.push(Action::DisableService { name: m.name.clone(), start: s.to_string(), running: m.running }),
                None => untouched.push(format!("service {}: unknown start type", m.name)),
            },
            "task" if m.enabled != Some(false) => match &m.location {
                Some(p) => actions.push(Action::DisableTask { path: p.clone() }),
                None => untouched.push(format!("task {}: path unknown", m.name)),
            },
            "task" => untouched.push(format!("task {}: already disabled", m.name)),
            "run" | "startup" if m.enabled != Some(false) => match approved_key(m) {
                Some(k) => actions.push(Action::StartupOff { approved_key: k, value: m.name.clone() }),
                None => untouched.push(format!("{} {}: location unknown", m.kind, m.name)),
            },
            "run" | "startup" => untouched.push(format!("{} {}: already off", m.kind, m.name)),
            "runonce" => untouched.push(format!("runonce {}: runs once, nothing to keep off", m.name)),
            "driver" => untouched.push(format!("driver {}: never touched (vendor uninstaller only)", m.name)),
            _ => {}
        }
    }
    let svc_running = f.members.iter().filter(|m| m.kind == "service" && m.running).count();
    let procs = f.members.iter().filter(|m| m.kind == "process").count();
    if procs > 0 {
        untouched.push(format!("{procs} running processes: those of the {svc_running} running services stop with them, others are not killed"));
    }
    Plan { family: f.name.clone(), uninstallers: f.uninstallers.clone(), actions, untouched }
}

/// A single-quoted PowerShell string literal.
pub fn ps(s: &str) -> String { format!("'{}'", s.replace('\'', "''")) }

/// File-name-safe form of a name.
pub fn safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
}

/// Registry provider path (`HKCU:\...`) for a `HKCU\...` / `HKLM\...` key.
fn ps_reg(key: &str) -> String { key.replacen('\\', ":\\", 1) }

fn task_file(i: usize, path: &str) -> String { format!("task-{i:02}-{}.xml", safe(path.rsplit('\\').next().unwrap_or(path))) }

const HEADER: &str = r#"#Requires -RunAsAdministrator
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$backup = Join-Path $here 'backup'
Start-Transcript -Append -Path (Join-Path $here 'log.txt') | Out-Null
$failed = @()
# Native exe: judged by exit code only (reg.exe reports success on stderr, which Windows
# PowerShell 5.1 would turn into a terminating error under 'Stop').
function Native([string]$exe, [string[]]$argv) {
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    try { $out = & $exe @argv 2>&1 } finally { $ErrorActionPreference = $old }
    if ($LASTEXITCODE) { throw "$exe exited $LASTEXITCODE : $out" }
    $out | Where-Object { $_ -is [string] }
}
function Try-Step($what, [scriptblock]$do) {
    try { & $do; Write-Host "ok    $what" } catch { Write-Host "FAIL  $what : $_" -ForegroundColor Red; $script:failed += $what }
}
"#;

const FOOTER: &str = r#"
if ($failed.Count) { Write-Host "$($failed.Count) step(s) failed; see log.txt" -ForegroundColor Yellow } else { Write-Host 'done' }
Stop-Transcript | Out-Null
"#;

/// The disable script: back up everything first (abort on any failure), then change.
pub fn disable_script(p: &Plan, generated: &str) -> String {
    let mut s = format!("# Keel cleanup: disable '{}' (generated {generated})\n# Disables only, never deletes. Undo with undo.ps1 in this folder.\n", p.family);
    if !p.uninstallers.is_empty() {
        s += "# Prefer the vendor's own uninstaller first (run it yourself):\n";
        for u in &p.uninstallers { s += &format!("#   {u}\n"); }
    }
    s += HEADER;
    s += "\n# 1. Backups (any failure stops the script before anything changes)\nNew-Item -ItemType Directory -Force $backup | Out-Null\n";
    for (i, a) in p.actions.iter().enumerate() {
        match a {
            Action::DisableService { name, .. } => s += &format!(
                "Native reg.exe @('export', {}, (Join-Path $backup {}), '/y') | Out-Null\n",
                ps(&format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{name}")), ps(&format!("service-{}.reg", safe(name)))),
            Action::DisableTask { path } => s += &format!(
                "Native schtasks.exe @('/query', '/tn', {}, '/xml') | Out-File -Encoding unicode (Join-Path $backup {})\n",
                ps(path), ps(&task_file(i, path))),
            Action::StartupOff { approved_key, value } => s += &format!(
                "$v = (Get-ItemProperty -Path {} -Name {} -ErrorAction SilentlyContinue).{}; Set-Content -Path (Join-Path $backup {}) -Value $(if ($v) {{ ($v | ForEach-Object {{ $_.ToString() }}) -join ',' }} else {{ 'absent' }})\n",
                ps(&ps_reg(approved_key)), ps(value), ps(value), ps(&format!("startup-{i:02}-{}.txt", safe(value)))),
        }
    }
    s += "\n# 2. Changes\n";
    for a in &p.actions {
        match a {
            Action::DisableService { name, running, .. } => {
                s += &format!("Try-Step {} {{ Set-Service -Name {} -StartupType Disabled }}\n", ps(&format!("service {name} -> Disabled")), ps(name));
                if *running { s += &format!("Try-Step {} {{ Stop-Service -Name {} -Force }}\n", ps(&format!("stop service {name}")), ps(name)); }
            }
            Action::DisableTask { path } => s += &format!(
                "Try-Step {} {{ Native schtasks.exe @('/change', '/tn', {}, '/disable') | Out-Null }}\n",
                ps(&format!("task {path} -> disabled")), ps(path)),
            Action::StartupOff { approved_key, value } => s += &format!(
                "Try-Step {} {{ New-Item -Force -Path {} -ErrorAction SilentlyContinue | Out-Null; New-ItemProperty -Force -Path {} -Name {} -PropertyType Binary -Value ([byte[]](3,0,0,0,0,0,0,0,0,0,0,0)) | Out-Null }}\n",
                ps(&format!("startup item {value} -> off")), ps(&ps_reg(approved_key)), ps(&ps_reg(approved_key)), ps(value)),
        }
    }
    s + FOOTER
}

/// The undo script: restore service keys from the backup, re-enable tasks and startup items,
/// restart services that were running.
pub fn undo_script(p: &Plan, generated: &str) -> String {
    let mut s = format!("# Keel cleanup: undo for '{}' (generated {generated})\n# Restores what disable.ps1 changed, from ./backup.\n", p.family);
    s += HEADER;
    s += "\n";
    for (i, a) in p.actions.iter().enumerate() {
        match a {
            Action::DisableService { name, .. } => s += &format!(
                "Try-Step {} {{ Native reg.exe @('import', (Join-Path $backup {})) | Out-Null }}\n",
                ps(&format!("service {name}: restore start type")), ps(&format!("service-{}.reg", safe(name)))),
            Action::DisableTask { path } => s += &format!(
                "Try-Step {} {{ Native schtasks.exe @('/change', '/tn', {}, '/enable') | Out-Null }}\n",
                ps(&format!("task {path} -> enabled")), ps(path)),
            Action::StartupOff { approved_key, value } => s += &format!(
                "Try-Step {} {{ $old = Get-Content (Join-Path $backup {}); if ($old -eq 'absent') {{ Remove-ItemProperty -Path {} -Name {} }} else {{ New-ItemProperty -Force -Path {} -Name {} -PropertyType Binary -Value ([byte[]]($old -split ',')) | Out-Null }} }}\n",
                ps(&format!("startup item {value} -> restored")), ps(&format!("startup-{i:02}-{}.txt", safe(value))),
                ps(&ps_reg(approved_key)), ps(value), ps(&ps_reg(approved_key)), ps(value)),
        }
    }
    for a in &p.actions {
        if let Action::DisableService { name, running: true, .. } = a {
            s += &format!("Try-Step {} {{ Start-Service -Name {} }}\n", ps(&format!("start service {name}")), ps(name));
        }
    }
    s + FOOTER
}

/// Human summary for the terminal.
pub fn summary(p: &Plan) -> String {
    let mut s = format!("cleanup plan for {}: {} changes\n", p.family, p.actions.len());
    if !p.uninstallers.is_empty() {
        s += "  vendor uninstallers (preferred; run them yourself first):\n";
        for u in p.uninstallers.iter().take(3) { s += &format!("    {u}\n"); }
    }
    for a in &p.actions {
        s += &match a {
            Action::DisableService { name, start, running } => format!("  service {name}: {start} -> disabled{}\n", if *running { " (and stop)" } else { "" }),
            Action::DisableTask { path } => format!("  task {path}: -> disabled\n"),
            Action::StartupOff { value, .. } => format!("  startup item {value}: -> off\n"),
        };
    }
    for u in &p.untouched { s += &format!("  left alone: {u}\n"); }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn m(kind: &'static str, name: &str, start: Option<&str>, enabled: Option<bool>, loc: Option<&str>) -> Member {
        let mut x = Member::new(kind, name);
        x.start = start.map(Into::into);
        x.enabled = enabled;
        x.location = loc.map(Into::into);
        x
    }

    fn fam(members: Vec<Member>, protected: Option<&'static str>) -> Family {
        Family { key: "ni".into(), name: "National Instruments".into(), protected, processes: 0, private_bytes: 0, working_set: 0, memory: 0,
            disk_bytes: 0, disk_partial: false, auto_start: 0, counts: BTreeMap::new(), uninstallers: vec!["NI Package Manager: x".into()], members }
    }

    #[test]
    fn plans_disable_only_and_never_drivers() {
        let mut running = m("service", "niauth", Some("auto"), Some(true), None);
        running.running = true;
        let f = fam(vec![
            running,
            m("service", "NIApplicationWebServer64", Some("disabled"), Some(false), None),
            m("driver", "NIPALK", Some("boot"), Some(true), None),
            m("driver", "nipxirmk", Some("auto"), Some(true), None),
            m("task", "NIUpdateServiceCheckTask", None, Some(true), Some(r"\NIUpdateServiceCheckTask")),
            m("run", "NI Device Monitor", None, Some(true), Some(r"HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run")),
            m("run", "NIRegistrationWizard", None, Some(false), Some(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run")),
        ], None);
        let p = plan(&f);
        assert_eq!(p.actions, vec![
            Action::DisableService { name: "niauth".into(), start: "auto".into(), running: true },
            Action::DisableTask { path: r"\NIUpdateServiceCheckTask".into() },
            Action::StartupOff { approved_key: r"HKLM\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32".into(), value: "NI Device Monitor".into() },
        ]);
        assert_eq!(p.untouched.iter().filter(|u| u.starts_with("driver")).count(), 2);
        assert!(!disable_script(&p, "t").to_lowercase().contains("nipalk"));
    }

    #[test]
    fn protected_family_gets_no_actions() {
        let f = fam(vec![m("service", "WinDefend", Some("auto"), Some(true), None)], Some("windows / microsoft"));
        let p = plan(&f);
        assert!(p.actions.is_empty());
        assert!(p.untouched[0].contains("protected"));
    }

    #[test]
    fn scripts_back_up_before_changing_and_quote_safely() {
        assert_eq!(ps("it's"), "'it''s'");
        assert_eq!(safe(r"\A B\c"), "_A_B_c");
        let p = Plan { family: "X".into(), uninstallers: vec![], untouched: vec![], actions: vec![
            Action::DisableService { name: "svc'1".into(), start: "auto".into(), running: true },
            Action::DisableTask { path: r"\Vendor\Up date".into() },
        ] };
        let d = disable_script(&p, "t");
        let first_change = d.find("# 2. Changes").unwrap();
        assert!(d.find("Native reg.exe @('export'").unwrap() < first_change);
        assert!(d.find("Native schtasks.exe @('/query'").unwrap() < first_change);
        assert!(d.contains(r"'HKLM\SYSTEM\CurrentControlSet\Services\svc''1'"));
        assert!(d.contains(r"'/tn', '\Vendor\Up date', '/disable'"));
        assert!(d.starts_with("# Keel") && d.contains("#Requires -RunAsAdministrator"));
        let u = undo_script(&p, "t");
        assert!(u.contains("Native reg.exe @('import'") && u.contains("'/enable'") && u.contains("Start-Service -Name 'svc''1'"));
        assert!(!u.contains("Remove-Item ") && !d.contains("Remove-Item "));
    }
}

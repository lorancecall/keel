//! keel: the Keel command line. Each feature is a subcommand group (`keel ram ...`,
//! `keel egpu ...`, `keel net ...`), plus the service (`keel service`, run by the SCM) and its controls.
mod egpu;
mod net;

use anyhow::{bail, Result};
use keel_ram::{cleanup, daemon, events, family, observe, ondemand, pipe, scan, trayicons, userside};

const HELP: &str = "\
keel - Keel: memory, eGPU and network guard for one Legion Go

USAGE
  keel ram <command>      memory hogs, app families, leaks, on-demand families (`keel ram help`)
  keel egpu <command>     eGPU limits, watchdog, telemetry, drivers, display scaling (`keel egpu help`)
  keel net <command>      Wi-Fi beside the LAN, internet side, VPN pinning, kill switches (`keel net help`)
  keel status             the service's features: running, waiting, restarts
  keel feature <ram|egpu|net> on|off
                          switch a feature of the service (saved in %ProgramData%\\keel\\service.toml)
  keel service-install  * register and start the Keel service (from C:\\Program Files\\Keel only;
                          the installer does this)
  keel service-uninstall *  stop the service, put back everything it changed, remove it
  keel service-restart  *
  keel daemon           * the service's work in this console until Ctrl+C (test with KEEL_DATA=<dir>)
  keel version
  keel help

  * needs an administrator prompt. The tray is keel-tray.exe, the main window keel-gui.exe.
";

const RAM_HELP: &str = "\
keel ram - memory hogs, their launchers and their network endpoints

USAGE (nothing here needs an administrator prompt; cleanup only writes scripts)
  keel ram scan [--top N] [--sort private|ws] [--json <path>] [--no-size]
        top N processes (default 25) by private bytes (default) or working set, with
        parent, session, hosted services, launchers (Run/RunOnce, services, scheduled tasks,
        Startup folder) and current TCP/UDP endpoints; then families: everything one vendor /
        product suite installed (processes, services, drivers, Run items, tasks, products,
        folders) with memory and disk totals. --json writes the full report;
        --no-size skips measuring folder sizes.
  keel ram family <name> [--no-size]
        every member of the families whose name contains <name>
  keel ram sample                     record one sample now (memory, handles, GPU memory per adapter)
  keel ram watch                      record a sample every 5 minutes and display / power / TDR
                                         events (with a dwm GPU snapshot after each) until stopped
  keel ram leaks [--hours N]          processes whose memory / handles / GPU memory keep growing
                                         over the last N hours (default 6), with their family;
                                         shows who holds GPU memory next to dwm.exe
        samples: %LOCALAPPDATA%\\keel\\samples\\YYYYMMDD.jsonl, kept 7 days
  keel ram tray-icons [--prune]       entries per app in Settings > Taskbar > Other system tray
                                         icons; --prune keeps one per app that has 10+ (backup first)
  keel ram cleanup <name> [--out <dir>]
        writes disable.ps1 + undo.ps1 (+ plan.json) for one family; changes nothing itself.
        disable.ps1 backs up first, then disables services / tasks / startup items (never deletes,
        never touches drivers). Run it yourself in an admin PowerShell. Default folder:
        %LOCALAPPDATA%\\keel\\cleanup\\<family>-<time>

ON-DEMAND FAMILIES (need the Keel service)
  keel ram ondemand                   families under on-demand control and whether they are on
  keel ram ondemand <family> off      keep its services Disabled, tasks and startup items off
  keel ram ondemand <family> follow --app <exe> [--app <exe>]
                                       off, but on while one of these apps runs (and 20 s after)
  keel ram ondemand <family> start | stop   switch on / off now
  keel ram ondemand <family> normal   put everything back as it was and stop managing it
        Originals are saved before any change (%ProgramData%\\keel\\ondemand.json). Drivers
        and protected families are never switched. Something that turns a managed service or task
        back on is switched off again within 5 minutes (logged in service.log).
  keel ram daemon                     the service's on-demand loop in the foreground (admin
                                         console; test with KEEL_DATA=<scratch dir>)
  keel ram help
";

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn cmd_scan(args: &[String]) -> Result<()> {
    let top = match opt(args, "--top") { Some(n) => n.parse()?, None => 25 };
    let sort = match opt(args, "--sort").as_deref() {
        None | Some("private") => scan::SortBy::Private,
        Some("ws") => scan::SortBy::WorkingSet,
        Some(x) => bail!("unknown --sort {x} (private | ws)"),
    };
    let report = scan::run(top, sort, !args.iter().any(|a| a == "--no-size"))?;
    scan::print(&report);
    if let Some(path) = opt(args, "--json") {
        std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
        println!("\nfull report: {path}");
    }
    Ok(())
}

/// Writes a text file as UTF-8 with BOM and CRLF (what Windows PowerShell 5.1 reads correctly).
fn write_ps(path: &std::path::Path, text: &str) -> Result<()> {
    let body = text.replace("\r\n", "\n").replace('\n', "\r\n");
    std::fs::write(path, [b"\xEF\xBB\xBF".as_slice(), body.as_bytes()].concat())?;
    Ok(())
}

/// Samples every 5 minutes and records display / power / driver events until stopped; one
/// instance per user session (it needs the session to receive display broadcasts).
fn cmd_watch() -> Result<()> {
    let Some(_one) = keel_core::single::acquire("keel-watch") else { bail!("keel ram watch is already running") };
    println!("sampling every {} min into {}, events into {} (Ctrl+C to stop)",
        observe::INTERVAL_SECS / 60, observe::dir().display(), events::dir().display());
    events::watch()
}

/// The one family whose name (or key) is `q`, or the only one containing it.
fn pick_family(q: &str) -> Result<family::Family> {
    let report = scan::run(0, scan::SortBy::Private, false)?;
    let ql = q.to_lowercase();
    let exact: Vec<_> = report.families.iter().filter(|f| f.name.to_lowercase() == ql || f.key == ql).collect();
    let hits: Vec<_> = if exact.is_empty() { report.families.iter().filter(|f| f.name.to_lowercase().contains(&ql)).collect() } else { exact };
    match hits.as_slice() {
        [f] => Ok(family::Family { members: f.members.clone(), counts: f.counts.clone(), uninstallers: f.uninstallers.clone(), name: f.name.clone(), key: f.key.clone(), ..**f }),
        [] => bail!("no family matches {q}"),
        many => bail!("{q} matches several families: {}", many.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

fn print_ondemand(st: &ondemand::State) {
    if st.entries.is_empty() { println!("no on-demand families"); return; }
    println!("{:<28} {:<7} {:<4} {:>4} {:>5} {:>8}  APPS", "FAMILY", "MODE", "NOW", "SVC", "TASKS", "STARTUP");
    for e in &st.entries {
        println!("{:<28} {:<7} {:<4} {:>4} {:>5} {:>8}  {}", scan::clip(&e.family, 28), format!("{:?}", e.mode).to_lowercase(),
            if e.on { "on" } else { "off" }, e.services.len(), e.tasks.iter().filter(|t| t.enabled).count(), e.startup.len(), e.apps.join(", "));
    }
}

fn cmd_ondemand(args: &[String]) -> Result<()> {
    let Some(q) = args.first().filter(|a| !a.starts_with("--")) else {
        match pipe::call(&ondemand::Request::Status) {
            Ok(r) => print_ondemand(&r.state),
            Err(e) => { println!("{e:#}\nlast saved state:"); print_ondemand(&daemon::load()); }
        }
        return Ok(());
    };
    let action = args.get(1).map(String::as_str).unwrap_or("");
    let f = pick_family(q)?;
    let parts = ondemand::parts(&f).map_err(anyhow::Error::msg)?;
    let apps: Vec<String> = args.windows(2).filter(|w| w[0] == "--app").map(|w| w[1].clone()).collect();
    let req = match action {
        "off" => ondemand::Request::Set { family: f.name.clone(), key: f.key.clone(), mode: Some(ondemand::Mode::Off), apps },
        "follow" => {
            if apps.is_empty() {
                bail!("follow needs the app that switches {} on: --app <exe> (repeatable). Its startup items start: {}",
                    f.name, if parts.app_hints.is_empty() { "-".into() } else { parts.app_hints.join(", ") });
            }
            ondemand::Request::Set { family: f.name.clone(), key: f.key.clone(), mode: Some(ondemand::Mode::Follow), apps }
        }
        "normal" => ondemand::Request::Set { family: f.name.clone(), key: f.key.clone(), mode: None, apps },
        "start" | "stop" => ondemand::Request::Turn { key: f.key.clone(), on: action == "start" },
        _ => bail!("usage: keel ram ondemand <family> off | follow --app <exe> | normal | start | stop"),
    };
    let r = pipe::call(&req)?;
    println!("{}", r.message);
    if r.ok || matches!(req, ondemand::Request::Set { mode: None, .. }) {
        match action {
            "off" | "follow" if !parts.hkcu.is_empty() => { userside::user_off(&f.key, &parts.hkcu)?; println!("your startup items: {} off", parts.hkcu.len()); }
            "normal" => userside::user_restore(&f.key)?,
            _ => {}
        }
    }
    if matches!(action, "off" | "follow" | "stop") {
        println!("services stopped; leftover processes in the family's folders are closed (see {})", daemon::dir().join("service.log").display());
    }
    print_ondemand(&r.state);
    Ok(())
}

fn cmd_cleanup(args: &[String]) -> Result<()> {
    let Some(q) = args.first().filter(|a| !a.starts_with("--")) else { bail!("usage: keel ram cleanup <family name> [--out <dir>]") };
    let f = &pick_family(q)?;
    let plan = cleanup::plan(f);
    print!("{}", cleanup::summary(&plan));
    if plan.actions.is_empty() { println!("nothing to do; no scripts written"); return Ok(()); }
    let now = chrono::Local::now();
    let dir = match opt(args, "--out") {
        Some(d) => std::path::PathBuf::from(d),
        None => keel_core::paths::user_data_dir().join("cleanup")
            .join(format!("{}-{}", cleanup::safe(&f.name), now.format("%Y%m%d-%H%M%S"))),
    };
    std::fs::create_dir_all(&dir)?;
    let stamp = now.to_rfc3339();
    write_ps(&dir.join("disable.ps1"), &cleanup::disable_script(&plan, &stamp))?;
    write_ps(&dir.join("undo.ps1"), &cleanup::undo_script(&plan, &stamp))?;
    std::fs::write(dir.join("plan.json"), serde_json::to_string_pretty(&plan)?)?;
    println!("\nwritten to {}\nreview disable.ps1, then run it in an admin PowerShell; undo.ps1 reverts it.", dir.display());
    Ok(())
}

fn ram(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("scan") => cmd_scan(&args[1..]),
        Some("family") => {
            let Some(q) = args.get(1).filter(|a| !a.starts_with("--")) else { bail!("usage: keel ram family <name>") };
            let report = scan::run(0, scan::SortBy::Private, !args.iter().any(|a| a == "--no-size"))?;
            if !scan::print_family(&report.families, q) { bail!("no family matches {q}"); }
            Ok(())
        }
        Some("cleanup") => cmd_cleanup(&args[1..]),
        Some("sample") => {
            let s = observe::take()?;
            // Into the running tray's session, not a new one (which would push an older one out).
            observe::join_latest_session();
            let path = observe::store(&s, keel_core::traycfg::Prefs::load().keep_memory_history)?;
            println!("{} processes recorded -> {}", s.p.len(), path.display());
            Ok(())
        }
        Some("watch") => cmd_watch(),
        Some("leaks") => {
            let hours: f64 = match opt(args, "--hours") { Some(h) => h.parse()?, None => 6.0 };
            let samples = observe::load(hours);
            // Families only for live pids that appear in the report (costs a few seconds).
            let family_of = if samples.is_empty() { Default::default() } else {
                scan::run(0, scan::SortBy::Private, false)?.families.iter()
                    .flat_map(|f| f.members.iter().filter_map(|m| m.pid).map(|pid| (pid, f.name.clone()))).collect()
            };
            observe::print_report(&samples, &events::load(hours), hours, &family_of);
            Ok(())
        }
        Some("ondemand") => cmd_ondemand(&args[1..]),
        Some("tray-icons") => {
            if args.iter().any(|a| a == "--prune") {
                match trayicons::prune()? {
                    (0, _) => println!("no app has {}+ tray-settings entries; nothing to do", trayicons::MIN_DUPLICATES),
                    (n, f) => println!("removed {n} duplicate entries; backup: {}", f.map(|p| p.display().to_string()).unwrap_or_default()),
                }
            }
            for (exe, n) in trayicons::counts().into_iter().take(10) { println!("{n:>5}  {exe}"); }
            Ok(())
        }
        // Foreground on-demand loop for testing in an admin console (Ctrl+C stops it; state stays).
        Some("daemon") => { keel_core::paths::ensure_may_write_data_dir()?; daemon::run(|_| {}, true) }
        None | Some("help" | "-h" | "--help") => { print!("{RAM_HELP}"); Ok(()) }
        Some(x) => bail!("unknown command ram {x}; see `keel ram help`"),
    }
}

fn print_service(r: &keel_service::Reply) {
    println!("Keel service {} (pid {}): {}", r.version, r.pid, r.message);
    for f in &r.features {
        let state = match (f.enabled, f.running) { (false, _) => "off", (true, true) => "running", (true, false) => "restarting" };
        println!("  {:<5} {state}{}", f.name, if f.restarts > 0 {
            format!("  ({} restart(s); last: {})", f.restarts, f.last_error.as_deref().unwrap_or("-"))
        } else { String::new() });
    }
}

/// While the Keel service exists but its pipe is not up yet (just installed or restarted, which
/// took more than 8 s here), waits up to 30 s instead of failing.
fn service_status() -> Result<()> {
    let start = std::time::Instant::now();
    let mut said = false;
    loop {
        match keel_service::call(&keel_service::Request::Status) {
            Ok(r) => { print_service(&r); return Ok(()); }
            Err(_) if keel_service::svc::exists(keel_service::svc::NAME) && start.elapsed() < std::time::Duration::from_secs(30) => {
                if !said { eprintln!("waiting for the Keel service to start ..."); said = true; }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(e) => return Err(e),
        }
    }
}

fn set_feature(args: &[String]) -> Result<()> {
    let (Some(name), Some(on)) = (args.first(), args.get(1).and_then(|a| match a.as_str() { "on" => Some(true), "off" => Some(false), _ => None })) else {
        bail!("usage: keel feature ram|egpu|net on|off");
    };
    let r = keel_service::call(&keel_service::Request::SetFeature { name: name.clone(), on })?;
    print_service(&r);
    if !r.ok { bail!("{}", r.message); }
    Ok(())
}

/// The whole service in this console until Ctrl+C (admin console, or KEEL_DATA=<scratch dir>).
fn daemon_foreground() -> Result<()> {
    keel_core::paths::ensure_may_write_data_dir()?;
    anyhow::ensure!(!keel_service::svc::exists(keel_service::svc::NAME) || std::env::var_os(keel_core::paths::DATA_ENV).is_some(),
        "the Keel service is installed; run this only with KEEL_DATA=<scratch dir> beside it");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = stop.clone();
    egpu::on_ctrl_c(move || s.store(true, std::sync::atomic::Ordering::SeqCst));
    keel_service::run(&stop)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("ram") => ram(&args[1..]),
        Some("egpu") => egpu::main(&args[1..]),
        Some("net") => net::main(&args[1..]),
        Some("status") => service_status(),
        Some("feature") => set_feature(&args[1..]),
        Some("service") => keel_service::svc::dispatch(),
        Some("daemon") => daemon_foreground(),
        Some("service-install") => keel_service::svc::install(),
        Some("service-uninstall") => keel_service::svc::uninstall(),
        Some("service-restart") => keel_service::svc::restart(),
        Some("version" | "--version" | "-V") => { println!("keel {}", env!("CARGO_PKG_VERSION")); Ok(()) }
        None | Some("help" | "-h" | "--help") => { print!("{HELP}"); Ok(()) }
        Some(x) => bail!("unknown command {x}; see `keel help`"),
    }
}

//! `keel egpu ...`: the eGPU feature's CLI. `run` runs the guard loop in the console, for testing
//! without the service.
use anyhow::{Context, Result, bail};
use keel_egpu::{amd, apps, config, display, driver, gpu, guard, legion, telemetry, util};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const HELP: &str = "\
keel egpu - eGPU limits, watchdog, telemetry, drivers and display scaling

USAGE (commands marked * need an administrator prompt)
  keel egpu status                      adapters and the service's eGPU status
  keel egpu telemetry [seconds]         read the eGPU's sensors here (ADLX / NVML / PDH), default 5 s
  keel egpu amd-features                AMD eGPU 3D / display features: supported, on/off (read-only)
  keel egpu amd-scaling status | on | off
                                          AMD eGPU displays: GPU Scaling, Scaling Mode, Integer Scaling.
                                          on = record the current values, GPU Scaling on, then Integer
                                          Scaling on if the driver supports it; off = restore the record
  keel egpu nv-scaling [DISPLAYn <0-8>] NVIDIA display config: each display's scaling (8 = integer);
                                          with a display and a value, set it (the tray's integer
                                          scaling mode does this for you)
  keel egpu stress [seconds]            full compute load on the eGPU (default 60 s; the main window's
                                          stress test runs this and measures)
  keel egpu display                     active displays, their GPU, position, scale and modes
  keel egpu display-scale DISPLAYn <%>  Windows scale of an external display (not the built-in screen)
  keel egpu fit-windows                 full-screen windows left at another mode's size -> the current
                                          size, on external displays
  keel egpu run                       * the guard loop in this console (Ctrl+C stops it)
  keel egpu driver-status               display adapters and driver versions
  keel egpu driver-update <path>      * install a driver only on the AMD eGPU
                                          <path> = vendor installer .exe, folder, or .inf
        [--hardware-id <ID>] [--no-block] [--force]
                                          default ID from config; keeps Windows Update blocked;
                                          --force = roll back to an older version / ignore known issue
  keel egpu driver-find <path> [--hardware-id <ID>]  which INF driver-update would use
  keel egpu driver-block   [--hardware-id <ID>]   * stop Windows Update replacing that driver
  keel egpu driver-unblock [--hardware-id <ID>]   * allow it again
  keel egpu prefer-gpu <game.exe> [--off]   make Windows run that exe on the eGPU
  keel egpu amd-tune [--json]           AMD eGPU: power limit, max clock, Radeon features now
  keel egpu amd-tune set [power <%>] [max-mhz <n>]   set them (and remember for the next attach)
  keel egpu amd-tune reset              back to the driver's factory values (and forget)
  keel egpu amd-tune apply              set the remembered values again (the tray does this on attach)
  keel egpu amd-feature <name> on|off   Anti-Lag, Chill, Boost, Image Sharpening, Enhanced Sync,
                                          Radeon Super Resolution, Fluid Motion Frames
  keel egpu apps                        tuning apps running now + Adrenalin state (ok / stuck)
  keel egpu legion-fix status           Legion Space folders and whether they use the iGPU's ADL
  keel egpu legion-fix on | off       * point Legion Space's AMD features at the iGPU / undo
  keel egpu config                      config, log and status paths
  keel egpu help
";

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn hardware_id(args: &[String], cfg: &config::Config) -> Result<String> {
    if let Some(id) = opt(args, "--hardware-id") { return Ok(id); }
    cfg.gpus.iter().find(|g| g.vendor_id == gpu::VENDOR_AMD && !g.hardware_id.is_empty())
        .map(|g| g.hardware_id.clone())
        .ok_or_else(|| anyhow::anyhow!("no AMD eGPU hardware_id in config; pass --hardware-id"))
}

fn print_status() -> Result<()> {
    println!("adapters now:");
    for a in gpu::adapters()? {
        println!("  {:<40} {:04X}:{:04X}  {} MB", a.name, a.vendor_id, a.device_id, a.vram_mb);
    }
    match std::fs::read_to_string(util::status_path()) {
        Ok(s) => println!("\nKeel eGPU status ({}):\n{s}", util::status_path().display()),
        Err(_) => println!("\nKeel eGPU status: none yet (is the Keel service running?)"),
    }
    Ok(())
}

/// The attached eGPU from the config's profiles, with its profile.
fn attached_egpu(cfg: &config::Config) -> Result<(gpu::Adapter, config::GpuProfile)> {
    gpu::adapters()?.into_iter()
        .find_map(|a| cfg.profile_for(a.vendor_id, a.device_id).cloned().map(|p| (a, p)))
        .ok_or_else(|| anyhow::anyhow!("no eGPU from the config's [[gpus]] is attached"))
}

fn print_telemetry(secs: u64) -> Result<()> {
    let cfg = config::Config::load();
    let (a, p) = attached_egpu(&cfg)?;
    let mut b: Box<dyn telemetry::GpuBackend> = if a.vendor_id == gpu::VENDOR_NVIDIA {
        // Read-only here: NvGuard would apply limits, so NVIDIA goes through PDH in the CLI.
        Box::new(keel_egpu::pdh::PdhMonitor::open(&a)?)
    } else {
        telemetry::open(&a, &p).ok_or_else(|| anyhow::anyhow!("no telemetry backend for {}", a.name))?
    };
    for _ in 0..secs.max(1) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        match b.tick() {
            Some(t) => println!("[{}] {}  {}", t.source, t.name, t.summary()),
            None => println!("no sample"),
        }
    }
    Ok(())
}

/// The attached AMD eGPU's profile.
fn amd_egpu() -> Result<config::GpuProfile> {
    let (a, p) = attached_egpu(&config::Config::load())?;
    if a.vendor_id != gpu::VENDOR_AMD { bail!("{} is not an AMD GPU", a.name); }
    Ok(p)
}

/// What the user set per AMD eGPU (`%LOCALAPPDATA%\keelmd-tuning.json`, key "VVVV:DDDD"), set
/// again on each attach: drivers can drop manual tuning after a reset or an update.
#[derive(serde::Serialize, serde::Deserialize, Default, Clone, Debug)]
struct AmdSaved { power: Option<i32>, max_mhz: Option<i32>, #[serde(default)] features: std::collections::BTreeMap<String, bool> }

fn amd_saved_path() -> PathBuf { util::user_dir().join("amd-tuning.json") }
fn amd_key(p: &config::GpuProfile) -> String { format!("{:04X}:{:04X}", p.vendor_id, p.device_id) }

fn amd_saved_all() -> std::collections::BTreeMap<String, AmdSaved> {
    std::fs::read_to_string(amd_saved_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn amd_save(p: &config::GpuProfile, f: impl FnOnce(&mut AmdSaved)) -> Result<()> {
    let mut all = amd_saved_all();
    let e = all.entry(amd_key(p)).or_default();
    f(e);
    if e.power.is_none() && e.max_mhz.is_none() && e.features.is_empty() { all.remove(&amd_key(p)); }
    let path = amd_saved_path();
    if let Some(d) = path.parent() { std::fs::create_dir_all(d)?; }
    std::fs::write(path, serde_json::to_string_pretty(&all)?)?;
    Ok(())
}

fn amd_log(lines: &[String]) { for l in lines { println!("{l}"); keel_core::log::user(format!("egpu: amd-tune: {l}")); } }

fn amd_tune(args: &[String]) -> Result<()> {
    let p = amd_egpu()?;
    let num = |key: &str| args.windows(2).find(|w| w[0] == key).map(|w| w[1].trim_end_matches('%').parse::<i32>()).transpose();
    match args.first().map(String::as_str) {
        None | Some("--json") => {
            let t = keel_egpu::amdtune::read(&p)?;
            if args.iter().any(|a| a == "--json") { println!("{}", serde_json::to_string(&t)?); return Ok(()); }
            println!("{}{}", t.gpu, if t.at_factory == Some(true) { " (factory settings)" } else { "" });
            match t.power { Some(r) => println!("power limit  {:+} % (range {:+}..{:+})", r.now, r.min, r.max), None => println!("power limit  not adjustable") }
            match t.max_mhz { Some(r) => println!("max clock    {} MHz (range {}..{})", r.now, r.min, r.max), None => println!("max clock    not adjustable") }
            for f in &t.features { println!("{:<24} {}", f.name, if f.on { "on" } else { "off" }); }
            Ok(())
        }
        Some("set") => {
            let (power, mhz) = (num("power")?, num("max-mhz")?);
            if power.is_none() && mhz.is_none() { bail!("usage: keel egpu amd-tune set [power <%>] [max-mhz <n>]"); }
            let done = keel_egpu::amdtune::set(&p, power, mhz)?;
            amd_save(&p, |s| { if power.is_some() { s.power = power; } if mhz.is_some() { s.max_mhz = mhz; } })?;
            amd_log(&done);
            Ok(())
        }
        Some("reset") => {
            keel_egpu::amdtune::reset(&p)?;
            amd_save(&p, |s| { s.power = None; s.max_mhz = None; })?;
            amd_log(&["tuning back to factory values".to_string()]);
            Ok(())
        }
        Some("apply") => {
            let Some(s) = amd_saved_all().get(&amd_key(&p)).cloned() else { return Ok(()) };
            let mut done = Vec::new();
            if s.power.is_some() || s.max_mhz.is_some() { done.extend(keel_egpu::amdtune::set(&p, s.power, s.max_mhz)?); }
            for (name, on) in &s.features {
                match keel_egpu::amdtune::set_feature(&p, name, *on) {
                    Ok(()) => done.push(format!("{name} {}", if *on { "on" } else { "off" })),
                    Err(e) => done.push(format!("{name}: {e:#}")),
                }
            }
            amd_log(&done);
            Ok(())
        }
        Some(other) => bail!("unknown amd-tune command {other:?}"),
    }
}

fn amd_feature(args: &[String]) -> Result<()> {
    let (Some(name), Some(state)) = (args.first(), args.get(1)) else { bail!("usage: keel egpu amd-feature <name> on|off") };
    let on = match state.as_str() { "on" => true, "off" => false, _ => bail!("on or off, not {state:?}") };
    let p = amd_egpu()?;
    keel_egpu::amdtune::set_feature(&p, name, on)?;
    // Save what the driver shows now for every remembered feature: switching one on can switch
    // another off (Chill and Anti-Lag), and the next attach must not set both.
    let now = keel_egpu::amdtune::read(&p).map(|t| t.features).unwrap_or_default();
    amd_save(&p, |s| {
        s.features.insert(name.clone(), on);
        for f in &now { if let Some(v) = s.features.get_mut(&f.name) { *v = f.on; } }
    })?;
    amd_log(&[format!("{name} {}", if on { "on" } else { "off" })]);
    Ok(())
}

fn print_amd_features() -> Result<()> {
    let cfg = config::Config::load();
    let (_, p) = attached_egpu(&cfg)?;
    if p.vendor_id != gpu::VENDOR_AMD { bail!("{} is not an AMD GPU", p.name); }
    let (name, features) = amd::features(&p)?;
    println!("{name}\n");
    for f in features {
        let state = match (f.supported, f.enabled) {
            (false, _) => "not supported".to_string(),
            (true, Some(on)) => if on { "on".into() } else { "off".into() },
            (true, None) => "supported".to_string(),
        };
        println!("  {:<10} {:<26} {state}", f.scope, f.name);
    }
    println!("\nDisplay features apply only to displays connected to the eGPU.");
    Ok(())
}

fn nv_scaling(args: &[String]) -> Result<()> {
    use keel_egpu::nvdisp;
    if let [gdi, v] = args {
        let gdi = if gdi.starts_with(r"\\.\") { gdi.clone() } else { format!(r"\\.\{gdi}") };
        let v: i32 = v.parse().context("value 0-8")?;
        let before = nvdisp::set_scaling(&gdi, v)?;
        let line = format!("{gdi}: scaling {before} ({}) -> {v} ({})", nvdisp::scaling_name(before), nvdisp::scaling_name(v));
        println!("{line}");
        keel_core::log::user(format!("egpu: nv-scaling: {line}"));
        return Ok(());
    }
    for d in display::active_displays() {
        if let Ok(id) = nvdisp::id_of(&d.gdi_name) { println!("{}  {}  display 0x{id:08x}", d.gdi_name, d.monitor); }
    }
    for l in nvdisp::describe()? { println!("{l}"); }
    Ok(())
}

/// Read-only: GPU Scaling / Scaling Mode / Integer Scaling of the AMD iGPU's displays (the
/// built-in screen), through ADLX loaded from the iGPU's own driver folder.
fn igpu_scaling(args: &[String]) -> Result<()> {
    let p = amd::igpu_profile()?;
    println!("{} ({:04X}:{:04X}), driver folder {}", p.name, p.vendor_id, p.device_id,
             amd::driver_dir(p.vendor_id, p.device_id).map_or("? (not readable)".into(), |d| d.display().to_string()));
    if let [what] = args {
        let on = match what.as_str() { "integer-on" => true, "integer-off" => false, _ => bail!("usage: keel egpu igpu-scaling [integer-on | integer-off]") };
        let monitor = amd::scaling_status(&p)?.into_iter().next().map(|s| s.display).ok_or_else(|| anyhow::anyhow!("no display on the iGPU"))?;
        for l in amd::scaling_write(&p, &monitor, amd::ScalingWrite::Integer(on))? {
            println!("{l}");
            keel_core::log::user(format!("egpu: igpu-scaling: {l}"));
        }
        return Ok(());
    }
    for s in amd::scaling_status(&p)? { println!("{s:?}"); }
    Ok(())
}

/// Per-user record of the AMD eGPU display scaling values before `amd-scaling on`.
fn scaling_record() -> PathBuf { util::user_dir().join("amd-scaling.json") }

fn amd_scaling(what: &str) -> Result<()> {
    let cfg = config::Config::load();
    let (_, p) = attached_egpu(&cfg)?;
    if p.vendor_id != gpu::VENDOR_AMD { bail!("{} is not an AMD GPU", p.name); }
    let rec = scaling_record();
    let lines = match what {
        "status" => {
            for s in amd::scaling_status(&p)? { println!("{s:?}"); }
            if rec.exists() { println!("recorded before 'on': {}", std::fs::read_to_string(&rec)?); }
            return Ok(());
        }
        "on" => {
            // Record the originals before the first change; a second 'on' keeps the first record.
            if !rec.exists() {
                let now = amd::scaling_status(&p)?;
                std::fs::create_dir_all(rec.parent().unwrap())?;
                std::fs::write(&rec, serde_json::to_string_pretty(&now)?)
                    .with_context(|| format!("cannot record {}; nothing changed", rec.display()))?;
            }
            amd::scaling_on(&p, None)?.1
        }
        "off" => {
            let saved: Vec<amd::Scaling> = serde_json::from_str(&std::fs::read_to_string(&rec)
                .with_context(|| format!("no record at {}", rec.display()))?)?;
            let l = amd::scaling_restore(&p, &saved)?;
            std::fs::remove_file(&rec)?;
            l
        }
        _ => bail!("usage: keel egpu amd-scaling status | on | off"),
    };
    for l in &lines { println!("{l}"); keel_core::log::user(format!("egpu: amd-scaling: {l}")); }
    Ok(())
}

/// Windows scale of an external display (the built-in screen belongs to Legion Space).
fn display_scale(name: Option<&str>, pct: Option<u32>) -> Result<()> {
    let (Some(name), Some(pct)) = (name, pct) else { bail!("usage: keel egpu display-scale DISPLAYn <percent>") };
    let d = display::active_displays().into_iter()
        .find(|d| d.gdi_name.eq_ignore_ascii_case(name) || d.gdi_name.to_ascii_uppercase().ends_with(&name.to_ascii_uppercase()))
        .ok_or_else(|| anyhow::anyhow!("{name}: not an active display"))?;
    if d.internal { bail!("{}: the built-in screen's scale belongs to Legion Space / Windows", d.gdi_name); }
    let before = display::scale(&d.gdi_name);
    display::set_scale(&d.gdi_name, pct)?;
    let pct = |s: Option<u32>| s.map_or("?".to_string(), |s| format!("{s}%"));
    println!("{} ({}): scale {} -> {}", d.gdi_name, d.monitor, pct(before), pct(display::scale(&d.gdi_name)));
    Ok(())
}

fn print_displays() -> Result<()> {
    let adapters = gpu::adapters()?;
    let pos = display::positions();
    for d in display::active_displays() {
        let gpu = adapters.iter().find(|a| a.luid == d.adapter_luid).map(|a| a.name.as_str()).unwrap_or("?");
        let cur = display::current_mode(&d.gdi_name).map(|m| m.to_string()).unwrap_or_default();
        let at = pos.iter().find(|(n, _, _)| *n == d.gdi_name)
            .map(|(_, x, y)| format!("  at {x},{y}{}", if (*x, *y) == (0, 0) { " (main display)" } else { "" })).unwrap_or_default();
        let sc = display::scale(&d.gdi_name).map(|s| format!("  scale {s}%")).unwrap_or_default();
        println!("{}  {}{}\n  GPU: {gpu}\n  now: {cur}{at}{sc}", d.gdi_name, d.monitor, if d.internal { " (built-in)" } else { "" });
        let modes: Vec<String> = display::modes(&d.gdi_name).iter().take(12).map(|m| m.to_string()).collect();
        println!("  modes: {}\n", modes.join(", "));
    }
    let text = match display::font_smoothing() { Some(1) => "grayscale", Some(2) => "ClearType", Some(_) => "other", None => "?" };
    println!("Text smoothing (all displays): {text}");
    Ok(())
}

fn legion_fix(what: &str) -> Result<()> {
    match what {
        "on" => {
            let mut cfg = config::Config::load_or_create()?;
            let dir = legion::igpu_dir(&cfg).ok_or_else(|| anyhow::anyhow!("iGPU driver folder not found"))?;
            for d in legion::sync_adl(&dir)? { println!("copied iGPU ADL into {}", d.display()); }
            cfg.legion_space_fix = true;
            cfg.save()?;
            println!("on. Restart Legion Space to apply.");
        }
        "off" => {
            let mut cfg = config::Config::load_or_create()?;
            cfg.legion_space_fix = false;
            cfg.save()?;
            for d in legion::remove_adl() { println!("removed from {}", d.display()); }
            println!("off. Restart Legion Space to apply.");
        }
        _ => {
            let cfg = config::Config::load();
            println!("legion_space_fix = {}", cfg.legion_space_fix);
            let igpu = legion::igpu_dir(&cfg);
            for (d, ok) in legion::adl_status(igpu.as_deref()) {
                println!("  {}  {}", d.display(), if ok { "iGPU ADL" } else { "System32 ADL (may see no GPU)" });
            }
        }
    }
    Ok(())
}

fn prefer_gpu(args: &[String]) -> Result<()> {
    let Some(exe) = args.get(1).filter(|a| !a.starts_with("--")) else {
        bail!("usage: keel egpu prefer-gpu <full path to game.exe> [--off]")
    };
    let exe = std::fs::canonicalize(PathBuf::from(exe))?;
    let exe = exe.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let (key, _) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .create_subkey(r"Software\Microsoft\DirectX\UserGpuPreferences")?;
    if args.iter().any(|a| a == "--off") {
        let _ = key.delete_value(&exe);
        println!("removed GPU preference for {exe}");
    } else {
        key.set_value(&exe, &"GpuPreference=2;".to_string())?;
        println!("{exe} -> high-performance GPU (the connected eGPU)");
    }
    Ok(())
}

fn run_guard() -> Result<()> {
    keel_core::paths::ensure_may_write_data_dir()?;
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    on_ctrl_c(move || s.store(true, Ordering::SeqCst));
    guard::run(|| stop.load(Ordering::SeqCst))
}

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("status") => print_status(),
        Some("run") => run_guard(),
        Some("driver-status") => {
            let cfg = config::Config::load();
            let s = driver::status()?;
            println!("{s}");
            for (v, note) in cfg.driver_known_issues.iter().filter(|(v, _)| s.contains(v.as_str())) {
                println!("\nKnown issue: {v} - {note}");
            }
            Ok(())
        }
        Some("driver-update") => {
            let cfg = config::Config::load_or_create()?;
            let Some(src) = args.get(1).filter(|a| !a.starts_with("--")) else {
                bail!("usage: keel egpu driver-update <installer.exe | folder | .inf>")
            };
            let hwid = hardware_id(args, &cfg)?;
            driver::update(&PathBuf::from(src), &hwid, !args.iter().any(|a| a == "--no-block"),
                           args.iter().any(|a| a == "--force"), &cfg.driver_known_issues)?;
            println!("{}", driver::status()?);
            Ok(())
        }
        Some("driver-find") => {
            let cfg = config::Config::load();
            let Some(src) = args.get(1).filter(|a| !a.starts_with("--")) else { bail!("usage: keel egpu driver-find <path>") };
            println!("{}", driver::locate_inf(&PathBuf::from(src), &hardware_id(args, &cfg)?)?.display());
            Ok(())
        }
        Some("driver-block") => { let cfg = config::Config::load_or_create()?; driver::block(&hardware_id(args, &cfg)?) }
        Some("driver-unblock") => { let cfg = config::Config::load_or_create()?; driver::unblock(&hardware_id(args, &cfg)?) }
        Some("apps") => {
            let mut w = apps::AppWatch::new().ok_or_else(|| anyhow::anyhow!("process list unavailable"))?;
            let (state, running) = w.check();
            for r in running { println!("{}", r.replace("app started: ", "running: ")); }
            println!("Adrenalin: {state:?}");
            Ok(())
        }
        Some("prefer-gpu") => prefer_gpu(args),
        Some("games") => {
            for f in keel_egpu::scan::installed() {
                let on = if keel_egpu::gpupref::is_high_performance(&f.exe.to_string_lossy()) { "eGPU" } else { "-" };
                println!("{:<6} {:<5} {:<40} {}", f.source, on, f.name, f.exe.display());
            }
            Ok(())
        }
        Some("telemetry") => print_telemetry(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5)),
        Some("amd-features") => print_amd_features(),
        Some("amd-tune") => amd_tune(&args[1..]),
        Some("amd-feature") => amd_feature(&args[1..]),
        Some("nv-3d") => {
            if args.get(1).map(String::as_str) == Some("--json") { println!("{}", serde_json::to_string(&keel_egpu::nvdrs::read()?)?); return Ok(()); }
            for c in keel_egpu::nvdrs::read()? {
                let v = c.value.map_or("driver default".to_string(), |v| format!("{v} ({v:#x})"));
                println!("{:<18} {:<12} {:<40} driver: {}", c.label, format!("{:#x}", c.id), v, c.driver_name);
            }
            Ok(())
        }
        Some("amd-scaling") => amd_scaling(args.get(1).map(String::as_str).unwrap_or("status")),
        Some("nv-scaling") => nv_scaling(&args[1..]),
        Some("igpu-scaling") => igpu_scaling(&args[1..]),
        Some("stress") => keel_egpu::stress::run(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60)),
        Some("display") => print_displays(),
        Some("fit-windows") => {
            // Physical pixels on every monitor, like the tray.
            unsafe { let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
            for d in display::active_displays().into_iter().filter(|d| !d.internal) {
                let l = display::fit_fullscreen(&d.gdi_name);
                println!("{}: {}", d.gdi_name, if l.is_empty() { "nothing to fit".to_string() } else { l.join("; ") });
            }
            Ok(())
        }
        Some("display-scale") => display_scale(args.get(1).map(String::as_str), args.get(2).and_then(|s| s.trim_end_matches('%').parse().ok())),
        Some("legion-fix") => legion_fix(args.get(1).map(String::as_str).unwrap_or("status")),
        Some("config") => {
            let own = util::config_path();
            println!("config: {}{}\nlog:    {}\nstatus: {}", own.display(), if own.exists() { "" } else { " (none yet: defaults)" },
                util::log_path().display(), util::status_path().display());
            Ok(())
        }
        None | Some("help" | "-h" | "--help") => { print!("{HELP}"); Ok(()) }
        Some(x) => bail!("unknown command egpu {x}; see `keel egpu help`"),
    }
}

/// Minimal Ctrl+C hook for `run` (SetConsoleCtrlHandler).
pub fn on_ctrl_c(f: impl Fn() + Send + Sync + 'static) {
    use std::sync::OnceLock;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    use windows::core::BOOL;
    static HANDLER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
    let _ = HANDLER.set(Box::new(f));
    unsafe extern "system" fn on_ctrl(_: u32) -> BOOL {
        if let Some(h) = HANDLER.get() { h(); }
        BOOL(1)
    }
    unsafe { let _ = SetConsoleCtrlHandler(Some(on_ctrl), true); }
}

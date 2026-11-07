//! The watch loop: eGPU attach/detach, NVIDIA limits + watchdog, eGPU telemetry (NVML / ADLX /
//! PDH), game detection, status file.
use crate::config::Config;
use crate::coord::{ollama_unload_all, GpuLock};
use crate::games::GameWatch;
use crate::gpu::{self, Adapter, VENDOR_NVIDIA};
use crate::nvidia::{NvGuard, Telemetry};
use crate::telemetry::{self, GpuBackend, GpuTelemetry};
use crate::util::{alert, log, recent_alerts, status_path, Alert};
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Serialize)]
struct Status<'a> {
    utc: String,
    started_utc: &'a str,
    adapters: &'a [Adapter],
    egpus: Vec<&'a str>,
    nvidia: Option<Telemetry>,
    /// The attached eGPU, any vendor (added in 0.3; `nvidia` stays for older readers).
    telemetry: Option<GpuTelemetry>,
    game: Option<&'a str>,
    game_exe: Option<&'a str>,
    gpu_lock_held: bool,
    /// NVIDIA services Keel stopped because no NVIDIA GPU is present.
    nvidia_services_stopped: &'a [String],
    alerts: Vec<Alert>,
}

pub fn run(stop: impl Fn() -> bool) -> anyhow::Result<()> {
    let mut cfg = Config::load_or_create()?;
    log(format!("guard: started (pid {})", std::process::id()));
    let started = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut nv: Option<NvGuard> = None;
    let mut nv_retry = Instant::now();
    let (mut nv_attached_once, mut nv_fails) = (false, 0u32);
    let mut last: Vec<Adapter> = Vec::new();
    let mut games = GameWatch::new();
    let mut game: Option<(String, String)> = None;
    let mut lock = cfg.gpu_lock.as_deref().map(GpuLock::new);
    // Telemetry for an eGPU without an NvGuard: (adapter LUID, ADLX or PDH backend).
    let mut mon: Option<(u64, Box<dyn GpuBackend>)> = None;
    let mut mon_retry = Instant::now();
    let mut legion_check = Instant::now();
    let mut nv_svc_present: Option<bool> = None;
    let mut nv_svc_stopped: Vec<String> = Vec::new();
    let mut nv_absent_since: Option<Instant> = None;
    let mut nv_svc_bad: Vec<String> = Vec::new();

    while !stop() {
        let listed = gpu::adapters();
        // NVIDIA services act on a settled presence: an absence counts after ABSENT_GRACE.
        let present = crate::nvsvc::settled(crate::nvsvc::nvidia_present(listed.as_deref().ok()), &mut nv_absent_since,
                                            Instant::now(), crate::nvsvc::ABSENT_GRACE);
        let adapters = listed.unwrap_or_else(|e| { log(format!("dxgi: {e}")); Vec::new() });
        // A new external GPU (Windows expects it to be unplugged) gets its own profile.
        if adapters != last && adapters.iter().any(|a| cfg.profile_for(a.vendor_id, a.device_id).is_none()) {
            let ext = gpu::external_devices();
            let found: Vec<(String, u32, u32, String)> = adapters.iter()
                .filter_map(|a| ext.iter().find(|e| e.0 == a.vendor_id && e.1 == a.device_id).map(|e| (a.name.clone(), e.0, e.1, e.2.clone())))
                .collect();
            let added = cfg.add_found(&found);
            if !added.is_empty() {
                match cfg.save() {
                    Ok(()) => log(format!("new eGPU profile: {}", added.join(", "))),
                    Err(e) => log(format!("new eGPU profile not saved: {e:#}")),
                }
            }
        }
        let egpus: Vec<&Adapter> = adapters.iter()
            .filter(|a| cfg.profile_for(a.vendor_id, a.device_id).is_some()).collect();

        if adapters != last {
            let names: Vec<&str> = adapters.iter().map(|a| a.name.as_str()).collect();
            log(format!("adapters: {names:?}"));
            let lost = last.iter().any(|a| cfg.profile_for(a.vendor_id, a.device_id).is_some() && !adapters.contains(a));
            if lost {
                alert("eGPU disconnected - Ollama models unloaded");
                ollama_unload_all(&cfg.ollama_url);
            }
            for a in adapters.iter().filter(|a| cfg.profile_for(a.vendor_id, a.device_id).is_some() && a.vendor_id != VENDOR_NVIDIA) {
                if !last.contains(a) { alert(format!("{} attached", a.name)); }
            }
            last = adapters.clone();
        }

        // NVIDIA eGPU: attach (apply limits) when it appears, drop when it goes away.
        let nv_profile = egpus.iter().find(|a| a.vendor_id == VENDOR_NVIDIA)
            .and_then(|a| cfg.profile_for(a.vendor_id, a.device_id));
        match (nv_profile, nv.is_some()) {
            (Some(p), false) if Instant::now() >= nv_retry => match NvGuard::attach(p) {
                Ok(g) => { alert(format!("{} attached - limits applied", p.name)); nv = Some(g); nv_attached_once = true; nv_fails = 0; }
                Err(e) => {
                    log(format!("{} not ready: {e}", p.name));
                    nv_retry = Instant::now() + Duration::from_secs(30);
                    nv_fails += 1;
                    // NVIDIA's DLLs stay pinned in this process (nvidia.rs), so after a driver update
                    // NVML may not start here again. A fresh process loads the new driver's DLLs:
                    // exit, and the SCM's recovery action restarts the service.
                    if nv_attached_once && nv_fails >= 3 && crate::util::in_service() {
                        alert(format!("{}: NVML does not start again in this process (driver updated?); restarting the service", p.name));
                        std::thread::sleep(Duration::from_secs(1));
                        std::process::exit(3);
                    }
                }
            },
            (None, true) => { nv = None; log("NVIDIA detached"); }
            _ => {}
        }
        let telemetry = nv.as_mut().and_then(|g| g.tick());
        let mut egpu_tel = telemetry.as_ref().zip(egpus.iter().find(|a| a.vendor_id == VENDOR_NVIDIA))
            .map(|(t, a)| GpuTelemetry { name: a.name.clone(), ..t.into() });

        // Other eGPUs (AMD via ADLX, anything via PDH): open on attach, release on detach so the
        // ADLX DLLs only stay loaded while the eGPU is there.
        let other = egpus.iter().find(|a| !(a.vendor_id == VENDOR_NVIDIA && nv.is_some()));
        match other {
            Some(a) if mon.as_ref().is_none_or(|(l, _)| *l != a.luid) && Instant::now() >= mon_retry => {
                mon = cfg.profile_for(a.vendor_id, a.device_id).and_then(|p| telemetry::open(a, p)).map(|b| (a.luid, b));
                if mon.is_none() { mon_retry = Instant::now() + Duration::from_secs(60); }
            }
            None if mon.is_some() => { mon = None; log("telemetry: eGPU backend released"); }
            _ => {}
        }
        if egpu_tel.is_none() {
            if let Some((_, b)) = mon.as_mut() {
                egpu_tel = b.tick();
                if egpu_tel.is_none() { mon = None; mon_retry = Instant::now() + Duration::from_secs(10); }
            }
        }

        // Games: free VRAM for the game and keep cooperating GPU jobs waiting until it exits.
        let now_game = games.running_game(&cfg);
        if now_game != game {
            match &now_game {
                Some((n, _)) => { log(format!("game: {n} started")); ollama_unload_all(&cfg.ollama_url); }
                None => { log("game: exited"); if let Some(l) = lock.as_mut() { l.release(); } }
            }
            game = now_game;
        }
        let held = match (&game, lock.as_mut()) { (Some(_), Some(l)) => l.hold(), _ => false };

        // Legion Space ADL redirect (opt-in, re-read so `legion-fix on` needs no restart).
        // NVIDIA services follow the NVIDIA GPU (opt-in): on a presence change, and once a minute
        // with the re-read config (catches the option being switched and services restarted by others).
        let minute = Instant::now() >= legion_check;
        if minute || (present.is_some() && present != nv_svc_present) {
            let c = Config::load();
            if minute {
                legion_check = Instant::now() + Duration::from_secs(60);
                if c.legion_space_fix { crate::legion::maintain(&c); }
            }
            let bad = crate::nvsvc::allowed(&c.nvidia_services).1;
            if c.nvidia_services_follow_egpu && !bad.is_empty() && bad != nv_svc_bad {
                log(format!("nvidia-services: not allowed, ignored: {bad:?} (allowed: {:?})", crate::nvsvc::ALLOWED));
            }
            nv_svc_bad = bad;
            if present.is_some() { nv_svc_present = present; }
            nv_svc_stopped = crate::nvsvc::sync(c.nvidia_services_follow_egpu, &c.nvidia_services, present);
        }

        let status = Status {
            utc: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            started_utc: &started,
            adapters: &adapters,
            egpus: egpus.iter().map(|a| a.name.as_str()).collect(),
            nvidia: telemetry,
            telemetry: egpu_tel,
            game: game.as_ref().map(|g| g.0.as_str()),
            game_exe: game.as_ref().map(|g| g.1.as_str()).filter(|s| !s.is_empty()),
            gpu_lock_held: held,
            nvidia_services_stopped: &nv_svc_stopped,
            alerts: recent_alerts(),
        };
        if let Ok(s) = serde_json::to_string_pretty(&status) { let _ = std::fs::write(status_path(), s); }

        let until = Instant::now() + Duration::from_secs(cfg.poll_secs.max(1));
        while Instant::now() < until && !stop() { std::thread::sleep(Duration::from_millis(200)); }
    }
    log("guard: stopped");
    Ok(())
}

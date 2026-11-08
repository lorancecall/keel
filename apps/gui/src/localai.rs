//! Local AI (ui/localai.slint: eGPU memory above the GPU apps tabs, and the Local AI tab), built around three questions:
//! 1. Is there room on the eGPU for a model now? (its VRAM: total, used by apps, free)
//! 2. Which installed models fit? (Ollama's manifests on disk, so it works with Ollama off)
//! 3. What AI runs now, and how much RAM does it take? (Ollama's API when its server runs, other
//!    runtimes by process; unload a model, end Ollama to get its memory back)
//! Keel never starts Ollama (`ollama ps` from the CLI starts the Ollama app, 3.2 GB). Read on a
//! worker thread every few seconds while the page is open.
use crate::{AiModel, AiProc, LocalAiData, MainWindow};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const OLLAMA: &str = "http://127.0.0.1:11434";
/// Process names of local AI runtimes (lower case) and what to call them.
const RUNTIMES: &[(&str, &str)] = &[
    ("ollama.exe", "Ollama"), ("ollama app.exe", "Ollama"), ("lm studio.exe", "LM Studio"), ("lms.exe", "LM Studio"),
    ("llama-server.exe", "llama.cpp"), ("llama-cli.exe", "llama.cpp"), ("koboldcpp.exe", "KoboldCpp"), ("jan.exe", "Jan"),
];
/// Room a loaded model needs beyond its file: context (KV cache) and buffers. A rule of thumb:
/// 10 % plus 0.5 GB.
fn needed(size: u64) -> u64 { size + size / 10 + (512 << 20) }

fn gb(b: u64) -> String { if b >= 1 << 30 { format!("{:.1} GB", b as f64 / (1u64 << 30) as f64) } else { format!("{} MB", b >> 20) } }

// ------------------------------------------------------------------ Ollama

#[derive(serde::Deserialize, Default)]
struct Details { #[serde(default)] parameter_size: String, #[serde(default)] quantization_level: String }

#[derive(serde::Deserialize)]
struct Loaded { name: String, size: u64, #[serde(default)] size_vram: u64, #[serde(default)] expires_at: String, #[serde(default)] details: Details }

#[derive(serde::Deserialize)]
struct List<T> { models: Vec<T> }

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_millis(1500))).build().into()
}

fn get<T: serde::de::DeserializeOwned>(path: &str) -> anyhow::Result<T> {
    Ok(agent().get(&format!("{OLLAMA}{path}")).call()?.body_mut().read_json()?)
}

/// Asks Ollama to drop a model from memory now (keep_alive 0).
fn unload(model: &str) -> anyhow::Result<()> {
    agent().post(&format!("{OLLAMA}/api/generate")).send_json(serde_json::json!({ "model": model, "keep_alive": 0 }))?;
    Ok(())
}

/// Ollama's model folder (`OLLAMA_MODELS`, else `%USERPROFILE%\.ollama\models`).
fn models_dir() -> Option<PathBuf> {
    std::env::var_os("OLLAMA_MODELS").map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".ollama").join("models")))
}

/// Installed models from the manifests on disk: name (`model:tag`) and the size of the weights.
fn installed() -> Vec<(String, u64)> {
    let Some(root) = models_dir().map(|d| d.join("manifests")) else { return Vec::new() };
    let mut out = Vec::new();
    // manifests\<registry>\<namespace>\<model>\<tag>
    let walk = |p: &std::path::Path| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).collect::<Vec<_>>();
    for reg in walk(&root) {
        for ns in walk(&reg) {
            for model in walk(&ns) {
                for tag in walk(&model).into_iter().filter(|p| p.is_file()) {
                    let Some(v) = std::fs::read_to_string(&tag).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else { continue };
                    let size: u64 = v["layers"].as_array().map(|l| l.iter()
                        .filter(|x| x["mediaType"].as_str().is_some_and(|m| m.ends_with(".model") || m.ends_with(".projector")))
                        .filter_map(|x| x["size"].as_u64()).sum()).unwrap_or(0);
                    if size == 0 { continue; }
                    let file = |p: &std::path::Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let ns_name = file(&ns);
                    let name = if ns_name == "library" { file(&model) } else { format!("{ns_name}/{}", file(&model)) };
                    out.push((format!("{name}:{}", file(&tag)), size));
                }
            }
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

// ------------------------------------------------------------------ snapshot

pub struct Snapshot {
    /// eGPU name, VRAM total and used (Task Manager's value), its largest users (name, bytes).
    egpu: Option<(String, u64, u64, Vec<(String, u64)>)>,
    installed: Vec<(String, u64)>,
    /// Ollama's server answered: the loaded models.
    loaded: Option<Vec<Loaded>>,
    /// Runtime processes: label, RAM (as Task Manager), GPU memory on the eGPU.
    procs: Vec<(String, u64, u64)>,
    /// The service's on-demand entries (AI mode's state), None when it does not answer.
    od: Option<Vec<keel_ram::ondemand::Entry>>,
}

/// AI mode table rows (pure; tested): app name, now, note.
pub fn ai_rows(apps: &[keel_core::traycfg::AiApp], entries: Option<&[keel_ram::ondemand::Entry]>) -> Vec<[String; 3]> {
    use keel_ram::ondemand::Mode;
    apps.iter().map(|a| {
        let e = entries.and_then(|v| v.iter().find(|e| e.key == a.key));
        let (now, note) = match (entries, e) {
            (None, _) => ("?", "the Keel service does not answer"),
            (_, Some(e)) if e.ai_off => ("Off", "by AI mode, back when the model unloads"),
            (_, Some(e)) if !e.on && e.mode == Mode::Off => ("Off", "kept off (On-demand apps)"),
            (_, Some(e)) if !e.on => ("Off", "until its app runs (On-demand apps)"),
            (_, Some(_)) => ("On", "On-demand apps: follows its app"),
            (_, None) => ("—", "switched off when a model loads"),
        };
        [a.name.clone(), now.to_string(), note.to_string()]
    }).collect()
}

/// eGPU adapter key (as the GPU counters name it), name and VRAM from the service's status file.
fn egpu_status() -> Option<(String, String, u64, u64)> {
    let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(keel_egpu::util::status_path()).ok()?).ok()?;
    let egpus: Vec<&str> = s["egpus"].as_array()?.iter().filter_map(|v| v.as_str()).collect();
    let a = s["adapters"].as_array()?.iter().find(|x| x["name"].as_str().is_some_and(|n| egpus.contains(&n)))?;
    let luid = a["luid"].as_u64()?;
    let t = &s["telemetry"];
    let total = t["vram_total_mb"].as_u64().or_else(|| a["vram_mb"].as_u64())? << 20;
    let used = t["vram_used_mb"].as_u64().unwrap_or(0) << 20;
    Some((keel_ram::gpu::luid_key((luid >> 32) as i32, luid as u32), a["name"].as_str()?.to_string(), total, used))
}

/// Worker thread.
pub fn snapshot() -> Snapshot {
    let eg = egpu_status();
    let gmem = keel_ram::gpu::process_memory();
    let mut on_egpu: HashMap<u32, u64> = HashMap::new();
    if let Some((key, ..)) = &eg {
        for ((pid, a), m) in &gmem { if a == key { *on_egpu.entry(*pid).or_default() += m.committed; } }
    }
    let ps = keel_ram::procs::snapshot().unwrap_or_default();
    let mut users: Vec<(String, u64)> = ps.iter().filter_map(|p| on_egpu.get(&p.pid).map(|b| (p.name.clone(), *b))).filter(|x| x.1 >= 16 << 20).collect();
    users.sort_by(|a, b| b.1.cmp(&a.1));
    // One line per runtime: its processes added up.
    let mut procs: Vec<(String, u64, u64)> = Vec::new();
    for p in &ps {
        let lower = p.name.to_lowercase();
        let Some((_, label)) = RUNTIMES.iter().find(|(n, _)| *n == lower) else { continue };
        let g = on_egpu.get(&p.pid).copied().unwrap_or(0);
        match procs.iter_mut().find(|x| x.0 == *label) {
            Some(x) => { x.1 += p.private_ws; x.2 += g; }
            None => procs.push((label.to_string(), p.private_ws, g)),
        }
    }
    let loaded = get::<serde_json::Value>("/api/version").ok().map(|_| get::<List<Loaded>>("/api/ps").map(|l| l.models).unwrap_or_default());
    Snapshot { egpu: eg.map(|(_, n, t, u)| (n, t, u, users.into_iter().take(3).collect())), installed: installed(), loaded, procs,
               od: keel_ram::pipe::call(&keel_ram::ondemand::Request::Status).ok().map(|r| r.state.entries) }
}

fn until(expires: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(expires).ok().map(|t| {
        let m = (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_minutes();
        if m > 60 * 24 * 365 { "stays loaded".to_string() } else if m >= 0 { format!("unloads by itself in {m} min") } else { "unloading".into() }
    }).unwrap_or_default()
}

pub fn show(ui: &MainWindow, s: Snapshot) {
    let d = ui.global::<LocalAiData>();
    let p = keel_core::traycfg::Prefs::load();
    d.set_ai_mode(p.ai_mode);
    d.set_ai_apps(p.ai_mode_apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ").into());
    let rows: Vec<ModelRc<slint::StandardListViewItem>> = ai_rows(&p.ai_mode_apps, s.od.as_deref()).into_iter()
        .map(|r| ModelRc::new(VecModel::from(r.into_iter().map(|c| slint::StandardListViewItem::from(slint::SharedString::from(c))).collect::<Vec<_>>())))
        .collect();
    d.set_ai_rows(ModelRc::new(VecModel::from(rows)));
    // 1. Room on the eGPU.
    let free = match &s.egpu {
        Some((name, total, used, users)) => {
            let free = total.saturating_sub(*used);
            d.set_egpu_name(name.clone().into());
            d.set_vram_text(format!("{} free / {}", gb(free), gb(*total)).into());
            d.set_vram_frac(*used as f32 / (*total).max(1) as f32);
            d.set_vram_users(if users.is_empty() { "Used by: nothing much".into() } else {
                format!("Used by: {}", users.iter().map(|(n, b)| format!("{n} {}", gb(*b))).collect::<Vec<_>>().join(", ")).into()
            });
            Some((free, *total))
        }
        None => {
            d.set_egpu_name(SharedString::new());
            d.set_vram_text("No eGPU attached".into());
            d.set_vram_frac(0.0);
            d.set_vram_users("Models run on the CPU, in system memory.".into());
            None
        }
    };
    // 2. Installed models against that room.
    let fits: Vec<AiModel> = s.installed.iter().map(|(name, size)| {
        let need = needed(*size);
        let (state, text) = match free {
            None => (2, "no eGPU".to_string()),
            Some((f, _)) if need <= f => (0, "Fits".into()),
            Some((f, t)) if need <= t => (1, format!("{} over (close apps on the eGPU)", gb(need - f))),
            Some((f, _)) => (2, format!("Too big: {} in system memory", gb(need.saturating_sub(f)))),
        };
        AiModel { name: name.clone().into(), info: format!("needs ~{}", gb(need)).into(), size: gb(*size).into(), state, text: text.into(), gpu_frac: 0.0 }
    }).collect();
    d.set_installed(ModelRc::new(VecModel::from(fits)));
    d.set_installed_note(match models_dir() {
        Some(p) if s.installed.is_empty() => format!("No Ollama models in {}.", p.display()),
        Some(p) => format!("Ollama · {}", p.display()),
        None => String::new(),
    }.into());
    // 3. What runs now.
    let loaded: Vec<AiModel> = s.loaded.as_ref().map(|v| v.iter().map(|m| {
        let in_ram = m.size.saturating_sub(m.size_vram);
        let frac = if m.size > 0 { m.size_vram as f32 / m.size as f32 } else { 0.0 };
        let text = if in_ram == 0 { format!("GPU {} · unload before unplugging", gb(m.size)) }
                   else if m.size_vram == 0 { format!("RAM {} (CPU)", gb(m.size)) }
                   else { format!("GPU {} · RAM {}", gb(m.size_vram), gb(in_ram)) };
        AiModel {
            name: m.name.clone().into(),
            info: [m.details.parameter_size.as_str(), m.details.quantization_level.as_str(), &until(&m.expires_at)].iter()
                .filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" · ").into(),
            size: gb(m.size).into(), state: if in_ram == 0 { 0 } else { 1 }, text: text.into(), gpu_frac: frac,
        }
    }).collect()).unwrap_or_default();
    d.set_loaded(ModelRc::new(VecModel::from(loaded)));
    let procs: Vec<AiProc> = s.procs.iter().map(|(n, ram, g)| AiProc { name: n.clone().into(), memory: gb(*ram).into(), egpu: gb(*g).into(), other_gpu: SharedString::new() }).collect();
    let ollama_ram: u64 = s.procs.iter().filter(|p| p.0 == "Ollama").map(|p| p.1).sum();
    d.set_ollama_running(s.procs.iter().any(|p| p.0 == "Ollama"));
    d.set_running_text(match (s.procs.is_empty(), &s.loaded) {
        (true, _) => "Nothing".to_string(),
        (false, Some(v)) if v.is_empty() => format!("Ollama, no model loaded{}", if ollama_ram > 0 { format!(" · RAM {}", gb(ollama_ram)) } else { String::new() }),
        (false, _) => String::new(),
    }.into());
    d.set_procs(ModelRc::new(VecModel::from(procs)));
}

pub fn wire(ui: &MainWindow) {
    let d = ui.global::<LocalAiData>();
    let w = ui.as_weak();
    d.on_ai_mode_changed(move || if let Some(ui) = w.upgrade() {
        let mut p = keel_core::traycfg::Prefs::load();
        p.ai_mode = ui.global::<LocalAiData>().get_ai_mode();
        let _ = p.save();
        ui.global::<LocalAiData>().set_result(if p.ai_mode { "AI mode on".into() } else { "AI mode off".into() });
    });
}

pub fn refresh(ui: &MainWindow) {
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let s = snapshot();
        let _ = w.upgrade_in_event_loop(move |ui| show(&ui, s));
    });
}

pub fn unload_one(ui: &MainWindow, name: String) {
    let w = ui.as_weak();
    ui.global::<LocalAiData>().set_result(format!("Unloading {name}...").into());
    std::thread::spawn(move || {
        let r = unload(&name);
        keel_core::log::user(format!("local ai: unload {name}: {r:?}"));
        let _ = w.upgrade_in_event_loop(move |ui| {
            ui.global::<LocalAiData>().set_result(match r { Ok(()) => format!("{name} unloaded.").into(), Err(e) => format!("Not unloaded: {e}").into() });
            refresh(&ui);
        });
    });
}

/// Ends the Ollama app and its server (the user's own processes) to give their memory back.
pub fn stop_ollama(ui: &MainWindow) {
    let w = ui.as_weak();
    ui.global::<LocalAiData>().set_result("Ending Ollama...".into());
    std::thread::spawn(move || {
        let r = keel_core::cmd::hidden("taskkill.exe").args(["/im", "ollama app.exe", "/im", "ollama.exe"]).output();
        let text = match &r {
            Ok(o) if o.status.success() => "Ollama ended; its memory is free again.".to_string(),
            Ok(o) => format!("Not all ended: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => format!("Could not run taskkill: {e}"),
        };
        keel_core::log::user(format!("local ai: stop ollama: {text}"));
        let _ = w.upgrade_in_event_loop(move |ui| { ui.global::<LocalAiData>().set_result(text.into()); refresh(&ui); });
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn room_for_context() {
        assert_eq!(super::needed(1 << 30), (1 << 30) + (1 << 30) / 10 + (512 << 20));
    }
}

#[cfg(test)]
mod ai_tests {
    use super::ai_rows;
    use keel_core::traycfg::AiApp;
    use keel_ram::ondemand::{Entry, Mode};

    #[test]
    fn ai_mode_rows() {
        let app = |k: &str| AiApp { key: k.into(), name: k.to_uppercase() };
        let e = |k: &str, mode, on, ai_off| Entry { family: k.into(), key: k.into(), mode, apps: vec![], services: vec![], tasks: vec![],
            startup: vec![], on, dirs: vec![], since: String::new(), ai_off, ai_only: ai_off };
        let entries = vec![e("adobe", Mode::Off, false, true), e("onedrive", Mode::Follow, true, false)];
        let rows = ai_rows(&[app("adobe"), app("onedrive"), app("zoom")], Some(&entries));
        assert_eq!(rows.iter().map(|r| r[1].as_str()).collect::<Vec<_>>(), ["Off", "On", "—"]);
        assert_eq!(ai_rows(&[app("zoom")], None)[0][1], "?");
    }
}

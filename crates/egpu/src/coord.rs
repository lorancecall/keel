//! Coordination with local AI work: unload Ollama models and hold the shared GPU lock while
//! a game runs.
use crate::util::log;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(5))).build().into()
}

/// Number of models Ollama holds in memory now; None when Ollama does not answer.
pub fn ollama_loaded(url: &str) -> Option<usize> {
    let mut resp = agent().get(&format!("{url}/api/ps")).call().ok()?;
    let v: serde_json::Value = resp.body_mut().read_json().ok()?;
    Some(v["models"].as_array().map_or(0, |m| m.len()))
}

/// Asks Ollama to unload every resident model. Ollama not running is not an error.
pub fn ollama_unload_all(url: &str) -> Vec<String> {
    let a = agent();
    let Ok(mut resp) = a.get(&format!("{url}/api/ps")).call() else { return vec![] };
    let Ok(ps) = resp.body_mut().read_json::<serde_json::Value>() else { return vec![] };
    let mut done = vec![];
    for m in ps["models"].as_array().into_iter().flatten() {
        let Some(name) = m["model"].as_str().or(m["name"].as_str()) else { continue };
        let body = serde_json::json!({"model": name, "keep_alive": 0});
        if a.post(&format!("{url}/api/generate")).send_json(&body).is_ok() { done.push(name.to_string()); }
    }
    if !done.is_empty() { log(format!("ollama: unloaded {done:?}")); }
    done
}

/// The GPU lock held as holder "game". Rewritten every 10 min so other holders never judge it
/// stale (30 min) while the game is still running.
pub struct GpuLock {
    path: PathBuf,
    held: bool,
    refreshed: Instant,
}

impl GpuLock {
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), held: false, refreshed: Instant::now() }
    }

    fn body() -> String {
        serde_json::json!({
            "pid": std::process::id(),
            "holder": "game",
            "utc": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        }).to_string()
    }

    /// Tries to take (or refresh) the lock; returns whether it is held.
    pub fn hold(&mut self) -> bool {
        if self.held {
            if self.refreshed.elapsed() >= Duration::from_secs(600) {
                let _ = std::fs::write(&self.path, Self::body());
                self.refreshed = Instant::now();
            }
            return true;
        }
        if let Some(dir) = self.path.parent() { let _ = std::fs::create_dir_all(dir); }
        let created = std::fs::OpenOptions::new().write(true).create_new(true).open(&self.path);
        match created {
            Ok(mut f) => {
                use std::io::Write;
                let _ = f.write_all(Self::body().as_bytes());
                self.held = true;
                self.refreshed = Instant::now();
                log(format!("lock: holding {} for the game", self.path.display()));
            }
            Err(_) => {}  // another GPU job holds it; retried next tick
        }
        self.held
    }

    pub fn release(&mut self) {
        if !self.held { return; }
        // Only remove the file if it is still ours.
        let ours = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|v| v["pid"].as_u64() == Some(std::process::id() as u64) && v["holder"] == "game")
            .unwrap_or(false);
        if ours { let _ = std::fs::remove_file(&self.path); }
        self.held = false;
        log("lock: released GPU lock");
    }
}

impl Drop for GpuLock {
    fn drop(&mut self) { self.release(); }
}

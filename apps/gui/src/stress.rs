//! The window's stress test: runs `keel egpu stress <secs>` hidden (the load), and sums up what
//! the eGPU did meanwhile from the page's own telemetry history (power, temperature, clock and
//! what held the clock back), so the numbers are the same ones the service reports.
use std::io::BufRead;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const LENGTHS: [u64; 3] = [60, 180, 600];

pub struct Run {
    child: Child,
    pub started: Instant,
    pub secs: u64,
    out: Arc<Mutex<Vec<String>>>,
}

impl Run {
    pub fn start(keel: &std::path::Path, secs: u64) -> std::io::Result<Self> {
        let mut child = keel_core::cmd::hidden(keel).args(["egpu", "stress", &secs.to_string()])
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let out = Arc::new(Mutex::new(Vec::new()));
        if let Some(o) = child.stdout.take() {
            let out = out.clone();
            std::thread::spawn(move || for l in std::io::BufReader::new(o).lines().map_while(Result::ok) { out.lock().unwrap().push(l); });
        }
        if let Some(e) = child.stderr.take() {
            let out = out.clone();
            std::thread::spawn(move || for l in std::io::BufReader::new(e).lines().map_while(Result::ok) { out.lock().unwrap().push(format!("error: {l}")); });
        }
        Ok(Self { child, started: Instant::now(), secs, out })
    }

    pub fn stop(&mut self) { let _ = self.child.kill(); }

    /// None while running; Some(Ok(())) when it finished, Some(Err(msg)) when it failed.
    pub fn finished(&mut self) -> Option<Result<(), String>> {
        let st = self.child.try_wait().ok().flatten()?;
        let lines = self.out.lock().unwrap().clone();
        if st.success() && lines.iter().any(|l| l.contains(r#""done":true"#)) { return Some(Ok(())); }
        let err = lines.iter().rev().find(|l| l.starts_with("error:")).cloned().unwrap_or_else(|| format!("stopped ({st})"));
        Some(Err(err.trim_start_matches("error:").trim().to_string()))
    }

    pub fn progress(&self) -> f32 { (self.started.elapsed().as_secs_f32() / self.secs as f32).min(1.0) }
}

/// What the eGPU did in one sample (from the page history).
#[derive(Clone, Debug, PartialEq)]
pub struct Point { pub power: Option<f32>, pub temp: Option<f32>, pub clock: Option<u32>, pub held: Vec<String> }

/// Result lines (label, text) from the samples taken during the run (pure; tested).
pub fn summary(points: &[Point], secs_done: u64, limit_w: Option<f32>, cap: Option<u32>, ok: bool) -> Vec<(String, String)> {
    let p: Vec<f32> = points.iter().filter_map(|x| x.power).collect();
    let t: Vec<f32> = points.iter().filter_map(|x| x.temp).collect();
    let c: Vec<u32> = points.iter().filter_map(|x| x.clock).collect();
    let mut v = Vec::new();
    let settings = format!("{}{}", limit_w.map_or("default power limit".into(), |l| format!("{l:.0} W")),
        cap.map_or(String::new(), |c| format!(" / {c} MHz")));
    v.push(("Run".into(), format!("{}:{:02} at {settings} ({} samples)", secs_done / 60, secs_done % 60, points.len())));
    if !p.is_empty() {
        let avg = p.iter().sum::<f32>() / p.len() as f32;
        v.push(("Power avg / peak".into(), format!("{avg:.0} / {:.0} W", p.iter().cloned().fold(0.0, f32::max))));
    }
    if !t.is_empty() { v.push(("Temperature max".into(), format!("{:.0} °C", t.iter().cloned().fold(0.0, f32::max)))); }
    if !c.is_empty() {
        let avg = c.iter().map(|&x| x as u64).sum::<u64>() / c.len() as u64;
        v.push(("Clock avg / min".into(), format!("{avg} / {} MHz", c.iter().min().unwrap_or(&0))));
    }
    if !points.is_empty() {
        let n = points.len() as f32;
        let share = |what: &str| (points.iter().filter(|x| x.held.iter().any(|h| h == what)).count() as f32 / n * 100.0).round();
        let parts: Vec<String> = ["power", "thermal", "clock ceiling"].iter()
            .map(|w| format!("{w} {}%", share(w))).collect();
        v.push(("Held back by".into(), parts.join(" · ")));
    }
    v.push(("Result".into(), if ok { "Finished, the eGPU stayed attached".into() } else { "Did not finish (see the status line)".into() }));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_what_held_the_clock_back() {
        let pt = |p: f32, t: f32, c: u32, h: &[&str]| Point { power: Some(p), temp: Some(t), clock: Some(c), held: h.iter().map(|s| s.to_string()).collect() };
        let s = summary(&[pt(170.0, 70.0, 1500, &["clock ceiling"]), pt(180.0, 72.0, 1455, &["power"]),
                          pt(178.0, 73.0, 1500, &["power", "clock ceiling"]), pt(176.0, 73.0, 1500, &[])],
                        180, Some(180.0), Some(1500), true);
        let get = |k: &str| s.iter().find(|(l, _)| l == k).map(|(_, t)| t.clone()).unwrap();
        assert_eq!(get("Power avg / peak"), "176 / 180 W");
        assert_eq!(get("Temperature max"), "73 °C");
        assert_eq!(get("Clock avg / min"), "1488 / 1455 MHz");
        assert_eq!(get("Held back by"), "power 50% · thermal 0% · clock ceiling 50%");
        assert!(get("Run").starts_with("3:00 at 180 W / 1500 MHz"));
    }
}

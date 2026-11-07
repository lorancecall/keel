//! Fault isolation for the service: each feature runs on its own thread under
//! `catch_unwind`. A feature that panics or returns an error is logged and restarted after a
//! growing pause; one that returns `Ok` while not asked to stop is restarted too (its loop is meant
//! to run until stopped). The other features never notice.
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Pause before restart number `n` (1-based): 10 s, doubling, at most 10 minutes.
pub fn backoff(n: u32) -> Duration {
    Duration::from_secs(10u64.saturating_mul(1u64 << n.saturating_sub(1).min(6)).min(600))
}

/// A run that lasted this long counts as healthy: the next failure starts the pauses over.
pub const HEALTHY: Duration = Duration::from_secs(600);

/// What the service reports about a feature.
#[derive(Default)]
pub struct Health {
    pub restarts: AtomicU32,
    pub running: AtomicBool,
    pub last_error: Mutex<Option<String>>,
}

pub struct Feature {
    pub name: &'static str,
    pub stop: Arc<AtomicBool>,
    pub health: Arc<Health>,
    thread: Option<JoinHandle<()>>,
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

/// Sleeps up to `d`, returning early (true) once `stop` is set.
pub fn sleep_unless(stop: &AtomicBool, d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(Ordering::SeqCst) { return true; }
        std::thread::sleep(Duration::from_millis(200).min(end.saturating_duration_since(Instant::now())));
    }
    stop.load(Ordering::SeqCst)
}

impl Feature {
    /// Starts `body` on its own thread; `body` gets the stop flag and must return soon after it is set.
    pub fn start(name: &'static str, log: fn(String), pause: fn(u32) -> Duration,
                 body: impl Fn(&Arc<AtomicBool>) -> anyhow::Result<()> + Send + 'static) -> Feature {
        let stop = Arc::new(AtomicBool::new(false));
        let health = Arc::new(Health::default());
        let (s, h) = (stop.clone(), health.clone());
        let thread = std::thread::Builder::new().name(name.into()).spawn(move || {
            let mut fails = 0u32;
            while !s.load(Ordering::SeqCst) {
                h.running.store(true, Ordering::SeqCst);
                let began = Instant::now();
                let r = catch_unwind(AssertUnwindSafe(|| body(&s)));
                h.running.store(false, Ordering::SeqCst);
                if s.load(Ordering::SeqCst) { break; }
                let why = match r {
                    Ok(Ok(())) => "stopped by itself".to_string(),
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(p) => format!("panicked: {}", panic_text(&*p)),
                };
                if began.elapsed() >= HEALTHY { fails = 0; }
                fails += 1;
                h.restarts.fetch_add(1, Ordering::SeqCst);
                *h.last_error.lock().unwrap() = Some(why.clone());
                let wait = pause(fails);
                log(format!("{name}: {why}; restarting in {} s", wait.as_secs()));
                if sleep_unless(&s, wait) { break; }
            }
        }).expect("spawn feature thread");
        Feature { name, stop, health, thread: Some(thread) }
    }

    /// Asks the feature to stop and waits for its thread.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(10));
        assert_eq!(backoff(2), Duration::from_secs(20));
        assert_eq!(backoff(7), Duration::from_secs(600));
        assert_eq!(backoff(40), Duration::from_secs(600));
    }

    #[test]
    fn a_panicking_feature_restarts_and_others_keep_running() {
        fn quiet(_: String) {}
        fn fast(_: u32) -> Duration { Duration::from_millis(10) }
        let crashes = Arc::new(AtomicU32::new(0));
        let c = crashes.clone();
        let bad = Feature::start("bad", quiet, fast, move |_| { c.fetch_add(1, Ordering::SeqCst); panic!("boom") });
        let ticks = Arc::new(AtomicU32::new(0));
        let t = ticks.clone();
        let good = Feature::start("good", quiet, fast, move |stop| {
            while !stop.load(Ordering::SeqCst) { t.fetch_add(1, Ordering::SeqCst); std::thread::sleep(Duration::from_millis(5)); }
            Ok(())
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(crashes.load(Ordering::SeqCst) >= 3, "restarted after each panic");
        assert!(bad.health.last_error.lock().unwrap().as_deref().unwrap().contains("boom"));
        assert!(good.health.running.load(Ordering::SeqCst));
        assert_eq!(good.health.restarts.load(Ordering::SeqCst), 0);
        let before = ticks.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert!(ticks.load(Ordering::SeqCst) > before, "the other feature keeps working");
        bad.stop();
        good.stop();
    }
}

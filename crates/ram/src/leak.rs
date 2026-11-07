//! Leak heuristic over one process metric's samples (pure). A leak is sustained growth of the
//! *floor*: garbage-collected / cache-like memory saws up and down but its lows stay put, while a
//! leak's lows climb. Verdicts need enough span and points so a single snapshot never decides.

/// Thresholds per metric kind.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub min_span_h: f64,
    pub min_points: usize,
    /// Minimum rise of the floor (lowest quarter start -> lowest quarter end), in the metric's unit.
    pub min_growth: f64,
}

pub const MB: f64 = 1048576.0;
/// Private bytes / working set: at least 64 MB of floor growth over 2 h and 12 samples.
pub const MEMORY: Rule = Rule { min_span_h: 2.0, min_points: 12, min_growth: 64.0 * MB };
/// Handle count.
pub const HANDLES: Rule = Rule { min_span_h: 2.0, min_points: 12, min_growth: 500.0 };
/// GPU dedicated / shared memory per adapter.
pub const GPU: Rule = Rule { min_span_h: 2.0, min_points: 12, min_growth: 64.0 * MB };

/// Kernel pool tag (driver memory, always resident when nonpaged): 32 MB of floor growth.
pub const POOL: Rule = Rule { min_span_h: 2.0, min_points: 12, min_growth: 32.0 * MB };

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub enum Level { Leak, Suspect }

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Verdict {
    pub level: Level,
    /// Least-squares slope, units per hour.
    pub per_hour: f64,
    /// Rise of the floor over the window.
    pub floor_growth: f64,
    pub r2: f64,
    /// Share of intervals in which the value went up.
    pub rising: f64,
    pub span_h: f64,
}

fn regression(t: &[f64], v: &[f64]) -> (f64, f64) {
    let n = t.len() as f64;
    let (mt, mv) = (t.iter().sum::<f64>() / n, v.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in t.iter().zip(v) {
        sxy += (x - mt) * (y - mv);
        sxx += (x - mt) * (x - mt);
        syy += (y - mv) * (y - mv);
    }
    if sxx == 0.0 { return (0.0, 0.0); }
    let slope = sxy / sxx;
    let r2 = if syy == 0.0 { 0.0 } else { (sxy * sxy) / (sxx * syy) };
    (slope, r2)
}

/// Judges one series (`t` in hours, ascending; `v` in the metric's unit).
pub fn judge(t: &[f64], v: &[f64], rule: &Rule) -> Option<Verdict> {
    let n = t.len().min(v.len());
    if n < rule.min_points.max(4) { return None; }
    let span_h = t[n - 1] - t[0];
    if span_h < rule.min_span_h { return None; }
    let q = (n / 4).max(1);
    let floor = |s: &[f64]| s.iter().cloned().fold(f64::INFINITY, f64::min);
    let floor_growth = floor(&v[n - q..n]) - floor(&v[..q]);
    if floor_growth < rule.min_growth { return None; }
    let (per_hour, r2) = regression(&t[..n], &v[..n]);
    if per_hour <= 0.0 { return None; }
    let rising = v[..n].windows(2).filter(|w| w[1] > w[0]).count() as f64 / (n - 1) as f64;
    let level = if r2 >= 0.8 || rising >= 0.8 { Level::Leak } else if r2 >= 0.5 || rising >= 0.6 { Level::Suspect } else { return None };
    Some(Verdict { level, per_hour, floor_growth, r2, rising, span_h })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hours(n: usize) -> Vec<f64> { (0..n).map(|i| i as f64 / 12.0).collect() } // 5-min samples

    #[test]
    fn steady_growth_is_a_leak() {
        let t = hours(72); // 6 h
        let v: Vec<f64> = t.iter().map(|h| 500.0 * MB + h * 40.0 * MB).collect();
        let r = judge(&t, &v, &MEMORY).unwrap();
        assert_eq!(r.level, Level::Leak);
        assert!((r.per_hour / MB - 40.0).abs() < 0.5);
    }

    #[test]
    fn sawtooth_without_rising_floor_is_not() {
        let t = hours(72);
        let v: Vec<f64> = (0..72).map(|i| 300.0 * MB + (i % 12) as f64 * 30.0 * MB).collect(); // GC every hour
        assert!(judge(&t, &v, &MEMORY).is_none());
    }

    #[test]
    fn sawtooth_on_a_rising_floor_is_caught() {
        let t = hours(72);
        let v: Vec<f64> = (0..72).map(|i| 300.0 * MB + i as f64 * 4.0 * MB + (i % 6) as f64 * 20.0 * MB).collect();
        assert!(judge(&t, &v, &MEMORY).is_some());
    }

    #[test]
    fn noise_short_spans_and_single_steps() {
        let t = hours(72);
        let noisy: Vec<f64> = (0..72).map(|i| 800.0 * MB + ((i * 37) % 11) as f64 * MB).collect();
        assert!(judge(&t, &noisy, &MEMORY).is_none());
        let short = hours(20); // 1.6 h
        let grow: Vec<f64> = short.iter().map(|h| h * 500.0 * MB).collect();
        assert!(judge(&short, &grow, &MEMORY).is_none(), "under the minimum span");
        let step: Vec<f64> = (0..72).map(|i| if i < 36 { 400.0 * MB } else { 900.0 * MB }).collect();
        assert_ne!(judge(&t, &step, &MEMORY).map(|r| r.level), Some(Level::Leak), "one jump is not a leak");
    }

    #[test]
    fn handle_leak_uses_its_own_threshold() {
        let t = hours(48);
        let v: Vec<f64> = (0..48).map(|i| 2000.0 + i as f64 * 15.0).collect(); // +705 in 4 h
        assert_eq!(judge(&t, &v, &HANDLES).unwrap().level, Level::Leak);
        let small: Vec<f64> = (0..48).map(|i| 2000.0 + i as f64 * 5.0).collect(); // +235
        assert!(judge(&t, &small, &HANDLES).is_none());
    }
}

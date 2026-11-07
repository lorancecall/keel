//! Another program changing the same settings: instead of knowing every such program by name,
//! the service notices when it has to apply the same change again and again. A change applied
//! `TIMES` times within `WINDOW` is contested: it is held (not applied) for `HOLD` and reported,
//! so two programs stop fighting over it. Kill-switch rules are never held (fail-closed): they are
//! re-applied every time and only reported.
use crate::model::Action;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const WINDOW: Duration = Duration::from_secs(600);
pub const TIMES: usize = 3;
pub const HOLD: Duration = Duration::from_secs(1800);

#[derive(Default)]
pub struct Contest {
    applied: HashMap<String, Vec<Instant>>,
    held: HashMap<String, Instant>,
    /// Kill-switch rules that keep being undone (still applied): key -> last time.
    fought: HashMap<String, Instant>,
}

fn holdable(a: &Action) -> bool { !matches!(a, Action::BlockRule { .. }) }

impl Contest {
    /// The actions to run now; held ones are left out.
    pub fn filter(&mut self, actions: &[Action], now: Instant) -> Vec<Action> {
        self.held.retain(|_, t| now.duration_since(*t) < HOLD);
        actions.iter().filter(|a| !self.held.contains_key(&a.to_string())).cloned().collect()
    }

    /// After `a` was applied. Returns a line to log when it just became contested.
    pub fn applied(&mut self, a: &Action, now: Instant) -> Option<String> {
        let key = a.to_string();
        let v = self.applied.entry(key.clone()).or_default();
        v.retain(|t| now.duration_since(*t) < WINDOW);
        v.push(now);
        if v.len() < TIMES { return None; }
        v.clear();
        if holdable(a) {
            self.held.insert(key.clone(), now);
            Some(format!("another program keeps undoing \"{key}\": not applied for {} min", HOLD.as_secs() / 60))
        } else {
            self.fought.insert(key.clone(), now);
            Some(format!("another program keeps undoing the kill switch \"{key}\": still re-applied"))
        }
    }

    /// Lines for the status / main window.
    pub fn problems(&mut self, now: Instant) -> Vec<String> {
        self.held.retain(|_, t| now.duration_since(*t) < HOLD);
        self.fought.retain(|_, t| now.duration_since(*t) < HOLD);
        let mut out: Vec<String> = self.held.keys().map(|k| format!("another program keeps changing this; Keel stepped back: {k}")).collect();
        out.extend(self.fought.keys().map(|k| format!("another program keeps undoing this kill switch; Keel re-applies it: {k}")));
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Action;

    fn metric() -> Action { Action::SetMetric { index: 7, alias: "Wi-Fi".into(), metric: 1 } }

    #[test]
    fn third_reapply_in_ten_minutes_holds_the_change() {
        let t0 = Instant::now();
        let s = |m: u64| t0 + Duration::from_secs(m * 60);
        let mut c = Contest::default();
        assert!(c.applied(&metric(), s(0)).is_none());
        assert!(c.applied(&metric(), s(4)).is_none());
        assert!(c.applied(&metric(), s(8)).is_some());
        assert!(c.filter(&[metric()], s(9)).is_empty());
        assert_eq!(c.problems(s(9)).len(), 1);
        // The hold ends after 30 minutes.
        assert_eq!(c.filter(&[metric()], s(39)).len(), 1);
    }

    #[test]
    fn spread_out_reapplies_are_not_a_fight() {
        let t0 = Instant::now();
        let mut c = Contest::default();
        for m in [0u64, 11, 22, 33] { assert!(c.applied(&metric(), t0 + Duration::from_secs(m * 60)).is_none()); }
    }

    #[test]
    fn kill_switch_is_never_held() {
        let t0 = Instant::now();
        let rule = Action::BlockRule { name: "Keel net: Tailscale out".into(), program: "t.exe".into(), aliases: vec!["Ethernet".into()], outbound: true };
        let mut c = Contest::default();
        for m in 0..3u64 { let _ = c.applied(&rule, t0 + Duration::from_secs(m)); }
        assert_eq!(c.filter(&[rule.clone()], t0 + Duration::from_secs(5)).len(), 1);
        assert_eq!(c.problems(t0 + Duration::from_secs(5)).len(), 1);
    }
}

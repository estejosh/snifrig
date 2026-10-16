//! Which app feels slow, and why. Two signs, both cheap:
//! - its window answers late (or not at all) to a tiny "are you there" message, measured against
//!   that app's own usual answer time;
//! - it is using every bit of the CPU it is allowed (all its cores), which is how a game crawls
//!   while its window still answers.
//! The reason names what to do about it: a rule holding it to too few cores, snifrig easing it,
//! the desktop compositor, another app hogging the PC, or its own work.
use std::collections::{HashMap, HashSet};

/// One app with a window on a screen, as measured now (summed over its processes).
#[derive(Clone, Debug, Default)]
pub struct AppNow { pub name: String, pub ms: f64, pub hung: bool, pub pct: f64, pub cores: u32 }

/// What else is going on, for the reason.
#[derive(Clone, Debug, Default)]
pub struct Ctx { pub total_cores: u32, pub dwm_pct: f64, pub total_pct: f64, pub eased: Option<u8>, pub rule_capped: bool, pub hog: Option<String> }

pub fn slow_ping(a: &AppNow, base_ms: f64) -> bool { a.hung || a.ms >= (base_ms * 5.0).max(150.0) }
pub fn saturated(a: &AppNow) -> bool { a.pct >= 85.0 * a.cores.max(1) as f64 }

/// Some(reason) when the app is slow now.
pub fn verdict(a: &AppNow, base_ms: f64, c: &Ctx) -> Option<String> {
    let (ping, sat) = (slow_ping(a, base_ms), saturated(a));
    if !ping && !sat { return None; }
    let s = |n: u32| if n == 1 { "" } else { "s" };
    Some(if a.hung { "it is not responding".into() }
    else if sat && a.cores < c.total_cores {
        format!("it is limited to {} core{}{} and is using all of {}", a.cores, s(a.cores), if c.rule_capped { " by your rule" } else { "" }, if a.cores == 1 { "it" } else { "them" })
    } else if let Some(l) = c.eased { format!("snifrig has it eased ({})", match l { 1 => "efficiency mode", 2 => "below normal", _ => "idle" }) }
    else if c.dwm_pct >= 50.0 { "the desktop compositor is overloaded, so every window lags".into() }
    else if c.total_pct >= 80.0 { match &c.hog { Some(h) => format!("the PC is busy, mostly with {}", h), None => "the PC is busy".into() } }
    else if sat { format!("it is busy with its own work ({:.0}% of a core)", a.pct) }
    else { format!("its window answers slowly ({:.0} ms, usually {:.0} ms)", a.ms, base_ms) })
}

/// Keeps each app's usual answer time and decides when an app starts and stops feeling slow
/// (two checks in a row, so one hiccup is not news).
#[derive(Default)]
pub struct Tracker {
    base: HashMap<String, f64>,
    streak: HashMap<String, u32>,
    announced: HashSet<String>,
    /// Apps slow right now: (name, ms, usual ms, reason).
    pub slow: Vec<(String, f64, f64, String)>,
}

impl Tracker {
    /// Feed one round of measurements. Returns (name, reason) for apps that just became slow.
    pub fn update(&mut self, apps: &[(AppNow, Ctx)]) -> Vec<(String, String)> {
        let mut now = Vec::new(); let mut fresh = Vec::new(); let mut seen = HashSet::new();
        for (a, c) in apps {
            let n = a.name.to_lowercase(); seen.insert(n.clone());
            let base = *self.base.get(&n).unwrap_or(&a.ms.max(5.0));
            let v = verdict(a, base, c);
            if !slow_ping(a, base) && !a.hung { self.base.insert(n.clone(), (base * 0.9 + a.ms * 0.1).max(5.0)); }
            let st = self.streak.entry(n.clone()).or_insert(0);
            match v {
                Some(why) => {
                    *st += 1;
                    if *st >= 2 {
                        now.push((a.name.clone(), a.ms, base, why.clone()));
                        if self.announced.insert(n.clone()) { fresh.push((a.name.clone(), why)); }
                    }
                }
                None => { *st = 0; self.announced.remove(&n); }
            }
        }
        self.streak.retain(|k, _| seen.contains(k));
        self.announced.retain(|k| seen.contains(k));
        self.slow = now;
        fresh
    }
}

#[cfg(test)]
mod slowapp_tests {
    use super::*;
    fn app(ms: f64, pct: f64, cores: u32) -> AppNow { AppNow { name: "legendsofidleon.exe".into(), ms, hung: false, pct, cores } }
    #[test]
    fn crawling_on_one_core_is_named() {
        let c = Ctx { total_cores: 16, rule_capped: true, ..Default::default() };
        assert_eq!(verdict(&app(8.0, 98.0, 1), 8.0, &c).unwrap(), "it is limited to 1 core by your rule and is using all of it");
        assert!(verdict(&app(8.0, 60.0, 1), 8.0, &c).is_none(), "room left on its core, answers fast: fine");
        assert!(verdict(&app(8.0, 300.0, 16), 8.0, &Ctx { total_cores: 16, ..Default::default() }).is_none(), "busy but far from all 16 cores");
    }
    #[test]
    fn slow_window_against_its_own_usual_time() {
        let c = Ctx { total_cores: 16, total_pct: 90.0, hog: Some("python.exe".into()), ..Default::default() };
        assert_eq!(verdict(&app(400.0, 5.0, 16), 10.0, &c).unwrap(), "the PC is busy, mostly with python.exe");
        assert!(verdict(&app(40.0, 5.0, 16), 10.0, &c).is_none(), "40 ms is under the 150 ms floor");
    }
    #[test]
    fn needs_two_rounds_and_announces_once() {
        let mut t = Tracker::default();
        let c = Ctx { total_cores: 16, rule_capped: true, ..Default::default() };
        let r = [(app(8.0, 98.0, 1), c.clone())];
        assert!(t.update(&r).is_empty());
        assert_eq!(t.update(&r).len(), 1);
        assert!(t.update(&r).is_empty(), "already announced");
        assert_eq!(t.slow.len(), 1);
        t.update(&[(app(8.0, 20.0, 1), c.clone())]);
        assert!(t.slow.is_empty());
    }
}

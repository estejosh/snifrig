//! CPU-hog detector. Reuses the per-process CPU deltas the monitor already computes each cycle (no new system calls).
//! A process is a hog when it averages >= 80% of one logical core over >= 10 minutes; a name group
//! (all processes with the same image name) when it averages >= 150% of a core over the same time.
use std::collections::{HashMap, VecDeque};

pub const PROC_PCT: f64 = 80.0;
pub const GROUP_PCT: f64 = 150.0;
const NEED_SECS: f64 = 570.0; // observed time required (10 min, minus cycle jitter)
const KEEP_SECS: f64 = 660.0;
const MERGE_SECS: f64 = 30.0; // fast intervals are merged so a window stays small
const REALERT: f64 = 2.0 * 3600.0;
const AUTO_RESTART: [&str; 4] = ["startmenuexperiencehost.exe", "searchhost.exe", "shellexperiencehost.exe", "textinputhost.exe"];

/// One process this cycle: pid, image name, creation FILETIME, CPU seconds used since the last cycle, CPU seconds since it started.
pub struct Sample { pub pid: u32, pub name: String, pub created: u64, pub delta_s: f64, pub total_s: f64 }

#[derive(Debug, Clone, PartialEq)]
pub struct Hog { pub key: String, pub name: String, pub pid: u32, pub pct: f64, pub minutes: f64, pub msg: String }

#[derive(Default)]
pub struct CpuHogs {
    procs: HashMap<(u32, u64), VecDeque<(f64, f64, f64)>>, // (t, seconds covered, cpu seconds)
    groups: HashMap<String, VecDeque<(f64, f64, f64)>>,
    last: HashMap<String, f64>,
    /// Every hog right now, whether or not it was alerted this cycle (feeds the verdict).
    pub hot: Vec<Hog>,
}

pub fn auto_restarts(name: &str) -> bool { AUTO_RESTART.contains(&name.to_ascii_lowercase().as_str()) }

pub fn friendly(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "startmenuexperiencehost.exe" => "the Windows Start menu".into(),
        "searchhost.exe" => "Windows Search".into(),
        "shellexperiencehost.exe" => "the Windows taskbar/notifications host".into(),
        "comet.exe" => "Comet browser".into(),
        _ => name.to_string(),
    }
}

fn push(q: &mut VecDeque<(f64, f64, f64)>, t: f64, dt: f64, d: f64) {
    match q.back_mut() {
        Some(b) if b.1 < MERGE_SECS => { b.0 = t; b.1 += dt; b.2 += d; }
        _ => q.push_back((t, dt, d)),
    }
    while q.front().map_or(false, |x| t - x.0 > KEEP_SECS) { q.pop_front(); }
}

/// (average % of one core, minutes observed) when the window is long enough.
fn avg(q: &VecDeque<(f64, f64, f64)>) -> Option<(f64, f64)> {
    let (dt, d) = q.iter().fold((0.0, 0.0), |a, x| (a.0 + x.1, a.1 + x.2));
    if dt >= NEED_SECS { Some((d / dt * 100.0, dt / 60.0)) } else { None }
}

pub fn message(name: &str, count: usize, pct: f64, minutes: f64, total_s: f64, started: &str) -> String {
    let who = if count > 1 { format!("{} ({} processes)", friendly(name), count) } else { friendly(name) };
    let mut m = format!("{} has used about {:.0}% of a CPU core for the last {:.0} minutes ({:.1} hours of CPU since it started at {}).",
        who, pct, minutes, total_s / 3600.0, started);
    if auto_restarts(name) { m.push_str(" Windows restarts it automatically, so ending it is safe."); }
    m
}

impl CpuHogs {
    pub fn new() -> Self { Self::default() }

    /// `dt` is seconds since the previous cycle. `started` turns a creation FILETIME into HH:MM.
    pub fn update(&mut self, t: f64, dt: f64, cur: &[Sample], started: &dyn Fn(u64) -> String) -> Vec<Hog> {
        let mut by_name: HashMap<String, (f64, f64, usize, u32, f64, u64)> = HashMap::new(); // delta, total, n, top pid, top delta, oldest created
        for s in cur {
            push(self.procs.entry((s.pid, s.created)).or_default(), t, dt, s.delta_s);
            let g = by_name.entry(s.name.to_ascii_lowercase()).or_insert((0.0, 0.0, 0, s.pid, -1.0, s.created));
            g.0 += s.delta_s; g.1 += s.total_s; g.2 += 1;
            if s.delta_s > g.4 { g.3 = s.pid; g.4 = s.delta_s; }
            if s.created != 0 && (g.5 == 0 || s.created < g.5) { g.5 = s.created; }
        }
        for (n, g) in &by_name { push(self.groups.entry(n.clone()).or_default(), t, dt, g.0); }
        self.procs.retain(|_, q| q.back().map_or(false, |x| t - x.0 <= KEEP_SECS));
        self.groups.retain(|_, q| q.back().map_or(false, |x| t - x.0 <= KEEP_SECS));
        self.last.retain(|_, l| t - *l < REALERT);

        let mut out = Vec::new();
        self.hot.clear();
        for s in cur {
            let lname = s.name.to_ascii_lowercase();
            let Some((pct, mins)) = self.procs.get(&(s.pid, s.created)).and_then(avg) else { continue };
            if pct < PROC_PCT { continue; }
            let hog = Hog { key: format!("cpu:{}#{}", s.name, s.pid), name: s.name.clone(), pid: s.pid, pct, minutes: mins,
                msg: message(&s.name, 1, pct, mins, s.total_s, &started(s.created)) };
            self.hot.push(hog.clone());
            let k = format!("p:{}:{}", s.pid, s.created);
            if self.last.contains_key(&k) { continue; }
            self.last.insert(k, t);
            self.last.insert(format!("g:{}", lname), t);
            out.push(hog);
        }
        for (n, g) in &by_name {
            if g.2 < 2 { continue; }
            let Some((pct, mins)) = self.groups.get(n).and_then(avg) else { continue };
            if pct < GROUP_PCT { continue; }
            let name = cur.iter().find(|s| s.pid == g.3).map(|s| s.name.clone()).unwrap_or_else(|| n.clone());
            if self.hot.iter().any(|h| h.name.eq_ignore_ascii_case(&name)) { continue; }
            let hog = Hog { key: format!("cpu:{}#{}", name, g.3), name: name.clone(), pid: g.3, pct, minutes: mins,
                msg: message(&name, g.2, pct, mins, g.1, &started(g.5)) };
            self.hot.push(hog.clone());
            let k = format!("g:{}", n);
            if self.last.contains_key(&k) { continue; }
            self.last.insert(k, t);
            out.push(hog);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(pid: u32, name: &str, delta: f64) -> Sample { Sample { pid, name: name.into(), created: 100, delta_s: delta, total_s: 6466.0 } }
    fn st(_: u64) -> String { "09:05".into() }

    fn run(h: &mut CpuHogs, minutes: usize, f: impl Fn() -> Vec<Sample>) -> Vec<Hog> {
        let mut last = Vec::new();
        for i in 1..=minutes {
            let o = h.update(i as f64 * 60.0, 60.0, &f(), &st);
            if !o.is_empty() { last = o; }
        }
        last
    }

    #[test]
    fn full_core_for_ten_minutes_alerts() {
        let mut h = CpuHogs::new();
        let o = run(&mut h, 11, || vec![s(5, "StartMenuExperienceHost.exe", 59.0), s(6, "idle.exe", 0.1)]);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].key, "cpu:StartMenuExperienceHost.exe#5");
        assert!(o[0].msg.starts_with("the Windows Start menu has used about 98% of a CPU core for the last 10 minutes (1.8 hours of CPU since it started at 09:05)."), "{}", o[0].msg);
        assert!(o[0].msg.ends_with("Windows restarts it automatically, so ending it is safe."));
    }

    #[test]
    fn short_burst_or_under_80_does_not_alert() {
        let mut h = CpuHogs::new();
        assert!(run(&mut h, 8, || vec![s(5, "a.exe", 59.0)]).is_empty());
        let mut h = CpuHogs::new();
        assert!(run(&mut h, 15, || vec![s(5, "a.exe", 40.0)]).is_empty());
    }

    #[test]
    fn group_of_many_processes_alerts_and_realerts_after_two_hours() {
        let mut h = CpuHogs::new();
        let f = || vec![s(1, "comet.exe", 30.0), s(2, "comet.exe", 30.0), s(3, "comet.exe", 30.0), s(4, "comet.exe", 5.0)];
        let o = run(&mut h, 11, f);
        assert_eq!(o.len(), 1);
        assert!(o[0].msg.starts_with("Comet browser (4 processes) has used about 158%"), "{}", o[0].msg);
        assert!(!o[0].msg.contains("restarts it automatically"));
        // quiet for the next hour: no repeat; after two hours it repeats
        let mut later = Vec::new();
        for i in 12..=135 {
            let o = h.update(i as f64 * 60.0, 60.0, &f(), &st);
            if !o.is_empty() { later.push(i); }
        }
        assert_eq!(later.len(), 1);
        assert!(later[0] >= 10 + 120);
    }

    #[test]
    fn pid_reuse_with_new_creation_time_starts_fresh() {
        let mut h = CpuHogs::new();
        run(&mut h, 9, || vec![s(5, "a.exe", 59.0)]);
        let mut n = s(5, "a.exe", 59.0);
        n.created = 200;
        assert!(h.update(600.0, 60.0, &[n], &st).is_empty());
    }
}

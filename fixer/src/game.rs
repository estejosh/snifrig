//! The game sniffer: a small recorder that runs while you play. Once a second it notes the
//! game's CPU, the whole PC's CPU, free memory, and the busiest other program, and marks moments
//! that likely stuttered (the game suddenly got much less CPU while something else was busy, the
//! PC was maxed out, or memory ran short), with who was busy at the time. When a full-screen game
//! ends, snifrig asks how it ran; if you say it stuttered, the programs behind those moments are
//! turned down harder in game mode next time (game-ease.txt).

use std::collections::{HashMap, VecDeque};
use std::path::Path;

pub const EASE_FILE: &str = "game-ease.txt";
pub const SESSIONS: &str = "game-sessions.jsonl";
pub const LAST: &str = "game-last.txt";
pub const MIN_SECS: f64 = 300.0;

#[derive(Clone, Debug, Default)]
pub struct Sample { pub t: f64, pub game_pct: f64, pub total: f64, pub top: String, pub top_pct: f64, pub avail_mb: f64, pub fullscreen: bool }

#[derive(Debug)]
pub struct Session {
    pub game: String,
    pub start: f64,
    pub secs: f64,
    pub fs_secs: f64,
    recent: VecDeque<f64>,
    pub hitches: Vec<(f64, String, &'static str)>,
    last_hitch: f64,
    last_t: f64,
}

impl Session {
    pub fn new(game: &str, now: f64) -> Session {
        Session { game: game.to_string(), start: now, secs: 0.0, fs_secs: 0.0, recent: VecDeque::new(), hitches: Vec::new(), last_hitch: 0.0, last_t: now }
    }

    /// Pure: feed one sample (about once a second).
    pub fn add(&mut self, s: &Sample) {
        let dt = (s.t - self.last_t).clamp(0.0, 5.0);
        self.last_t = s.t;
        self.secs += dt;
        if s.fullscreen { self.fs_secs += dt; }
        let mut v: Vec<f64> = self.recent.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let usual = if v.len() >= 10 { v[v.len() / 2] } else { 0.0 };
        let starved = usual >= 20.0 && s.game_pct < usual * 0.5 && (s.total >= 85.0 || s.top_pct >= 50.0);
        let why = if starved { Some("the game suddenly got less CPU") }
            else if s.total >= 97.0 { Some("the PC was maxed out") }
            else if s.avail_mb > 0.0 && s.avail_mb < 1500.0 { Some("memory ran short") }
            else { None };
        if let Some(w) = why {
            if s.t - self.last_hitch >= 3.0 {
                self.last_hitch = s.t;
                self.hitches.push((s.t, if s.top_pct >= 25.0 { s.top.clone() } else { String::new() }, w));
            }
        }
        self.recent.push_back(s.game_pct);
        while self.recent.len() > 30 { self.recent.pop_front(); }
    }

    /// Mostly played full screen, and long enough to ask about.
    pub fn worth_asking(&self) -> bool { self.secs >= MIN_SECS && self.fs_secs * 2.0 >= self.secs }

    /// Programs busy during the likely stutters, most often first.
    pub fn culprits(&self) -> Vec<(String, usize)> {
        let mut m: HashMap<&str, usize> = HashMap::new();
        for (_, c, _) in &self.hitches { if !c.is_empty() { *m.entry(c.as_str()).or_default() += 1; } }
        let mut v: Vec<(String, usize)> = m.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    pub fn question(&self) -> String {
        let mins = (self.secs / 60.0).round();
        let c = self.culprits();
        let seen = match (self.hitches.len(), c.first()) {
            (0, _) => "snifrig saw no moments that looked like a stutter.".to_string(),
            (n, Some((who, k))) => format!("snifrig saw {} moment{} that may have stuttered, {} of them while {} was busy.", n, if n == 1 { "" } else { "s" }, k, who),
            (n, None) => format!("snifrig saw {} moment{} that may have stuttered.", n, if n == 1 { "" } else { "s" }),
        };
        format!("How did {} run? You played {:.0} minutes. {}", self.game, mins, seen)
    }
}

/// Programs that get turned down harder while you play (learned from your answers).
pub fn load_ease(dir: &Path) -> std::collections::HashSet<String> {
    std::fs::read_to_string(dir.join(EASE_FILE)).unwrap_or_default().lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(t: f64, game: f64, total: f64, top: &str, top_pct: f64) -> Sample { Sample { t, game_pct: game, total, top: top.into(), top_pct, avail_mb: 8000.0, fullscreen: true } }

    #[test]
    fn spots_stutters_and_who_was_busy() {
        let mut x = Session::new("kingdomtwocrowns.exe", 0.0);
        for i in 1..=600 { x.add(&s(i as f64, 300.0, 60.0, "claude.exe", 20.0)); }
        assert!(x.hitches.is_empty(), "a steady game is not a stutter");
        x.add(&s(601.0, 90.0, 92.0, "syncthing.exe", 180.0));
        x.add(&s(602.0, 280.0, 60.0, "claude.exe", 20.0));
        x.add(&s(606.0, 80.0, 95.0, "syncthing.exe", 200.0));
        x.add(&s(607.0, 300.0, 98.0, "msmpeng.exe", 30.0));
        assert_eq!(x.hitches.len(), 2, "{:?}", x.hitches);
        assert_eq!(x.culprits()[0], ("syncthing.exe".to_string(), 2));
        assert!(x.worth_asking());
        assert!(x.question().contains("2 moments that may have stuttered, 2 of them while syncthing.exe was busy"), "{}", x.question());
    }

    #[test]
    fn short_or_windowed_play_is_not_asked_about() {
        let mut x = Session::new("idleon.exe", 0.0);
        for i in 1..=900 { let mut a = s(i as f64, 50.0, 40.0, "", 0.0); a.fullscreen = false; x.add(&a); }
        assert!(!x.worth_asking());
        let mut y = Session::new("game.exe", 0.0);
        for i in 1..=60 { y.add(&s(i as f64, 50.0, 40.0, "", 0.0)); }
        assert!(!y.worth_asking());
    }
}

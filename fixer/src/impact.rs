//! Measured impact: "without snifrig, your apps would respond X% slower when the PC is busy",
//! measured on this PC, not guessed.
//!
//! How: while the PC is busy (and you are not in a game, not asking for help, not in a quick
//! fix), snifrig runs a short test: two 3-minute blocks back to back, one where it holds off
//! (nothing eased, no new rules applied) and one where it works as usual, in alternating order.
//! In both it measures how fast the PC responds (the same feel meter snifrig always uses:
//! window response, message lag, your own input). A test counts only if the load was similar
//! in both blocks. At most one test every 3 hours, 4 a day; you can turn tests off.
//! Every test is kept in impact.jsonl, so the number can be checked line by line.
//! The comparison is conservative: rules already applied stay in place during the off block.

use std::path::Path;

pub const BLOCK_SECS: f64 = 180.0;
pub const MIN_GAP_SECS: f64 = 3.0 * 3600.0;
pub const MAX_PER_DAY: usize = 4;
pub const BUSY_PCT: f64 = 60.0;
pub const MAX_CPU_DIFF: f64 = 15.0;
pub const MIN_SAMPLES: usize = 20;
pub const LOG: &str = "impact.jsonl";
pub const SUMMARY: &str = "impact-summary.json";
pub const OFF_FILE: &str = "impact-off";
pub const NOW_FILE: &str = "impact-test-now";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block { pub ms: f64, pub p90: f64, pub cpu: f64, pub n: usize }

fn block(ms: &[f64], cpu: &[f64]) -> Block {
    let mut v = ms.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| if v.is_empty() { 0.0 } else { v[((v.len() - 1) as f64 * p).round() as usize] };
    Block { ms: q(0.5), p90: q(0.9), cpu: if cpu.is_empty() { 0.0 } else { cpu.iter().sum::<f64>() / cpu.len() as f64 }, n: v.len() }
}

/// Pure: is this pair of blocks a fair comparison?
pub fn valid(off: &Block, on: &Block) -> bool {
    off.n >= MIN_SAMPLES && on.n >= MIN_SAMPLES && (off.cpu - on.cpu).abs() <= MAX_CPU_DIFF && on.ms > 0.0
}

#[derive(Clone, Debug, PartialEq)]
pub struct Summary { pub tests: usize, pub valid: usize, pub slower_pct: f64, pub low: f64, pub high: f64, pub faster_pct: f64 }

/// Pure: from (off ms, on ms) of the valid tests: the median "slower without snifrig" and the
/// middle 60% range (20th to 80th percentile of the tests).
pub fn summarize(pairs: &[(f64, f64)], tests: usize) -> Summary {
    let mut r: Vec<f64> = pairs.iter().filter(|(_, on)| *on > 0.0).map(|(off, on)| (off / on - 1.0) * 100.0).collect();
    r.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| if r.is_empty() { 0.0 } else { r[((r.len() - 1) as f64 * p).round() as usize] };
    let slower = q(0.5);
    Summary { tests, valid: r.len(), slower_pct: slower, low: q(0.2), high: q(0.8), faster_pct: if slower > -100.0 { slower / (100.0 + slower) * 100.0 } else { 0.0 } }
}

/// Pure: the sentence people see.
pub fn sentence(s: &Summary) -> String {
    if s.valid < 3 {
        return format!("Measuring: {} of 3 tests done. snifrig tests only while your PC is busy, a few minutes at a time.", s.valid);
    }
    if s.low <= 0.0 {
        return format!("No clear difference measured yet ({} tests). snifrig keeps testing while your PC is busy.", s.valid);
    }
    format!("Measured on this PC: when it is busy, your apps respond {:.0}% slower without snifrig ({} tests, usually {:.0}-{:.0}%).", s.slower_pct, s.valid, s.low, s.high)
}

#[derive(Debug, Default)]
enum Phase { #[default] Idle, First { off: bool, since: f64 }, Second { off: bool, since: f64 } }

/// What the PC is doing, as a test needs it.
pub struct Now { pub now: f64, pub cpu: f64, pub feel_ms: Option<f64>, pub gaming: bool, pub user_wants_help: bool }

#[derive(Default)]
pub struct Impact {
    phase: Phase,
    ms: Vec<f64>,
    cpu: Vec<f64>,
    first: Option<(bool, Block)>,
    started: f64,
    last_test: f64,
    today: Vec<f64>,
    runs: u64,
}

impl Impact {
    pub fn new(dir: &Path) -> Impact {
        let mut i = Impact::default();
        for l in std::fs::read_to_string(dir.join(LOG)).unwrap_or_default().lines() {
            if let Some(t) = crate::json::num_field(l, "start") { i.last_test = i.last_test.max(t); i.today.push(t); i.runs += 1; }
        }
        i
    }

    /// True while snifrig should hold off (the "without snifrig" block of a test).
    pub fn holding(&self) -> bool {
        matches!(self.phase, Phase::First { off: true, .. } | Phase::Second { off: true, .. })
    }

    pub fn testing(&self) -> bool { !matches!(self.phase, Phase::Idle) }

    /// Call once a second. Returns a note when a test starts, stops or finishes.
    pub fn tick(&mut self, dir: &Path, n: &Now) -> Option<String> {
        self.today.retain(|t| n.now - t < 86400.0);
        let interrupted = n.gaming || n.user_wants_help;
        match self.phase {
            Phase::Idle => {
                let asked = dir.join(NOW_FILE).exists();
                if asked { let _ = std::fs::remove_file(dir.join(NOW_FILE)); }
                let allowed = !dir.join(OFF_FILE).exists() && !interrupted;
                let due = n.now - self.last_test >= MIN_GAP_SECS && self.today.len() < MAX_PER_DAY;
                if allowed && n.cpu >= if asked { 40.0 } else { BUSY_PCT } && (due || asked) {
                    // Alternate which block comes first, so load drifting one way does not favour either.
                    let off = self.runs % 2 == 0;
                    self.phase = Phase::First { off, since: n.now };
                    self.started = n.now;
                    self.ms.clear(); self.cpu.clear(); self.first = None;
                    return Some("impact test started: 3 minutes with snifrig holding off and 3 with it working, to measure what it does for this PC".into());
                }
                if asked { return Some(format!("impact test not started: the PC is calm ({:.0}% CPU); snifrig tests while it is busy", n.cpu)); }
                None
            }
            Phase::First { off, since } | Phase::Second { off, since } => {
                if interrupted {
                    self.phase = Phase::Idle;
                    self.last_test = n.now - MIN_GAP_SECS + 1800.0; // try again in half an hour
                    return Some("impact test stopped: you needed the PC".into());
                }
                if let Some(ms) = n.feel_ms { self.ms.push(ms); }
                self.cpu.push(n.cpu);
                if n.now - since < BLOCK_SECS { return None; }
                let b = block(&self.ms, &self.cpu);
                self.ms.clear(); self.cpu.clear();
                if matches!(self.phase, Phase::First { .. }) {
                    self.first = Some((off, b));
                    self.phase = Phase::Second { off: !off, since: n.now };
                    return None;
                }
                self.phase = Phase::Idle;
                self.last_test = n.now;
                self.today.push(self.started);
                self.runs += 1;
                let (first_off, fb) = self.first.take().unwrap_or((true, Block::default()));
                let (offb, onb) = if first_off { (fb, b) } else { (b, fb) };
                let ok = valid(&offb, &onb);
                let line = format!("{{\"start\":{:.0},\"off_first\":{},\"off_ms\":{:.1},\"off_p90\":{:.1},\"off_cpu\":{:.1},\"off_n\":{},\"on_ms\":{:.1},\"on_p90\":{:.1},\"on_cpu\":{:.1},\"on_n\":{},\"valid\":{}}}",
                    self.started, first_off, offb.ms, offb.p90, offb.cpu, offb.n, onb.ms, onb.p90, onb.cpu, onb.n, ok);
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(LOG)) { let _ = writeln!(f, "{}", line); }
                write_summary(dir);
                Some(if ok { format!("impact test done: {:.0} ms without snifrig, {:.0} ms with it (CPU {:.0}% and {:.0}%)", offb.ms, onb.ms, offb.cpu, onb.cpu) }
                     else { format!("impact test not counted: the load changed too much between the blocks (CPU {:.0}% and {:.0}%)", offb.cpu, onb.cpu) })
            }
        }
    }
}

/// Recompute impact-summary.json from every test in impact.jsonl.
pub fn write_summary(dir: &Path) -> Summary {
    use crate::json::{num_field, esc};
    let text = std::fs::read_to_string(dir.join(LOG)).unwrap_or_default();
    let mut tests = 0;
    let mut pairs = Vec::new();
    let mut last = 0.0f64;
    for l in text.lines() {
        tests += 1;
        last = last.max(num_field(l, "start").unwrap_or(0.0));
        if l.contains("\"valid\":true") { if let (Some(a), Some(b)) = (num_field(l, "off_ms"), num_field(l, "on_ms")) { pairs.push((a, b)); } }
    }
    let s = summarize(&pairs, tests);
    let body = format!("{{\"tests\":{},\"valid\":{},\"slower_pct\":{:.1},\"low\":{:.1},\"high\":{:.1},\"faster_pct\":{:.1},\"last\":{:.0},\"text\":\"{}\"}}\n",
        s.tests, s.valid, s.slower_pct, s.low, s.high, s.faster_pct, last, esc(&sentence(&s)));
    crate::rules::write_atomic(&dir.join(SUMMARY), &body);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fair_comparisons_only() {
        let a = Block { ms: 200.0, p90: 400.0, cpu: 80.0, n: 36 };
        let b = Block { ms: 100.0, p90: 150.0, cpu: 75.0, n: 36 };
        assert!(valid(&a, &b));
        assert!(!valid(&a, &Block { cpu: 50.0, ..b.clone() }), "load changed too much");
        assert!(!valid(&a, &Block { n: 5, ..b }), "too few samples");
    }

    #[test]
    fn summary_and_sentence() {
        let s = summarize(&[(200.0, 100.0), (150.0, 100.0), (300.0, 120.0), (110.0, 100.0), (180.0, 100.0)], 6);
        assert_eq!(s.valid, 5);
        assert!((s.slower_pct - 80.0).abs() < 0.1, "{}", s.slower_pct);
        assert!((s.faster_pct - 44.4).abs() < 0.2, "{}", s.faster_pct);
        assert!(sentence(&s).contains("80% slower without snifrig"));
        let unclear = summarize(&[(90.0, 100.0), (100.0, 100.0), (130.0, 100.0)], 3);
        assert!(sentence(&unclear).starts_with("No clear difference"), "an honest answer when it does not help");
        assert!(sentence(&summarize(&[(200.0, 100.0)], 1)).starts_with("Measuring"));
    }

    #[test]
    fn a_test_runs_two_blocks_and_stops_when_you_need_the_pc() {
        let d = std::env::temp_dir().join(format!("snif-impact-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::remove_file(d.join(LOG));
        let mut i = Impact::default();
        let mut t = 100_000.0;
        let now = |t: f64, ms: f64| Now { now: t, cpu: 80.0, feel_ms: Some(ms), gaming: false, user_wants_help: false };
        assert!(i.tick(&d, &now(t, 0.0)).unwrap().contains("started"));
        assert!(i.holding(), "first block holds off (even run)");
        for _ in 0..181 { t += 1.0; i.tick(&d, &now(t, 200.0)); }
        assert!(!i.holding(), "second block: snifrig works");
        let mut done = None;
        for _ in 0..181 { t += 1.0; if let Some(m) = i.tick(&d, &now(t, 100.0)) { done = Some(m); } }
        assert!(done.unwrap().contains("200 ms without snifrig, 100 ms with it"));
        let s = write_summary(&d);
        assert_eq!((s.tests, s.valid), (1, 1));
        // a game interrupts a test
        let mut j = Impact::default();
        assert!(j.tick(&d, &Now { now: 1e6, cpu: 90.0, feel_ms: None, gaming: false, user_wants_help: false }).is_some());
        assert!(j.tick(&d, &Now { now: 1e6 + 1.0, cpu: 90.0, feel_ms: None, gaming: true, user_wants_help: false }).unwrap().contains("stopped"));
        assert!(!j.holding());
        let _ = std::fs::remove_dir_all(&d);
    }
}

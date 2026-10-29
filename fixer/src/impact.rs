//! Measured impact on overall system performance: "over the last 7 days, your PC responded X%
//! faster with snifrig", measured on this PC over everything you did, not one moment.
//!
//! How: snifrig measures how fast the PC responds all day (the feel meter: window response,
//! message lag, your own input, sampled every 5 seconds). At random moments, about 4% of the
//! time, it holds off for 2 minutes (nothing eased, no new rules applied). Because those moments
//! are picked at random, they see the same mix of your real use as the rest of the day, so
//! "responsiveness while holding off" vs "the rest of the time" is a fair, whole-system
//! comparison. Never while a full-screen app (a game, a video) is in front, never while you ask
//! for help. Every holdout and every day's totals are kept, so the number can be checked.
//! It asks first: when you are at the PC, a card asks "can we run a small test?" (at most once
//! every 2 hours); no answer in a minute means it runs only if you have stepped away. When you
//! have been away for 5 minutes it just measures, since nobody is disturbed. Holdouts therefore
//! lean towards calmer moments, which can only make the number lower, never higher.
//! Conservative: rules already applied stay in place while snifrig holds off.

use std::collections::BTreeMap;
use std::path::Path;

pub const HOLD_SECS: f64 = 120.0;
/// Chance per 2-minute slot that it becomes a holdout (about 4% of the time).
pub const HOLD_CHANCE: f64 = 0.04;
pub const MAX_HOLDS_PER_DAY: usize = 48;
pub const DAYS: i64 = 7;
pub const MIN_OFF_SAMPLES: u64 = 240; // 20 minutes of holdouts
pub const MIN_ON_SAMPLES: u64 = 2000;
pub const HOLDS_LOG: &str = "impact-holdouts.jsonl";
pub const DAYS_FILE: &str = "impact-days.txt";
pub const SUMMARY: &str = "impact-summary.json";
pub const OFF_FILE: &str = "impact-off";
pub const NOW_FILE: &str = "impact-test-now";
pub const ANSWER_FILE: &str = "impact-answer";
pub const AWAY_SECS: f64 = 300.0;
pub const ASK_GAP_SECS: f64 = 7200.0;
pub const ASK_TIMEOUT_SECS: f64 = 60.0;

/// One day's totals for one mode: samples, sum of response ms, samples that felt sluggish.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tally { pub n: u64, pub sum: f64, pub slow: u64 }

impl Tally {
    pub fn add(&mut self, ms: f64, slow: bool) { self.n += 1; self.sum += ms; if slow { self.slow += 1; } }
    pub fn mean(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum / self.n as f64 } }
    pub fn slow_pct(&self) -> f64 { if self.n == 0 { 0.0 } else { self.slow as f64 * 100.0 / self.n as f64 } }
    fn plus(self, o: Tally) -> Tally { Tally { n: self.n + o.n, sum: self.sum + o.sum, slow: self.slow + o.slow } }
}

/// Per local day: (with snifrig, snifrig holding off).
pub type Days = BTreeMap<i64, (Tally, Tally)>;

pub fn parse_days(text: &str) -> Days {
    let mut d = Days::new();
    for l in text.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 7 { continue; }
        let u = |i: usize| f[i].parse::<u64>().unwrap_or(0);
        let x = |i: usize| f[i].parse::<f64>().unwrap_or(0.0);
        if let Ok(day) = f[0].parse::<i64>() { d.insert(day, (Tally { n: u(1), sum: x(2), slow: u(3) }, Tally { n: u(4), sum: x(5), slow: u(6) })); }
    }
    d
}

pub fn write_days(d: &Days) -> String {
    d.iter().map(|(k, (a, b))| format!("{} {} {:.1} {} {} {:.1} {}\n", k, a.n, a.sum, a.slow, b.n, b.sum, b.slow)).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub on: Tally, pub off: Tally, pub ready: bool,
    /// How much slower the PC responds without snifrig, and how much faster with it (%).
    pub slower_pct: f64, pub faster_pct: f64,
    /// Lowest and highest day (days with enough holdout data).
    pub low: f64, pub high: f64, pub days: usize,
}

/// Pure: the last DAYS days ending at `today`, pooled.
pub fn summarize(d: &Days, today: i64) -> Summary {
    let recent: Vec<&(Tally, Tally)> = d.range(today - DAYS + 1..=today).map(|(_, v)| v).collect();
    let (on, off) = recent.iter().fold((Tally::default(), Tally::default()), |(a, b), (x, y)| (a.plus(*x), b.plus(*y)));
    let ready = off.n >= MIN_OFF_SAMPLES && on.n >= MIN_ON_SAMPLES && on.mean() > 0.0;
    let slower = if on.mean() > 0.0 { (off.mean() / on.mean() - 1.0) * 100.0 } else { 0.0 };
    let mut per: Vec<f64> = recent.iter().filter(|(a, b)| b.n >= 60 && a.n >= 600 && a.mean() > 0.0).map(|(a, b)| (b.mean() / a.mean() - 1.0) * 100.0).collect();
    per.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Summary { on, off, ready, slower_pct: slower, faster_pct: if off.mean() > 0.0 { (1.0 - on.mean() / off.mean()) * 100.0 } else { 0.0 },
        low: per.first().copied().unwrap_or(slower), high: per.last().copied().unwrap_or(slower), days: recent.len() }
}

/// Pure: the sentence people see.
pub fn sentence(s: &Summary) -> String {
    let mins = s.off.n as f64 * 5.0 / 60.0;
    let hours = s.on.n as f64 * 5.0 / 3600.0;
    if !s.ready {
        return format!("Measuring: {:.0} of 20 minutes of comparison collected. snifrig measures your whole day and holds off at random moments, 2 minutes at a time, to compare.", mins.min(19.0));
    }
    if s.faster_pct < 2.0 {
        return format!("No clear difference measured over the last {} days ({:.0} hours measured).", s.days.min(DAYS as usize), hours);
    }
    format!("Over the last {} days your PC responded {:.0}% faster with snifrig. It felt sluggish {:.0}% of the time without snifrig and {:.0}% with it. Measured over {:.0} hours of your normal use, against {:.0} minutes where snifrig held off at random.",
        s.days.min(DAYS as usize), s.faster_pct, s.off.slow_pct(), s.on.slow_pct(), hours, mins)
}

/// What the PC is doing, as the meter needs it.
pub struct Now { pub now: f64, pub local_day: i64, pub sample: Option<(f64, bool)>, pub fullscreen: bool, pub user_wants_help: bool, pub idle_secs: f64 }

/// What a tick asks the fixer to do.
#[derive(Debug, Default, PartialEq)]
pub struct Out {
    pub note: Option<String>,
    /// Some(true): show the "can we run a small test?" card; Some(false): take it away.
    pub card: Option<bool>,
    /// You said it feels slow right now.
    pub felt_slow: bool,
}

pub struct Impact {
    days: Days,
    hold_until: f64,
    slot: i64,
    holds_today: Vec<f64>,
    rng: u64,
    dirty_at: f64,
    asking: Option<f64>,
    last_ask: f64,
}

impl Impact {
    pub fn new(dir: &Path) -> Impact {
        let mut seed = [0u8; 8];
        #[cfg(windows)]
        {
            #[link(name = "advapi32", kind = "raw-dylib")]
            extern "system" { #[link_name = "SystemFunction036"] fn RtlGenRandom(buf: *mut u8, len: u32) -> u8; }
            unsafe { RtlGenRandom(seed.as_mut_ptr(), 8); }
        }
        #[cfg(not(windows))]
        { use std::io::Read; let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut seed)); }
        let holds: Vec<f64> = std::fs::read_to_string(dir.join(HOLDS_LOG)).unwrap_or_default().lines().filter_map(|l| crate::json::num_field(l, "start")).collect();
        Impact { days: parse_days(&std::fs::read_to_string(dir.join(DAYS_FILE)).unwrap_or_default()), hold_until: 0.0, slot: 0, holds_today: holds, rng: u64::from_le_bytes(seed) | 1, dirty_at: 0.0, asking: None, last_ask: 0.0 }
    }

    fn rand(&mut self) -> f64 { self.rng ^= self.rng << 13; self.rng ^= self.rng >> 7; self.rng ^= self.rng << 17; (self.rng >> 11) as f64 / (1u64 << 53) as f64 }

    /// True while snifrig should hold off (a random holdout).
    pub fn holding(&self, now: f64) -> bool { now < self.hold_until }

    fn start(&mut self, dir: &Path, now: f64, why: &str, felt_slow: bool) -> String {
        self.hold_until = now + HOLD_SECS;
        self.holds_today.push(now);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(HOLDS_LOG)) {
            let _ = writeln!(f, "{{\"start\":{:.0},\"secs\":{:.0},\"why\":\"{}\",\"felt_slow\":{}}}", now, HOLD_SECS, why, felt_slow);
        }
        format!("measuring: snifrig holds off for 2 minutes to compare ({})", why)
    }

    /// Call once a second.
    pub fn tick(&mut self, dir: &Path, n: &Now) -> Out {
        let mut out = Out::default();
        if n.now < self.hold_until && (n.fullscreen || n.user_wants_help) {
            self.hold_until = 0.0; // you need the PC: stop holding off at once
        }
        // Waiting for your answer to "can we run a small test?"
        if let Some(since) = self.asking {
            let answer = std::fs::read_to_string(dir.join(ANSWER_FILE)).ok();
            if answer.is_some() { let _ = std::fs::remove_file(dir.join(ANSWER_FILE)); }
            let a = answer.as_deref().map(|s| s.trim().to_lowercase());
            let timed_out = n.now - since >= ASK_TIMEOUT_SECS;
            if a.is_some() || timed_out || n.fullscreen {
                self.asking = None;
                out.card = Some(false);
                match a.as_deref() {
                    Some("yes") => out.note = Some(self.start(dir, n.now, "you said yes", false)),
                    Some("slow") => { out.felt_slow = true; out.note = Some(self.start(dir, n.now, "you said yes, it feels slow", true)); }
                    Some(_) => out.note = Some("measuring: not now, you said no".into()),
                    None if !n.fullscreen && n.idle_secs >= ASK_TIMEOUT_SECS => out.note = Some(self.start(dir, n.now, "no answer, you had stepped away", false)),
                    None => out.note = Some("measuring: no answer and you were busy, so not now".into()),
                }
            }
        }
        // Every 2-minute slot gets one random draw (the same chance whatever the PC is doing).
        let slot = (n.now / HOLD_SECS).floor() as i64;
        let asked = dir.join(NOW_FILE).exists();
        if asked { let _ = std::fs::remove_file(dir.join(NOW_FILE)); }
        if (slot != self.slot || asked) && !self.holding(n.now) {
            self.slot = slot;
            self.holds_today.retain(|t| n.now - t < 86400.0);
            let allowed = !dir.join(OFF_FILE).exists() && !n.fullscreen && !n.user_wants_help && self.holds_today.len() < MAX_HOLDS_PER_DAY;
            let draw = self.rand();
            if allowed && self.asking.is_none() && (asked || draw < HOLD_CHANCE) {
                if asked { out.note = Some(self.start(dir, n.now, "you asked", false)); }
                else if n.idle_secs >= AWAY_SECS { out.note = Some(self.start(dir, n.now, "picked at random, you were away", false)); }
                else if n.now - self.last_ask >= ASK_GAP_SECS {
                    // You are at the PC: ask first.
                    self.asking = Some(n.now);
                    self.last_ask = n.now;
                    out.card = Some(true);
                }
            }
        }
        if let Some((ms, slow)) = n.sample {
            let held = self.holding(n.now);
            let e = self.days.entry(n.local_day).or_default();
            if held { e.1.add(ms, slow) } else { e.0.add(ms, slow) }
            if n.now - self.dirty_at > 300.0 { self.save(dir, n.local_day); self.dirty_at = n.now; }
        }
        out
    }

    pub fn save(&mut self, dir: &Path, today: i64) {
        let old: Vec<i64> = self.days.keys().filter(|k| **k < today - 60).copied().collect();
        for k in old { self.days.remove(&k); }
        crate::rules::write_atomic(&dir.join(DAYS_FILE), &write_days(&self.days));
        write_summary_from(dir, &self.days, today);
    }
}

fn write_summary_from(dir: &Path, d: &Days, today: i64) -> Summary {
    use crate::json::esc;
    let s = summarize(d, today);
    let rows: Vec<String> = d.range(today - DAYS + 1..=today).map(|(k, (a, b))| format!("{{\"day\":{},\"on_ms\":{:.1},\"on_slow\":{:.1},\"on_min\":{:.0},\"off_ms\":{:.1},\"off_slow\":{:.1},\"off_min\":{:.1}}}",
        k, a.mean(), a.slow_pct(), a.n as f64 * 5.0 / 60.0, b.mean(), b.slow_pct(), b.n as f64 * 5.0 / 60.0)).collect();
    let body = format!("{{\"ready\":{},\"faster_pct\":{:.1},\"slower_pct\":{:.1},\"low\":{:.1},\"high\":{:.1},\"on_ms\":{:.1},\"off_ms\":{:.1},\"on_slow\":{:.1},\"off_slow\":{:.1},\"hours\":{:.1},\"off_minutes\":{:.1},\"text\":\"{}\",\"days\":[{}]}}\n",
        s.ready, s.faster_pct, s.slower_pct, s.low, s.high, s.on.mean(), s.off.mean(), s.on.slow_pct(), s.off.slow_pct(), s.on.n as f64 * 5.0 / 3600.0, s.off.n as f64 * 5.0 / 60.0, esc(&sentence(&s)), rows.join(","));
    crate::rules::write_atomic(&dir.join(SUMMARY), &body);
    s
}

/// Recompute impact-summary.json from impact-days.txt (CLI and start-up).
pub fn write_summary(dir: &Path, today: i64) -> Summary {
    let d = parse_days(&std::fs::read_to_string(dir.join(DAYS_FILE)).unwrap_or_default());
    write_summary_from(dir, &d, today)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(on: (u64, f64, u64), off: (u64, f64, u64)) -> (Tally, Tally) {
        (Tally { n: on.0, sum: on.1 * on.0 as f64, slow: on.2 }, Tally { n: off.0, sum: off.1 * off.0 as f64, slow: off.2 })
    }

    #[test]
    fn pooled_over_the_week() {
        let mut d = Days::new();
        d.insert(10, day((5000, 60.0, 250), (200, 100.0, 40)));
        d.insert(11, day((5000, 80.0, 500), (200, 120.0, 50)));
        d.insert(2, day((9000, 10.0, 0), (900, 500.0, 900))); // older than 7 days: ignored
        let s = summarize(&d, 11);
        assert!(s.ready);
        assert!((s.on.mean() - 70.0).abs() < 0.01 && (s.off.mean() - 110.0).abs() < 0.01);
        assert!((s.faster_pct - 36.36).abs() < 0.1, "{}", s.faster_pct);
        assert!((s.low - 50.0).abs() < 0.1 && (s.high - 66.7).abs() < 0.1, "{} {}", s.low, s.high);
        let t = sentence(&s);
        assert!(t.contains("36% faster with snifrig") && t.contains("sluggish 22% of the time without snifrig and 8% with it"), "{}", t);
    }

    #[test]
    fn honest_when_not_ready_or_no_difference() {
        let mut d = Days::new();
        d.insert(1, day((5000, 60.0, 0), (50, 100.0, 0)));
        assert!(sentence(&summarize(&d, 1)).starts_with("Measuring"));
        d.insert(1, day((5000, 60.0, 0), (300, 60.5, 0)));
        assert!(sentence(&summarize(&d, 1)).starts_with("No clear difference"));
    }

    #[test]
    fn days_roundtrip() {
        let mut d = Days::new();
        d.insert(20000, day((10, 5.0, 1), (2, 9.0, 1)));
        assert_eq!(parse_days(&write_days(&d)), d);
    }

    #[test]
    fn holdouts_are_random_two_minute_blocks_and_stop_for_full_screen() {
        let dir = std::env::temp_dir().join(format!("snif-impact2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(HOLDS_LOG));
        let mut i = Impact::new(&dir);
        i.holds_today.clear();
        let mut holds = 0;
        let mut held_secs = 0;
        let mut t = 1e9;
        for _ in 0..(86400 * 2) {
            t += 1.0;
            if i.tick(&dir, &Now { now: t, local_day: 1, sample: Some((50.0, false)), fullscreen: false, user_wants_help: false, idle_secs: 900.0 }).note.is_some() { holds += 1; }
            if i.holding(t) { held_secs += 1; }
        }
        let share = held_secs as f64 / (86400.0 * 2.0);
        assert!(share > 0.02 && share < 0.07, "about 4% of the time held off, got {:.3} ({} holdouts)", share, holds);
        // a full-screen app ends a holdout at once
        let _ = std::fs::write(dir.join(NOW_FILE), "1");
        i.tick(&dir, &Now { now: t + 1.0, local_day: 1, sample: None, fullscreen: false, user_wants_help: false, idle_secs: 0.0 });
        assert!(i.holding(t + 1.0) || i.holds_today.len() >= MAX_HOLDS_PER_DAY);
        i.tick(&dir, &Now { now: t + 2.0, local_day: 1, sample: None, fullscreen: true, user_wants_help: false, idle_secs: 0.0 });
        assert!(!i.holding(t + 2.0));
        // At the PC: it asks first, and no answer while you keep working means no test.
        let mut k = Impact::new(&dir);
        k.holds_today.clear();
        let mut t = 2e9;
        let mut asked_at = None;
        for _ in 0..100_000 {
            t += 1.0;
            let o = k.tick(&dir, &Now { now: t, local_day: 2, sample: None, fullscreen: false, user_wants_help: false, idle_secs: 3.0 });
            assert!(!k.holding(t), "never holds off unasked while you work");
            if o.card == Some(true) { asked_at = Some(t); break; }
        }
        let at = asked_at.expect("it asked");
        let mut last = Out::default();
        for s in 1..=61 { last = k.tick(&dir, &Now { now: at + s as f64, local_day: 2, sample: None, fullscreen: false, user_wants_help: false, idle_secs: 3.0 }); if last.card.is_some() { break; } }
        assert_eq!(last.card, Some(false));
        assert!(last.note.unwrap().contains("busy"));
        assert!(!k.holding(at + 62.0));
        // "yes, it feels slow" starts a test and says so.
        k.asking = Some(at + 100.0);
        let _ = std::fs::write(dir.join(ANSWER_FILE), "slow");
        let o = k.tick(&dir, &Now { now: at + 101.0, local_day: 2, sample: None, fullscreen: false, user_wants_help: false, idle_secs: 1.0 });
        assert!(o.felt_slow && k.holding(at + 102.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

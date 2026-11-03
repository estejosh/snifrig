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
//! have been away for 5 minutes it just measures, since nobody is disturbed.
//! Like with like: most holdouts happen while you are away, when a PC is calm anyway, so
//! comparing them with your working hours would make snifrig look worse than doing nothing.
//! Every sample is therefore filed as "you at the PC" or "you away", holdouts are compared
//! only with normal time in the same state, and the two results are weighted by how much of
//! your real use each state is. A state without enough holdouts yet is left out, and the
//! sentence says which states the number covers. A result that is worse is reported as worse.
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
/// Per day, split by whether you were at the PC (the older impact-days.txt mixed the two).
pub const DAYS_FILE: &str = "impact-days2.txt";
/// Enough holdout samples in a state to trust its comparison: 10 minutes at the PC, 20 away.
pub const MIN_OFF_HERE: u64 = 120;
pub const MIN_OFF_AWAY: u64 = 240;
pub const MIN_ON_STATE: u64 = 600;
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

/// Per local day: [with snifrig at the PC, held off at the PC, with snifrig away, held off away].
pub type Days = BTreeMap<i64, [Tally; 4]>;
pub const HERE_ON: usize = 0;
pub const HERE_OFF: usize = 1;
pub const AWAY_ON: usize = 2;
pub const AWAY_OFF: usize = 3;

pub fn parse_days(text: &str) -> Days {
    let mut d = Days::new();
    for l in text.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 13 { continue; }
        let u = |i: usize| f[i].parse::<u64>().unwrap_or(0);
        let x = |i: usize| f[i].parse::<f64>().unwrap_or(0.0);
        if let Ok(day) = f[0].parse::<i64>() {
            let t = |k: usize| Tally { n: u(1 + k * 3), sum: x(2 + k * 3), slow: u(3 + k * 3) };
            d.insert(day, [t(0), t(1), t(2), t(3)]);
        }
    }
    d
}

pub fn write_days(d: &Days) -> String {
    d.iter().map(|(k, v)| format!("{} {}\n", k, v.iter().map(|a| format!("{} {:.1} {}", a.n, a.sum, a.slow)).collect::<Vec<_>>().join(" "))).collect()
}

/// One state's comparison: (normal time, held off, faster %, slow % normal, slow % held off).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Part { pub on: Tally, pub off: Tally, pub faster_pct: f64 }

#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// All samples (for the hours and minutes measured).
    pub on: Tally, pub off: Tally, pub ready: bool,
    /// How much slower the PC responds without snifrig, and how much faster with it (%), each
    /// state compared like with like and weighted by how much of your use it is.
    pub slower_pct: f64, pub faster_pct: f64,
    /// How often it felt sluggish with and without snifrig (%), weighted the same way.
    pub on_slow: f64, pub off_slow: f64,
    /// Lowest and highest day (state-matched, days with enough holdout data).
    pub low: f64, pub high: f64, pub days: usize,
    /// The two states, when each has enough holdouts.
    pub here: Option<Part>, pub away: Option<Part>,
}

fn part(on: Tally, off: Tally, min_off: u64) -> Option<Part> {
    (off.n >= min_off && on.n >= MIN_ON_STATE && on.mean() > 0.0 && off.mean() > 0.0)
        .then(|| Part { on, off, faster_pct: (1.0 - on.mean() / off.mean()) * 100.0 })
}

/// Pure: the last DAYS days ending at `today`, pooled per state, then weighted by real use.
pub fn summarize(d: &Days, today: i64) -> Summary {
    let recent: Vec<&[Tally; 4]> = d.range(today - DAYS + 1..=today).map(|(_, v)| v).collect();
    let sum = |k: usize| recent.iter().fold(Tally::default(), |a, v| a.plus(v[k]));
    let here = part(sum(HERE_ON), sum(HERE_OFF), MIN_OFF_HERE);
    let away = part(sum(AWAY_ON), sum(AWAY_OFF), MIN_OFF_AWAY);
    let parts: Vec<Part> = [here, away].into_iter().flatten().collect();
    let w: f64 = parts.iter().map(|p| p.on.n as f64).sum();
    let avg = |f: &dyn Fn(&Part) -> f64| if w > 0.0 { parts.iter().map(|p| f(p) * p.on.n as f64).sum::<f64>() / w } else { 0.0 };
    let faster = avg(&|p| p.faster_pct);
    let slower = avg(&|p| (p.off.mean() / p.on.mean() - 1.0) * 100.0);
    let mut per: Vec<f64> = Vec::new();
    for v in &recent {
        let mut num = 0.0; let mut den = 0.0;
        for (on, off) in [(v[HERE_ON], v[HERE_OFF]), (v[AWAY_ON], v[AWAY_OFF])] {
            if off.n >= 60 && on.n >= 300 && on.mean() > 0.0 { num += (off.mean() / on.mean() - 1.0) * 100.0 * on.n as f64; den += on.n as f64; }
        }
        if den > 0.0 { per.push(num / den); }
    }
    per.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Summary { on: sum(HERE_ON).plus(sum(AWAY_ON)), off: sum(HERE_OFF).plus(sum(AWAY_OFF)), ready: !parts.is_empty(), slower_pct: slower, faster_pct: faster,
        on_slow: avg(&|p| p.on.slow_pct()), off_slow: avg(&|p| p.off.slow_pct()),
        low: per.first().copied().unwrap_or(slower), high: per.last().copied().unwrap_or(slower), days: recent.len(), here, away }
}

/// Pure: the sentence people see.
pub fn sentence(s: &Summary) -> String {
    let mins = s.off.n as f64 * 5.0 / 60.0;
    let hours = s.on.n as f64 * 5.0 / 3600.0;
    if !s.ready {
        return format!("Measuring: {:.0} minutes of comparison collected so far. snifrig measures your whole day and holds off at random moments, 2 minutes at a time, comparing like with like (you at the PC, or away).", mins);
    }
    let scope = match (s.here.is_some(), s.away.is_some()) {
        (true, true) => String::new(),
        (false, true) => " Measured while you were away only: not enough tests yet while you were at the PC.".into(),
        _ => " Measured while you were at the PC only: not enough tests yet while you were away.".into(),
    };
    let days = s.days.min(DAYS as usize);
    if s.faster_pct.abs() < 3.0 {
        return format!("No clear difference measured over the last {} days ({:.0} hours measured).{}", days, hours, scope);
    }
    if s.faster_pct < 0.0 {
        return format!("Over the last {} days your PC responded {:.0}% slower with snifrig than when it held off. It felt sluggish {:.0}% of the time without snifrig and {:.0}% with it. Measured over {:.0} hours, against {:.0} minutes where snifrig held off at random.{}",
            days, -s.faster_pct, s.off_slow, s.on_slow, hours, mins, scope);
    }
    format!("Over the last {} days your PC responded {:.0}% faster with snifrig. It felt sluggish {:.0}% of the time without snifrig and {:.0}% with it. Measured over {:.0} hours of your normal use, against {:.0} minutes where snifrig held off at random.{}",
        days, s.faster_pct, s.off_slow, s.on_slow, hours, mins, scope)
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
            let away = n.idle_secs >= AWAY_SECS;
            let e = self.days.entry(n.local_day).or_default();
            e[match (away, held) { (false, false) => HERE_ON, (false, true) => HERE_OFF, (true, false) => AWAY_ON, (true, true) => AWAY_OFF }].add(ms, slow);
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
    let rows: Vec<String> = d.range(today - DAYS + 1..=today).map(|(k, v)| {
        let on = v[HERE_ON].plus(v[AWAY_ON]); let off = v[HERE_OFF].plus(v[AWAY_OFF]);
        format!("{{\"day\":{},\"on_ms\":{:.1},\"on_slow\":{:.1},\"on_min\":{:.0},\"off_ms\":{:.1},\"off_slow\":{:.1},\"off_min\":{:.1},\"here_on_min\":{:.0},\"here_off_min\":{:.1},\"away_on_min\":{:.0},\"away_off_min\":{:.1}}}",
            k, on.mean(), on.slow_pct(), on.n as f64 * 5.0 / 60.0, off.mean(), off.slow_pct(), off.n as f64 * 5.0 / 60.0,
            v[HERE_ON].n as f64 * 5.0 / 60.0, v[HERE_OFF].n as f64 * 5.0 / 60.0, v[AWAY_ON].n as f64 * 5.0 / 60.0, v[AWAY_OFF].n as f64 * 5.0 / 60.0)
    }).collect();
    let p = |x: &Option<Part>| x.map(|p| format!("{{\"faster_pct\":{:.1},\"on_ms\":{:.2},\"off_ms\":{:.2},\"on_slow\":{:.1},\"off_slow\":{:.1},\"on_min\":{:.0},\"off_min\":{:.1}}}",
        p.faster_pct, p.on.mean(), p.off.mean(), p.on.slow_pct(), p.off.slow_pct(), p.on.n as f64 * 5.0 / 60.0, p.off.n as f64 * 5.0 / 60.0)).unwrap_or_else(|| "null".into());
    let body = format!("{{\"ready\":{},\"faster_pct\":{:.1},\"slower_pct\":{:.1},\"low\":{:.1},\"high\":{:.1},\"on_ms\":{:.1},\"off_ms\":{:.1},\"on_slow\":{:.1},\"off_slow\":{:.1},\"hours\":{:.1},\"off_minutes\":{:.1},\"here\":{},\"away\":{},\"text\":\"{}\",\"days\":[{}]}}\n",
        s.ready, s.faster_pct, s.slower_pct, s.low, s.high, s.on.mean(), s.off.mean(), s.on_slow, s.off_slow, s.on.n as f64 * 5.0 / 3600.0, s.off.n as f64 * 5.0 / 60.0, p(&s.here), p(&s.away), esc(&sentence(&s)), rows.join(","));
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

    fn t(n: u64, mean: f64, slow: u64) -> Tally { Tally { n, sum: mean * n as f64, slow } }

    #[test]
    fn like_with_like() {
        // Working hours are slower than away time anyway. Holdouts mostly happen while away.
        // Pooled, snifrig would look 70% "slower"; matched, it is 20% faster in both states.
        let mut d = Days::new();
        d.insert(10, [t(8000, 2.0, 400), t(200, 2.5, 20), t(8000, 0.4, 0), t(2000, 0.5, 0)]);
        let s = summarize(&d, 10);
        assert!(s.ready);
        assert!((s.faster_pct - 20.0).abs() < 0.1, "{}", s.faster_pct);
        assert!(s.here.is_some() && s.away.is_some());
        let txt = sentence(&s);
        assert!(txt.contains("20% faster") && !txt.contains("away only"), "{}", txt);
        // Not enough holdouts at the PC: the number covers away time only, and says so.
        d.insert(10, [t(8000, 2.0, 400), t(50, 2.5, 20), t(8000, 0.4, 0), t(2000, 0.5, 0)]);
        let s = summarize(&d, 10);
        assert!(s.here.is_none() && sentence(&s).contains("away only"));
    }

    #[test]
    fn worse_is_reported_as_worse() {
        let mut d = Days::new();
        d.insert(1, [t(5000, 3.0, 300), t(300, 2.0, 6), t(0, 0.0, 0), t(0, 0.0, 0)]);
        let txt = sentence(&summarize(&d, 1));
        assert!(txt.contains("50% slower with snifrig"), "{}", txt);
    }

    #[test]
    fn honest_when_not_ready_or_no_difference() {
        let mut d = Days::new();
        d.insert(1, [t(5000, 60.0, 0), t(50, 100.0, 0), t(0, 0.0, 0), t(0, 0.0, 0)]);
        assert!(sentence(&summarize(&d, 1)).starts_with("Measuring"));
        d.insert(1, [t(5000, 60.0, 0), t(300, 60.5, 0), t(0, 0.0, 0), t(0, 0.0, 0)]);
        assert!(sentence(&summarize(&d, 1)).starts_with("No clear difference"));
    }

    #[test]
    fn days_roundtrip() {
        let mut d = Days::new();
        d.insert(20000, [t(10, 5.0, 1), t(2, 9.0, 1), t(3, 1.0, 0), t(4, 2.0, 0)]);
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

//! What is normal for this machine, by hour of the week.
//!
//! 168 hour-of-week buckets (plus 24 coarse hour-of-day buckets) each keep, per metric (total CPU %,
//! commit %), an EWMA (alpha 0.1) of the mean and variance of the hourly average, plus the within-hour
//! variance, and a count of which processes were the top CPU users. `update` is called once a minute;
//! the minutes of one hour are averaged and folded into the EWMA when that hour is over, so one
//! "hourly sample" is one real hour. Page-in pressure is not collected (no cheap source).
//!
//! `unusual(metric, value, now)` gives a z-score, or None until the bucket has MIN_WEEK_HOURS
//! hourly samples (about 4 weeks) or, failing that, the hour-of-day bucket has MIN_DAY_HOURS (3 days).
//! Times passed in are real unix time; set_tz shifts them to local time for bucketing.
//!
//! baseline.json: one bucket per line, {"b":"w|d","i":N,"hours":N,"minutes":N,"cpu_m":..,"cpu_v":..,"cpu_w":..,
//! "commit_m":..,"commit_v":..,"commit_w":..,"names":"a.exe:12|b.exe:3"} (names cannot contain '|' or ':' on Windows).

use crate::json::{esc, num_field, str_field};
use std::collections::HashMap;
use std::path::Path;

pub const ALPHA: f64 = 0.1;
pub const MIN_WEEK_HOURS: u32 = 4;
pub const MIN_DAY_HOURS: u32 = 3;
pub const TOP_NAMES: usize = 10;
pub const SAVE_EVERY_SECS: f64 = 600.0;
/// Smallest spread (in percentage points) used for a z-score, so a perfectly flat bucket does not give infinite z.
pub const MIN_SD: f64 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Metric { Cpu, Commit }

impl Metric {
    fn ix(self) -> usize { match self { Metric::Cpu => 0, Metric::Commit => 1 } }
}

#[derive(Clone, Copy, Debug, Default)]
struct Ewma { mean: f64, var: f64, wvar: f64, n: u32 }

impl Ewma {
    /// Fold one finished hour (its mean and its within-hour variance).
    fn fold(&mut self, hour_mean: f64, hour_var: f64) {
        if self.n == 0 {
            self.mean = hour_mean; self.var = 0.0; self.wvar = hour_var;
        } else {
            let d = hour_mean - self.mean;
            self.mean += ALPHA * d;
            self.var = (1.0 - ALPHA) * (self.var + ALPHA * d * d);
            self.wvar += ALPHA * (hour_var - self.wvar);
        }
        self.n += 1;
    }
    fn sd(&self) -> f64 { (self.var + self.wvar).max(0.0).sqrt().max(MIN_SD) }
}

/// The hour in progress (Welford).
#[derive(Clone, Copy, Debug, Default)]
struct Acc { hour: i64, n: u32, mean: f64, m2: f64 }

impl Acc {
    fn push(&mut self, v: f64) {
        self.n += 1;
        let d = v - self.mean;
        self.mean += d / self.n as f64;
        self.m2 += d * (v - self.mean);
    }
}

#[derive(Clone, Debug, Default)]
struct Bucket {
    m: [Ewma; 2],
    acc: [Acc; 2],
    /// Minute samples that fed `names`.
    minutes: u32,
    names: HashMap<String, u32>,
}

impl Bucket {
    fn hours(&self) -> u32 { self.m[0].n }

    fn fold_before(&mut self, cur_hour: i64) {
        for i in 0..2 {
            let a = self.acc[i];
            if a.n > 0 && a.hour < cur_hour {
                self.m[i].fold(a.mean, if a.n > 1 { a.m2 / (a.n - 1) as f64 } else { 0.0 });
                self.acc[i] = Acc::default();
            }
        }
    }

    fn push(&mut self, hour: i64, vals: [f64; 2], top: &[String]) {
        for i in 0..2 {
            if self.acc[i].n == 0 { self.acc[i].hour = hour; }
            self.acc[i].push(vals[i]);
        }
        self.minutes += 1;
        for n in top { *self.names.entry(n.to_lowercase()).or_insert(0) += 1; }
        if self.names.len() > TOP_NAMES * 2 { self.prune(); }
    }

    fn prune(&mut self) {
        let mut v: Vec<(String, u32)> = self.names.drain().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(TOP_NAMES);
        self.names = v.into_iter().collect();
    }
}

pub struct Baseline {
    week: Vec<Bucket>,
    day: Vec<Bucket>,
    tz_offset: i64,
    last_save: f64,
    dirty: bool,
}

impl Baseline {
    pub fn new() -> Baseline {
        Baseline { week: vec![Bucket::default(); 168], day: vec![Bucket::default(); 24], tz_offset: 0, last_save: 0.0, dirty: false }
    }

    /// Seconds to add to unix time to get local time.
    pub fn set_tz(&mut self, offset_secs: i64) { self.tz_offset = offset_secs; }

    fn local(&self, now: f64) -> i64 { now as i64 + self.tz_offset }

    /// (hour-of-week 0..168 with Monday 00:00 = 0, hour-of-day, absolute local hour number)
    fn slots(&self, now: f64) -> (usize, usize, i64) {
        let t = self.local(now);
        let h = t.div_euclid(3600);
        let days = h.div_euclid(24);
        let hod = h.rem_euclid(24);
        let dow = (days + 3).rem_euclid(7); // 1970-01-01 was a Thursday; Monday = 0
        ((dow * 24 + hod) as usize, hod as usize, h)
    }

    /// Fold every finished hour into its EWMA.
    pub fn fold_before(&mut self, now: f64) {
        let (_, _, h) = self.slots(now);
        for b in self.week.iter_mut().chain(self.day.iter_mut()) { b.fold_before(h); }
    }

    /// Once a minute: total CPU %, commit %, and the current top CPU process names.
    pub fn update(&mut self, now: f64, cpu: f64, commit: f64, top: &[String]) {
        self.fold_before(now);
        let (w, d, h) = self.slots(now);
        let mut t: Vec<String> = top.to_vec();
        t.truncate(TOP_NAMES);
        self.week[w].push(h, [cpu, commit], &t);
        self.day[d].push(h, [cpu, commit], &t);
        self.dirty = true;
    }

    fn usable(&self, now: f64) -> Option<&Bucket> {
        let (w, d, _) = self.slots(now);
        if self.week[w].hours() >= MIN_WEEK_HOURS { Some(&self.week[w]) }
        else if self.day[d].hours() >= MIN_DAY_HOURS { Some(&self.day[d]) }
        else { None }
    }

    /// z-score of `value` against what is normal at this hour; None while there is too little history.
    pub fn unusual(&self, metric: Metric, value: f64, now: f64) -> Option<f64> {
        let e = self.usable(now)?.m[metric.ix()];
        Some((value - e.mean) / e.sd())
    }

    /// Process names that are usually among the top CPU users at this hour (lowercase, busiest first).
    pub fn expected_names(&self, now: f64) -> Vec<String> {
        let Some(b) = self.usable(now) else { return Vec::new() };
        let min = (b.minutes / 4).max(3);
        let mut v: Vec<(&String, &u32)> = b.names.iter().filter(|(_, &c)| c >= min).collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.into_iter().take(TOP_NAMES).map(|(n, _)| n.clone()).collect()
    }

    /// Hourly samples the current hour-of-week bucket has (for status output).
    pub fn week_hours(&self, now: f64) -> u32 { self.week[self.slots(now).0].hours() }

    // ---- persistence ----

    pub fn to_json(&self, now: f64) -> String {
        let mut lines = Vec::new();
        for (tag, set) in [("w", &self.week), ("d", &self.day)] {
            for (i, b) in set.iter().enumerate() {
                if b.hours() == 0 && b.minutes == 0 { continue; }
                let mut names: Vec<(&String, &u32)> = b.names.iter().collect();
                names.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                let names: Vec<String> = names.into_iter().take(TOP_NAMES).map(|(n, c)| format!("{}:{}", n, c)).collect();
                lines.push(format!("{{\"b\":\"{}\",\"i\":{},\"hours\":{},\"minutes\":{},\"cpu_m\":{:.3},\"cpu_v\":{:.3},\"cpu_w\":{:.3},\"commit_m\":{:.3},\"commit_v\":{:.3},\"commit_w\":{:.3},\"names\":\"{}\"}}",
                    tag, i, b.m[0].n, b.minutes, b.m[0].mean, b.m[0].var, b.m[0].wvar, b.m[1].mean, b.m[1].var, b.m[1].wvar, esc(&names.join("|"))));
            }
        }
        format!("{{\"unix\":{:.0},\"tz\":{},\"buckets\":[\n{}\n]}}\n", now, self.tz_offset, lines.join(",\n"))
    }

    pub fn from_json(text: &str) -> Baseline {
        let mut b = Baseline::new();
        for line in text.lines() {
            let line = line.trim().trim_end_matches(',');
            let (Some(tag), Some(i)) = (str_field(line, "b"), num_field(line, "i")) else { continue };
            let i = i as usize;
            let set = if tag == "w" { &mut b.week } else { &mut b.day };
            let Some(bk) = set.get_mut(i) else { continue };
            let n = num_field(line, "hours").unwrap_or(0.0) as u32;
            bk.m[0] = Ewma { mean: num_field(line, "cpu_m").unwrap_or(0.0), var: num_field(line, "cpu_v").unwrap_or(0.0), wvar: num_field(line, "cpu_w").unwrap_or(0.0), n };
            bk.m[1] = Ewma { mean: num_field(line, "commit_m").unwrap_or(0.0), var: num_field(line, "commit_v").unwrap_or(0.0), wvar: num_field(line, "commit_w").unwrap_or(0.0), n };
            bk.minutes = num_field(line, "minutes").unwrap_or(0.0) as u32;
            for part in str_field(line, "names").unwrap_or_default().split('|') {
                if let Some((name, c)) = part.rsplit_once(':') {
                    if let Ok(c) = c.parse::<u32>() { bk.names.insert(name.to_string(), c); }
                }
            }
        }
        b
    }

    pub fn load(dir: &Path) -> Baseline {
        Baseline::from_json(&std::fs::read_to_string(dir.join("baseline.json")).unwrap_or_default())
    }

    /// Write baseline.json (atomic) if something changed and SAVE_EVERY_SECS passed since the last write.
    pub fn maybe_save(&mut self, dir: &Path, now: f64) {
        if !self.dirty || now - self.last_save < SAVE_EVERY_SECS { return; }
        self.save(dir, now);
    }

    pub fn save(&mut self, dir: &Path, now: f64) {
        crate::rules::write_atomic(&dir.join("baseline.json"), &self.to_json(now));
        self.last_save = now;
        self.dirty = false;
    }
}

#[cfg(test)]
mod baseline_tests {
    use super::*;

    const WEEK: f64 = 604800.0;
    // 1970-01-05 was a Monday: Monday 00:00 UTC.
    const MON: f64 = 4.0 * 86400.0;

    fn names(v: &[&str]) -> Vec<String> { v.iter().map(|s| s.to_string()).collect() }

    /// Feed `weeks` Mondays at 10:00, 5 minutes each, around `level` with a little jitter.
    fn feed(b: &mut Baseline, weeks: u32, level: f64) {
        for w in 0..weeks {
            for min in 0..5 {
                let t = MON + w as f64 * WEEK + 10.0 * 3600.0 + min as f64 * 60.0;
                b.update(t, level + (min as f64 - 2.0), 50.0, &names(&["ffmpeg.exe", "chrome.exe"]));
            }
        }
    }

    #[test]
    fn hour_of_week_slots() {
        let b = Baseline::new();
        assert_eq!(b.slots(MON).0, 0);
        assert_eq!(b.slots(MON + 10.0 * 3600.0).0, 10);
        assert_eq!(b.slots(MON + 86400.0 + 3600.0).0, 25);
        assert_eq!(b.slots(MON + 6.0 * 86400.0 + 23.0 * 3600.0).0, 167);
        let mut c = Baseline::new();
        c.set_tz(-6 * 3600);
        assert_eq!(c.slots(MON + 6.0 * 3600.0).0, 0, "06:00 UTC is Monday 00:00 at UTC-6");
    }

    #[test]
    fn none_until_four_hourly_samples() {
        let mut b = Baseline::new();
        feed(&mut b, 4, 30.0);
        let t5 = MON + 4.0 * WEEK + 10.0 * 3600.0;
        b.fold_before(t5); // the fourth hour is only folded once it is over
        assert_eq!(b.week_hours(t5), 4);
        let z = b.unusual(Metric::Cpu, 30.0, t5).expect("4 samples is enough");
        assert!(z.abs() < 1.0);
        let mut c = Baseline::new();
        feed(&mut c, 2, 30.0);
        c.fold_before(t5);
        assert_eq!(c.week_hours(t5), 2);
        assert!(c.unusual(Metric::Cpu, 30.0, t5).is_none(), "2 hourly samples: neither bucket is ready");
    }

    #[test]
    fn spike_scores_high_and_calm_scores_low() {
        let mut b = Baseline::new();
        feed(&mut b, 5, 30.0);
        let t = MON + 5.0 * WEEK + 10.0 * 3600.0;
        b.fold_before(t);
        let calm = b.unusual(Metric::Cpu, 31.0, t).unwrap();
        let spike = b.unusual(Metric::Cpu, 95.0, t).unwrap();
        assert!(calm.abs() < 1.0, "{}", calm);
        assert!(spike > 5.0, "{}", spike);
        assert!(b.unusual(Metric::Commit, 50.0, t).unwrap().abs() < 1.0);
    }

    #[test]
    fn day_bucket_fallback_after_three_days() {
        let mut b = Baseline::new();
        for d in 0..3 { // 3 consecutive days at 14:00
            for min in 0..3 {
                b.update(MON + d as f64 * 86400.0 + 14.0 * 3600.0 + min as f64 * 60.0, 20.0, 40.0, &[]);
            }
        }
        // Thursday 14:00 is a different hour-of-week bucket (no history) but the same hour of day.
        let t = MON + 3.0 * 86400.0 + 14.0 * 3600.0;
        b.fold_before(t);
        assert!(b.unusual(Metric::Cpu, 20.0, t).is_some());
        assert!(b.unusual(Metric::Cpu, 20.0, t + 3600.0).is_none(), "15:00 has no history");
    }

    #[test]
    fn expected_names_and_prune() {
        let mut b = Baseline::new();
        feed(&mut b, 4, 30.0);
        let t = MON + 4.0 * WEEK + 10.0 * 3600.0;
        b.fold_before(t);
        let e = b.expected_names(t);
        assert!(e.contains(&"ffmpeg.exe".to_string()) && e.contains(&"chrome.exe".to_string()));
        assert!(b.expected_names(t + 3600.0).is_empty());
        let mut bk = Bucket::default();
        for i in 0..40 { bk.push(0, [1.0, 1.0], &[format!("p{}.exe", i)]); }
        bk.prune();
        assert_eq!(bk.names.len(), TOP_NAMES);
    }

    #[test]
    fn json_round_trip() {
        let mut b = Baseline::new();
        feed(&mut b, 5, 30.0);
        let t = MON + 5.0 * WEEK + 10.0 * 3600.0;
        b.fold_before(t);
        let c = Baseline::from_json(&b.to_json(t));
        assert_eq!(c.week_hours(t), 5);
        let (x, y) = (b.unusual(Metric::Cpu, 60.0, t).unwrap(), c.unusual(Metric::Cpu, 60.0, t).unwrap());
        assert!((x - y).abs() < 0.01, "{} {}", x, y);
        assert_eq!(b.expected_names(t), c.expected_names(t));
    }
}

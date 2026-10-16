//! The feel meter: does the PC *feel* slow right now? Measured the way a person notices it,
//! not by CPU %:
//! - ping: how long the app in front takes to answer a no-op window message (a frozen or
//!   busy UI answers late; Windows calls it "not responding" after 5 s),
//! - lag: how late a short sleep wakes up (CPU or driver starvation delays every thread),
//! - hung: Windows itself flags the front window as not responding.
//! One small thread, one sample a second, about nothing in CPU and memory.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample { pub ping_ms: f64, pub lag_ms: f64, pub hung: bool }

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Feel { pub ping_ms: f64, pub lag_ms: f64, pub hung: bool, pub samples: usize }

pub const SLUGGISH_PING_MS: f64 = 80.0;
pub const SLUGGISH_LAG_MS: f64 = 20.0;
/// The desktop compositor using this much of one core means the screen itself is behind.
pub const DWM_BUSY_PCT: f64 = 50.0;

/// The meter's view combined with what the governor measured: a pegged compositor (dwm) is
/// sluggish no matter how fast the front app answers.
pub fn word_with(f: &Feel, dwm_pct: f64) -> &'static str { if dwm_pct >= DWM_BUSY_PCT { "sluggish" } else { f.word() } }
pub fn sluggish_with(f: &Feel, dwm_pct: f64) -> bool { dwm_pct >= DWM_BUSY_PCT || f.sluggish() }
const KEEP: usize = 60;

static RING: Mutex<VecDeque<Sample>> = Mutex::new(VecDeque::new());

impl Feel {
    /// One number for "how slow it feels": front-app answer time plus scheduling lag, ms.
    pub fn ms(&self) -> f64 { if self.hung { 1000.0 } else { self.ping_ms + self.lag_ms } }
    pub fn sluggish(&self) -> bool { self.samples > 0 && (self.hung || self.ping_ms >= SLUGGISH_PING_MS || self.lag_ms >= SLUGGISH_LAG_MS) }
    pub fn word(&self) -> &'static str {
        if self.samples == 0 { "unknown" } else if self.hung { "frozen" } else if self.sluggish() { "sluggish" } else if self.ms() >= 30.0 { "ok" } else { "snappy" }
    }
}

/// Pure: average of the newest `n` samples (median-ish by trimming the single worst, so one
/// stray spike does not count as sluggish).
pub fn summarize(s: &[Sample], n: usize) -> Feel {
    let take: Vec<&Sample> = s.iter().rev().take(n).collect();
    if take.is_empty() { return Feel::default(); }
    let trim = |mut v: Vec<f64>| -> f64 { v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)); if v.len() > 3 { v.pop(); } v.iter().sum::<f64>() / v.len() as f64 };
    let hung = take.iter().filter(|x| x.hung).count() * 2 > take.len();
    Feel { ping_ms: trim(take.iter().map(|x| x.ping_ms).collect()), lag_ms: trim(take.iter().map(|x| x.lag_ms).collect()), hung, samples: take.len() }
}

/// How it has felt over the last `secs` seconds.
pub fn recent(secs: usize) -> Feel {
    let g = RING.lock().unwrap_or_else(|e| e.into_inner());
    let v: Vec<Sample> = g.iter().cloned().collect();
    summarize(&v, secs)
}

fn sample() -> Sample {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsHungAppWindow, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_NULL};
    // Scheduling lag: four 25 ms sleeps, how late on average.
    let mut late = 0.0;
    for _ in 0..4 {
        let t = Instant::now();
        std::thread::sleep(Duration::from_millis(25));
        late += (t.elapsed().as_secs_f64() * 1000.0 - 25.0).max(0.0);
    }
    let lag_ms = late / 4.0;
    let (mut ping_ms, mut hung) = (0.0, false);
    unsafe {
        let h = GetForegroundWindow();
        if !h.is_null() {
            hung = IsHungAppWindow(h) != 0;
            if !hung {
                let mut r: usize = 0;
                let t = Instant::now();
                let ok = SendMessageTimeoutW(h, WM_NULL, 0, 0, SMTO_ABORTIFHUNG | SMTO_BLOCK, 500, &mut r);
                ping_ms = if ok == 0 { 500.0 } else { t.elapsed().as_secs_f64() * 1000.0 };
            }
        }
    }
    Sample { ping_ms, lag_ms, hung }
}

/// Start the meter (once). Samples every second.
pub fn start() {
    std::thread::Builder::new().name("feel".into()).stack_size(64 * 1024).spawn(|| loop {
        let s = sample();
        if let Ok(mut g) = RING.lock() { g.push_back(s); while g.len() > KEEP { g.pop_front(); } }
        std::thread::sleep(Duration::from_millis(900));
    }).ok();
}

#[cfg(test)]
mod feel_tests {
    use super::*;
    fn s(p: f64, l: f64) -> Sample { Sample { ping_ms: p, lag_ms: l, hung: false } }
    #[test]
    fn one_spike_is_not_sluggish_but_a_trend_is() {
        let calm = vec![s(3.0, 1.0), s(4.0, 1.0), s(400.0, 2.0), s(3.0, 1.0), s(5.0, 1.0)];
        let f = summarize(&calm, 10);
        assert!(!f.sluggish(), "{:?}", f);
        assert_eq!(f.word(), "snappy");
        let slow = vec![s(150.0, 5.0), s(120.0, 8.0), s(200.0, 9.0), s(90.0, 4.0)];
        assert!(summarize(&slow, 10).sluggish());
        let lagged = vec![s(5.0, 30.0), s(5.0, 25.0), s(5.0, 40.0), s(5.0, 22.0)];
        assert!(summarize(&lagged, 10).sluggish());
        assert_eq!(summarize(&[], 10).word(), "unknown");
    }
}

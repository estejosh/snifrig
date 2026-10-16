//! The feel meter: does the PC *feel* slow right now? Measured the way a person notices it,
//! not by CPU %:
//! - ping: how long the app in front takes to answer a no-op window message (a frozen or
//!   busy UI answers late; Windows calls it "not responding" after 5 s),
//! - lag: how late a short sleep wakes up (CPU or driver starvation delays every thread),
//! - hung: Windows itself flags the front window as not responding,
//! - input: how late your mouse moves, clicks and key presses reach programs. Windows stamps
//!   every input event with the time it happened; snifrig compares that with when it arrives.
//!   Only the timing is kept: never which key, never where you clicked.
//! - desktop: the screen compositor (dwm) being pegged, measured by the governor.
//! One small thread, one sample a second, about nothing in CPU and memory.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample { pub ping_ms: f64, pub lag_ms: f64, pub hung: bool }

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Feel { pub ping_ms: f64, pub lag_ms: f64, pub hung: bool, pub samples: usize, pub input_ms: f64, pub input_events: usize }

pub const SLUGGISH_PING_MS: f64 = 80.0;
pub const SLUGGISH_LAG_MS: f64 = 20.0;
/// Input arriving this late (90th percentile, ms) is what "the mouse is slow" feels like.
pub const SLUGGISH_INPUT_MS: f64 = 50.0;
const MIN_INPUT_EVENTS: usize = 5;
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
    pub fn ms(&self) -> f64 { if self.hung { 1000.0 } else { (self.ping_ms + self.lag_ms).max(if self.input_events >= MIN_INPUT_EVENTS { self.input_ms } else { 0.0 }) } }
    pub fn input_slow(&self) -> bool { self.input_events >= MIN_INPUT_EVENTS && self.input_ms >= SLUGGISH_INPUT_MS }
    pub fn sluggish(&self) -> bool { self.input_slow() || (self.samples > 0 && (self.hung || self.ping_ms >= SLUGGISH_PING_MS || self.lag_ms >= SLUGGISH_LAG_MS)) }
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
    Feel { ping_ms: trim(take.iter().map(|x| x.ping_ms).collect()), lag_ms: trim(take.iter().map(|x| x.lag_ms).collect()), hung, samples: take.len(), input_ms: 0.0, input_events: 0 }
}

/// How it has felt over the last `secs` seconds.
pub fn recent(secs: usize) -> Feel {
    let v: Vec<Sample> = { let g = RING.lock().unwrap_or_else(|e| e.into_inner()); g.iter().cloned().collect() };
    let mut f = summarize(&v, secs);
    let (n, p90) = input_recent(secs.max(10) as u32);
    f.input_events = n;
    f.input_ms = p90;
    f
}

// ---------- input delay (low-level mouse and keyboard hooks, timing only) ----------

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
const IN_N: usize = 512;
static IN_DELAY: [AtomicU32; IN_N] = [const { AtomicU32::new(0) }; IN_N];
static IN_AT: [AtomicU32; IN_N] = [const { AtomicU32::new(0) }; IN_N];
static IN_IDX: AtomicUsize = AtomicUsize::new(0);
static LAST_MOVE: AtomicU32 = AtomicU32::new(0);

fn tick() -> u32 { unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() } }

fn record(event_time: u32) {
    let now = tick();
    let d = now.wrapping_sub(event_time);
    if d > 10_000 { return; } // clock wrap or a replayed event: not a real delay
    let i = IN_IDX.fetch_add(1, Ordering::Relaxed) % IN_N;
    IN_DELAY[i].store(d, Ordering::Relaxed);
    IN_AT[i].store(now, Ordering::Relaxed);
}

/// Pure: (events, 90th-percentile delay ms) of the delays recorded in the last window.
pub fn percentile90(mut d: Vec<u32>) -> (usize, f64) {
    if d.is_empty() { return (0, 0.0); }
    d.sort_unstable();
    let i = ((d.len() as f64 * 0.9).ceil() as usize).saturating_sub(1).min(d.len() - 1);
    (d.len(), d[i] as f64)
}

fn input_recent(secs: u32) -> (usize, f64) {
    let now = tick();
    let mut d = Vec::new();
    for i in 0..IN_N {
        let at = IN_AT[i].load(Ordering::Relaxed);
        if at != 0 && now.wrapping_sub(at) <= secs * 1000 { d.push(IN_DELAY[i].load(Ordering::Relaxed)); }
    }
    percentile90(d)
}

unsafe extern "system" fn mouse_hook(code: i32, wp: usize, lp: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallNextHookEx, MSLLHOOKSTRUCT, WM_MOUSEMOVE};
    if code >= 0 && lp != 0 {
        let info = &*(lp as *const MSLLHOOKSTRUCT);
        let now = tick();
        // Every click counts; moves are sampled at most every 50 ms to stay cheap.
        if wp as u32 != WM_MOUSEMOVE || now.wrapping_sub(LAST_MOVE.load(Ordering::Relaxed)) >= 50 {
            if wp as u32 == WM_MOUSEMOVE { LAST_MOVE.store(now, Ordering::Relaxed); }
            record(info.time);
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wp, lp)
}

unsafe extern "system" fn key_hook(code: i32, wp: usize, lp: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallNextHookEx, KBDLLHOOKSTRUCT};
    // Only the event's timestamp is read. The key itself is never looked at or stored.
    if code >= 0 && lp != 0 { record((*(lp as *const KBDLLHOOKSTRUCT)).time); }
    CallNextHookEx(std::ptr::null_mut(), code, wp, lp)
}

/// A dedicated thread that does nothing but receive input timing, so it never delays input.
fn start_input() {
    std::thread::Builder::new().name("feel-input".into()).stack_size(64 * 1024).spawn(|| unsafe {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, MSG, WH_KEYBOARD_LL, WH_MOUSE_LL};
        let h = GetModuleHandleW(std::ptr::null());
        let m = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), h, 0);
        let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), h, 0);
        if m.is_null() && k.is_null() { return; }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 { TranslateMessage(&msg); DispatchMessageW(&msg); }
    }).ok();
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

static APPS: Mutex<Vec<(u32, f64, bool)>> = Mutex::new(Vec::new());

/// Every window on a screen, how fast it answered on the latest round: (pid, ms, not responding).
pub fn apps() -> Vec<(u32, f64, bool)> { APPS.lock().map(|g| g.clone()).unwrap_or_default() }

fn ping_window(h: isize) -> (f64, bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsHungAppWindow, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_NULL};
    let h = h as windows_sys::Win32::Foundation::HWND;
    unsafe {
        if IsHungAppWindow(h) != 0 { return (1000.0, true); }
        let mut r: usize = 0;
        let t = Instant::now();
        let ok = SendMessageTimeoutW(h, WM_NULL, 0, 0, SMTO_ABORTIFHUNG | SMTO_BLOCK, 500, &mut r);
        (if ok == 0 { 500.0 } else { t.elapsed().as_secs_f64() * 1000.0 }, false)
    }
}

/// Start the meter (once). Samples every second; input timing as it happens.
pub fn start() {
    start_input();
    std::thread::Builder::new().name("feel".into()).stack_size(64 * 1024).spawn(|| loop {
        let s = sample();
        if let Ok(mut g) = RING.lock() { g.push_back(s); while g.len() > KEEP { g.pop_front(); } }
        std::thread::sleep(Duration::from_millis(900));
    }).ok();
    // Every window on a screen, every 3 seconds, on its own thread: a hung window cannot stall the loop.
    std::thread::Builder::new().name("feel-apps".into()).stack_size(64 * 1024).spawn(|| loop {
        let v: Vec<(u32, f64, bool)> = crate::sys::visible_windows().into_iter().map(|(h, pid)| { let (ms, hung) = ping_window(h); (pid, ms, hung) }).collect();
        if let Ok(mut g) = APPS.lock() { *g = v; }
        std::thread::sleep(Duration::from_secs(3));
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
        assert_eq!(percentile90(vec![5, 6, 7, 8, 9, 10, 11, 12, 13, 400]), (10, 13.0));
        let mut f = summarize(&calm, 10);
        f.input_events = 20; f.input_ms = 120.0;
        assert!(f.sluggish() && f.ms() >= 120.0, "late input alone means sluggish");
        f.input_events = 2;
        assert!(!f.sluggish(), "two events are not enough to judge");
    }
}

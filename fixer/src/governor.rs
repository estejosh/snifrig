//! The governor: keeps the PC responsive when the CPU is pegged, like Process Lasso's
//! ProBalance, but gentler and self-explaining.
//!
//! Every tick (1 s) it measures CPU per process. When the machine is busy (total >= BUSY_PCT
//! for BUSY_TICKS ticks), it demotes the biggest background CPU users one step at a time:
//!   level 1: efficiency mode (EcoQoS) + low I/O priority
//!   level 2: below-normal priority
//!   level 3: idle priority + low memory priority
//! It never touches the foreground app (or other processes with its image name), the
//! never-touch list (policy::denied), our own processes, or anything a user rule pins.
//! When the machine is calm (total < CALM_PCT for CALM_TICKS ticks) it restores one step per
//! RESTORE_EVERY ticks; a demoted process that comes to the foreground is restored at once.
//! Every change is recorded (ledger) with the reason in plain words, and every change is
//! re-checked against pid + create time + name right before it is made.
//!
//! Pure decision logic lives in `decide` so it can be unit-tested without Windows.

use crate::policy;
use crate::sys::{self, Proc};
use crate::{Action, Target};
use std::collections::{HashMap, HashSet};

pub const TICK_SECS: f64 = 1.0;
pub const BUSY_PCT: f64 = 75.0;
pub const BUSY_TICKS: u32 = 3;
pub const CALM_PCT: f64 = 50.0;
pub const CALM_TICKS: u32 = 10;
pub const RESTORE_EVERY: u32 = 5;
/// A process must use at least this share of ONE core to be worth demoting.
pub const MIN_CORE_PCT: f64 = 25.0;
pub const MAX_DEMOTIONS_PER_TICK: usize = 2;
pub const MAX_LEVEL: u8 = 3;
/// CPU this high counts as busy even when the feel meter says the PC still feels fast.
pub const NEAR_MAX_PCT: f64 = 92.0;

/// Governor settings, from `fix-governor.txt` (key=value per line, '#' comments). Defaults are the consts above.
#[derive(Clone, Debug, PartialEq)]
pub struct Tuning {
    pub busy_pct: f64,
    pub busy_ticks: u32,
    pub calm_pct: f64,
    pub calm_ticks: u32,
    pub restore_every: u32,
    pub min_core_pct: f64,
    pub max_per_tick: usize,
    pub max_level: u8,
    /// Performance mode: when a game is in front, switch power plan and keep background quieter.
    pub game_mode: bool,
    /// Power plan for performance mode: "auto" (Ultimate, else High performance), "none", or a plan GUID.
    pub game_plan: String,
    /// Busy threshold while a game runs.
    pub game_busy_pct: f64,
    /// How often rules are re-checked and re-applied when an app undoes them (seconds, 0 = never).
    pub reapply_secs: u32,
    /// Only step in when the PC also *feels* slow (feel meter), unless CPU is nearly maxed.
    pub feel_gate: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { busy_pct: BUSY_PCT, busy_ticks: BUSY_TICKS, calm_pct: CALM_PCT, calm_ticks: CALM_TICKS, restore_every: RESTORE_EVERY,
            min_core_pct: MIN_CORE_PCT, max_per_tick: MAX_DEMOTIONS_PER_TICK, max_level: MAX_LEVEL,
            game_mode: true, game_plan: "auto".into(), game_busy_pct: 50.0, reapply_secs: 30, feel_gate: true }
    }
}

pub const TUNING_FILE: &str = "fix-governor.txt";

/// Written once if the file is missing, so the settings are easy to find. Every line is a comment.
pub const TUNING_TEMPLATE: &str = "# snifrig-fix governor settings. Remove the '#' in front of a line to change it.\r\n\
# The fixer re-reads this file within 30 seconds of a save.\r\n\
# busy_pct = 75            # total CPU that counts as busy\r\n\
# busy_secs = 3            # ...for this many seconds before the governor steps in\r\n\
# calm_pct = 50            # total CPU that counts as calm again\r\n\
# calm_secs = 10           # ...for this long before anything is put back\r\n\
# restore_every_secs = 5   # then one step back every this many seconds\r\n\
# min_core_pct = 25        # only ease apps using at least this much of one core\r\n\
# max_per_tick = 2         # apps eased per second at most\r\n\
# max_level = 3            # 1 efficiency mode, 2 below normal, 3 idle\r\n\
# game_mode = on           # performance mode when a game is in front\r\n\
# game_plan = auto         # auto (Ultimate, else High performance), none, or a plan name or GUID\r\n\
# game_busy_pct = 50       # busy threshold while a game runs\r\n\
# reapply_secs = 30        # re-check rules this often and undo apps that reset them (0 = never)\r\n\
# feel_gate = on           # only step in when the PC also feels slow (or CPU is nearly maxed)\r\n";

impl Tuning {
    /// Parse the settings file. Unknown keys and bad values are reported, and the default is kept.
    pub fn parse(text: &str) -> (Tuning, Vec<String>) {
        let mut t = Tuning::default();
        let mut errs = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let l = raw.split('#').next().unwrap_or("").trim();
            if l.is_empty() { continue; }
            let Some((k, v)) = l.split_once('=') else { errs.push(format!("line {}: '{}' is not key=value", i + 1, l)); continue };
            let (k, v) = (k.trim().to_lowercase(), v.trim().to_lowercase());
            let pct = |v: &str| v.trim_end_matches('%').parse::<f64>().ok().filter(|x| (1.0..=100.0).contains(x));
            let ok = match k.as_str() {
                "busy_pct" => pct(&v).map(|x| t.busy_pct = x).is_some(),
                "calm_pct" => pct(&v).map(|x| t.calm_pct = x).is_some(),
                "game_busy_pct" => pct(&v).map(|x| t.game_busy_pct = x).is_some(),
                "busy_secs" => v.parse::<u32>().ok().filter(|x| *x >= 1).map(|x| t.busy_ticks = x).is_some(),
                "calm_secs" => v.parse::<u32>().ok().filter(|x| *x >= 1).map(|x| t.calm_ticks = x).is_some(),
                "restore_every_secs" => v.parse::<u32>().ok().filter(|x| *x >= 1).map(|x| t.restore_every = x).is_some(),
                "min_core_pct" => v.trim_end_matches('%').parse::<f64>().ok().filter(|x| *x >= 1.0).map(|x| t.min_core_pct = x).is_some(),
                "max_per_tick" => v.parse::<usize>().ok().filter(|x| (1..=16).contains(x)).map(|x| t.max_per_tick = x).is_some(),
                "max_level" => v.parse::<u8>().ok().filter(|x| *x <= MAX_LEVEL).map(|x| t.max_level = x).is_some(),
                "game_mode" => match v.as_str() { "on" | "yes" => { t.game_mode = true; true } "off" | "no" => { t.game_mode = false; true } _ => false },
                "game_plan" => { t.game_plan = v.clone(); true }
                "reapply_secs" => v.parse::<u32>().ok().map(|x| t.reapply_secs = x).is_some(),
                "feel_gate" => match v.as_str() { "on" | "yes" => { t.feel_gate = true; true } "off" | "no" => { t.feel_gate = false; true } _ => false },
                _ => { errs.push(format!("line {}: unknown setting '{}'", i + 1, k)); continue; }
            };
            if !ok { errs.push(format!("line {}: bad value for {}: '{}'", i + 1, k, v)); }
        }
        if t.calm_pct > t.busy_pct { errs.push("calm_pct above busy_pct; using defaults for both".into()); t.calm_pct = CALM_PCT; t.busy_pct = BUSY_PCT; }
        (t, errs)
    }
    pub fn load(dir: &std::path::Path) -> (Tuning, Vec<String>) { Tuning::parse(&std::fs::read_to_string(dir.join(TUNING_FILE)).unwrap_or_default()) }
}

type Key = (u32, u64); // pid, create time: survives pid reuse

#[derive(Clone, Debug, PartialEq)]
pub struct Demotion {
    pub pid: u32,
    pub created: u64,
    pub name: String,
    pub level: u8,
    /// An app you asked to keep fast: its only "level" is below-normal priority (no efficiency
    /// mode, no slow cores, no idle), so it gives way when the PC is swamped but never crawls.
    pub gentle: bool,
    pub orig_priority: u32,
    pub since: f64,
    pub reason: String,
}

/// What the governor will do this tick. Pure output of `decide`.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Demote { key: Key, name: String, to_level: u8, reason: String },
    Restore { key: Key, name: String, to_level: u8, reason: String },
    Forget { key: Key },
}

/// One process as seen this tick.
#[derive(Clone, Debug)]
pub struct Seen { pub key: Key, pub name: String, pub core_pct: f64, pub foreground: bool, pub protected: bool, pub windowed: bool,
    /// Has a window showing on a monitor (another screen, or beside the app in front): you can see it, so it stays smooth.
    pub onscreen: bool }

/// Counters that carry between ticks.
#[derive(Clone, Debug, Default)]
pub struct Mood { pub busy_ticks: u32, pub calm_ticks: u32 }

impl Mood {
    /// `feels_slow`: the feel meter says the PC is sluggish right now.
    pub fn update(&mut self, total_pct: f64, t: &Tuning, game: bool, feels_slow: bool) {
        let busy_at = if game { t.game_busy_pct.min(t.busy_pct) } else { t.busy_pct };
        // High CPU alone is not a problem if the PC still feels fast: leave apps alone then.
        let hurts = !t.feel_gate || feels_slow || total_pct >= NEAR_MAX_PCT;
        if total_pct >= busy_at && hurts { self.busy_ticks += 1; } else { self.busy_ticks = 0; }
        if total_pct < t.calm_pct.min(busy_at) { self.calm_ticks += 1; } else { self.calm_ticks = 0; }
    }
    pub fn busy_with(&self, t: &Tuning) -> bool { self.busy_ticks >= t.busy_ticks }
    pub fn calm_with(&self, t: &Tuning) -> bool { self.calm_ticks >= t.calm_ticks }
    pub fn busy(&self) -> bool { self.busy_with(&Tuning::default()) }
}

/// What the learner knows, as plain data so `decide_with` stays pure. Names are lowercase.
#[derive(Clone, Debug)]
pub struct Hints {
    /// Names the user keeps undoing: never demoted.
    pub protect: HashSet<String>,
    /// Names demoting did not help for (enough tries, low success chance): never demoted.
    pub blocked: HashSet<String>,
    /// Chance that demoting this name helps; others get `default_prob`.
    pub prob: HashMap<String, f64>,
    pub default_prob: f64,
    /// Extra weight from the local brain's view of what the program is (1.0 = neutral).
    pub boost: HashMap<String, f64>,
    /// Deepest level the brain allows for this name (default MAX_LEVEL).
    pub max_level: HashMap<String, u8>,
    /// Apps you said you use and want fast (rule keep=yes): untouched while on a screen,
    /// only a lower priority while hidden.
    pub keep: HashSet<String>,
    /// Game mode: the game in front (lowercase), while performance mode is on. Everything else
    /// is turned down at once, without waiting for the PC to get busy.
    pub game_front: Option<String>,
    /// Programs that made your games stutter (from your answers): turned down hardest in game mode.
    pub game_ease: HashSet<String>,
    /// Thresholds in force (the Governor copies its settings in before deciding).
    pub tuning: Tuning,
}

impl Default for Hints {
    fn default() -> Self { Hints { protect: HashSet::new(), blocked: HashSet::new(), prob: HashMap::new(), default_prob: 1.0, boost: HashMap::new(), max_level: HashMap::new(), keep: HashSet::new(), game_front: None, game_ease: HashSet::new(), tuning: Tuning::default() } }
}

/// Pure policy without learning hints.
pub fn decide(total_pct: f64, mood: &Mood, seen: &[Seen], demoted: &HashMap<Key, Demotion>) -> Vec<Step> {
    decide_with(total_pct, mood, seen, demoted, &Hints::default())
}

/// Pure policy. `seen` is every live process this tick; `demoted` the current demotions.
pub fn decide_with(total_pct: f64, mood: &Mood, seen: &[Seen], demoted: &HashMap<Key, Demotion>, hints: &Hints) -> Vec<Step> {
    let mut out = Vec::new();
    let live: HashMap<Key, &Seen> = seen.iter().map(|s| (s.key, s)).collect();
    let fg_names: Vec<&str> = seen.iter().filter(|s| s.foreground).map(|s| s.name.as_str()).collect();
    // Apps showing on a screen, by name: their windowless helpers (renderers, GPU process) draw
    // the window you can see, so they get the same light touch as the window itself.
    let on_names: Vec<&str> = seen.iter().filter(|s| s.onscreen).map(|s| s.name.as_str()).collect();
    let t = &hints.tuning;

    // Restores first: gone, foreground, or calm.
    for (k, d) in demoted {
        match live.get(k) {
            None => out.push(Step::Forget { key: *k }),
            Some(s) if s.foreground || (fg_names.contains(&s.name.as_str()) && s.windowed) =>
                out.push(Step::Restore { key: *k, name: d.name.clone(), to_level: 0, reason: format!("{} is now the app you are using", d.name) }),
            // You asked to keep this app fast: on a screen it gets everything back; hidden, an old
            // (not gentle) easing is put back so only the gentle one can apply.
            Some(s) if hints.keep.contains(&s.name.to_lowercase()) && (s.onscreen || on_names.contains(&s.name.as_str()) || fg_names.contains(&s.name.as_str()) || !d.gentle) =>
                out.push(Step::Restore { key: *k, name: d.name.clone(), to_level: 0, reason: format!("you asked snifrig to keep {} fast", d.name) }),
            // A windowless helper (a browser tab, a renderer) of the app in front may stay reined in, but only lightly.
            Some(s) if fg_names.contains(&s.name.as_str()) && d.level > 1 =>
                out.push(Step::Restore { key: *k, name: d.name.clone(), to_level: 1, reason: format!("{} is in front, so its busy background part is only kept in efficiency mode", d.name) }),
            Some(s) if (s.onscreen || on_names.contains(&s.name.as_str())) && d.level > 1 =>
                out.push(Step::Restore { key: *k, name: d.name.clone(), to_level: 1, reason: format!("{} is showing on a screen, so it is only kept in efficiency mode", d.name) }),
            // The game is over: everything game mode turned down comes back at once.
            Some(_) if hints.game_front.is_none() && d.reason.starts_with("game mode") =>
                out.push(Step::Restore { key: *k, name: d.name.clone(), to_level: 0, reason: "the game is over".into() }),
            Some(_) if hints.game_front.is_some() && d.reason.starts_with("game mode") => {}
            Some(_) if mood.calm_with(t) && mood.calm_ticks % t.restore_every.max(1) == 0 =>
                out.push(Step::Restore { key: *k, name: d.name.clone(), to_level: d.level - 1, reason: format!("CPU is calm again ({:.0}%)", total_pct) }),
            _ => {}
        }
    }
    let game = hints.game_front.as_deref();
    if !mood.busy_with(t) && game.is_none() { return out; }

    // Demote the biggest background users, one step each, a couple per tick.
    let mut cands: Vec<&Seen> = seen.iter()
        // The app in front is never touched; its windowless helpers (busy tabs, renderers) may be reined in lightly.
        .filter(|s| !s.protected && !s.foreground && (!fg_names.contains(&s.name.as_str()) || !s.windowed))
        .filter(|s| s.core_pct >= if game.is_some() { 2.0 } else { t.min_core_pct })
        .filter(|s| game.map_or(true, |g| !s.name.eq_ignore_ascii_case(g)))
        .filter(|s| { let n = s.name.to_lowercase(); !hints.protect.contains(&n) && !hints.blocked.contains(&n) })
        .filter(|s| {
            let mut cap = hints.max_level.get(&s.name.to_lowercase()).copied().unwrap_or(t.max_level).min(t.max_level);
            // Game mode: everything else to below normal; what made your games stutter, all the way.
            if game.is_some() && !hints.game_ease.contains(&s.name.to_lowercase()) { cap = cap.min(2); }
            // Helpers of the app in front, and apps showing on a screen, only get efficiency mode.
            if fg_names.contains(&s.name.as_str()) || on_names.contains(&s.name.as_str()) { cap = cap.min(1); }
            if hints.keep.contains(&s.name.to_lowercase()) {
                cap = if s.onscreen || fg_names.contains(&s.name.as_str()) || on_names.contains(&s.name.as_str()) { 0 } else { cap.min(1) };
            }
            demoted.get(&s.key).map_or(0, |d| d.level) < cap
        })
        .collect();
    // Biggest user first, weighted by how often demoting that name has helped.
    let weight = |s: &Seen| { let n = s.name.to_lowercase();
        s.core_pct * hints.prob.get(&n).copied().unwrap_or(hints.default_prob) * hints.boost.get(&n).copied().unwrap_or(1.0) };
    cands.sort_by(|a, b| weight(b).partial_cmp(&weight(a)).unwrap_or(std::cmp::Ordering::Equal));
    for s in cands.into_iter().take(if game.is_some() { t.max_per_tick * 3 } else { t.max_per_tick }) {
        let lvl = demoted.get(&s.key).map_or(0, |d| d.level) + 1;
        let reason = if let Some(g) = game {
            format!("game mode: {} is in front, so {} is turned down until you stop playing", g, s.name)
        } else if fg_names.contains(&s.name.as_str()) {
            format!("CPU at {:.0}% and a background part of {} (a tab or helper, pid {}) was using {:.0}% of a core; the window you are using is untouched", total_pct, s.name, s.key.0, s.core_pct)
        } else {
            format!("CPU at {:.0}% and {} was using {:.0}% of a core in the background", total_pct, s.name, s.core_pct)
        };
        out.push(Step::Demote { key: s.key, name: s.name.clone(), to_level: lvl, reason });
    }
    out
}

/// At start: put back anything a previous fixer eased and never restored (it crashed or was
/// killed). Only processes that are still the same process (pid + start time + name).
pub fn recover(path: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    let mut out = Vec::new();
    for l in text.lines() {
        let f: Vec<&str> = l.splitn(6, ' ').collect();
        if f.len() < 6 { continue; }
        let (Ok(pid), Ok(created), Ok(level), Ok(orig), Ok(g)) = (f[0].parse::<u32>(), f[1].parse::<u64>(), f[2].parse::<u8>(), f[3].parse::<u32>(), f[4].parse::<u8>()) else { continue };
        if level == 0 || !sys::image_matches(pid, created, f[5]) { continue; }
        if apply_level(pid, level, 0, orig, g != 0).is_ok() {
            out.push(format!("put {} (pid {}) back to normal: the last snifrig-fix eased it and stopped before it could", f[5], pid));
        }
    }
    out
}

/// Apply one level change to a live process. Level 0 = original state.
fn apply_level(pid: u32, from: u8, to: u8, orig_priority: u32, gentle: bool) -> Result<(), String> {
    if gentle {
        if from < 1 && to >= 1 { sys::set_priority(pid, sys::BELOW_NORMAL)?; }
        if from >= 1 && to < 1 { sys::set_priority(pid, orig_priority)?; }
        return Ok(());
    }
    // Going up the ladder.
    if from < 1 && to >= 1 { sys::set_efficiency(pid, true)?; let _ = sys::set_io_priority(pid, 1); }
    if from < 2 && to >= 2 {
        sys::set_priority(pid, sys::BELOW_NORMAL)?;
        // Hybrid CPUs: an app turned this far down moves to the slow cores, leaving the fast ones to you.
        if let Some(e) = sys::efficiency_cpu_sets() { let _ = sys::set_cpu_sets(pid, e); }
    }
    if from < 3 && to >= 3 { sys::set_priority(pid, sys::IDLE)?; let _ = sys::set_memory_priority(pid, 2); }
    // Coming down.
    if from >= 3 && to < 3 { let _ = sys::set_memory_priority(pid, 5); sys::set_priority(pid, if to >= 2 { sys::BELOW_NORMAL } else { orig_priority })?; }
    if from >= 2 && to < 2 {
        if sys::efficiency_cpu_sets().is_some() { let _ = sys::set_cpu_sets(pid, &[]); }
        sys::set_priority(pid, orig_priority)?;
    }
    if from >= 1 && to < 1 { let _ = sys::set_io_priority(pid, 2); sys::set_efficiency(pid, false)?; }
    Ok(())
}

pub struct Governor {
    buf: Vec<u8>,
    prev: HashMap<Key, (u64, f64)>, // cpu time, wall time
    pub mood: Mood,
    pub demoted: HashMap<Key, Demotion>,
    protected_cache: HashMap<Key, bool>,
    cores: f64,
    extra_deny: Vec<String>,
    pinned: Vec<String>, // image names a user rule manages; the governor leaves them alone
    rules: Vec<crate::rules::Rule>, // user rules; any process a rule covers (wildcards too) is pinned
    undone: HashSet<Key>, // processes the user restored by hand; left alone from then on
    last_procs: Vec<Proc>, // the snapshot of the latest tick, for the rule enforcer
    /// Total CPU use measured on the latest tick (0-100).
    pub total_pct: f64,
    /// CPU (share of one core) of the foreground app on the latest tick, if there is one.
    pub fg_cpu_pct: Option<f64>,
    /// Busiest background-or-not processes of the latest tick (lowercase name, core %), biggest first, at most 5.
    pub top: Vec<(String, f64)>,
    /// What the learner suggests; refreshed by the watch loop.
    pub hints: Hints,
    /// Whether current demotions were really applied (false = dry run bookkeeping only).
    pub acting: bool,
    /// Settings from fix-governor.txt.
    pub tuning: Tuning,
    /// Lowercase image name of the game performance mode is running for, if any.
    pub game: Option<String>,
    windowed: HashSet<u32>,
    onscreen: HashSet<u32>,
    win_at: f64,
    /// Set by the watch loop from the feel meter before each tick.
    pub feels_slow: bool,
    /// CPU (share of one core) of the desktop compositor on the latest tick. When dwm is pegged,
    /// the mouse and every window stutter even though each app still answers instantly.
    pub dwm_pct: f64,
    /// What you told snifrig from the tray: Some(true) the PC feels slow (ease background apps
    /// now, even below the CPU threshold), Some(false) it feels fine (hold off).
    pub user_says: Option<bool>,
    /// Quick fix: until this time, a calm CPU does not bring eased apps back (they come back when you switch to them).
    pub hold_until: f64,
    /// Share of a core each pid used on the latest tick.
    last_pct: HashMap<u32, f64>,
    /// Apps that feel slow right now: (name, ms, usual ms, reason). Set by the watch loop.
    pub apps_slow: Vec<(String, f64, f64, String)>,
    /// Names of apps with a window on a screen (for the tray's "an app feels slow" list).
    pub visible: Vec<String>,
}

/// One change made, for the ledger and the tray.
pub struct Change { pub pid: u32, pub name: String, pub from: u8, pub to: u8, pub reason: String, pub result: Result<(), String> }

impl Governor {
    pub fn new(extra_deny: Vec<String>, pinned: Vec<String>) -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
        Governor { buf: Vec::new(), prev: HashMap::new(), mood: Mood::default(), demoted: HashMap::new(),
            protected_cache: HashMap::new(), cores, extra_deny, pinned, rules: Vec::new(), undone: HashSet::new(), last_procs: Vec::new(), total_pct: 0.0, fg_cpu_pct: None, top: Vec::new(), hints: Hints::default(), acting: false, tuning: Tuning::default(), game: None, windowed: HashSet::new(), onscreen: HashSet::new(), win_at: 0.0, feels_slow: false, dwm_pct: 0.0, user_says: None, hold_until: 0.0, last_pct: HashMap::new(), apps_slow: Vec::new(), visible: Vec::new() }
    }

    fn protected(&mut self, p: &Proc) -> bool {
        let key = (p.pid, p.created);
        if let Some(&b) = self.protected_cache.get(&key) { return b; }
        let name = p.name.to_lowercase();
        let mut b = self.pinned.iter().any(|x| *x == name) || crate::rules::is_pinned(&self.rules, &name);
        if !b {
            // Only read the command line for real candidates; it is the expensive part.
            let t = Target { pid: p.pid, name: p.name.clone(), age_secs: f64::MAX, cmdline: sys::cmdline(p.pid), service: None, private_mb: 0.0 };
            b = policy::denied(&t, &Action::LowerPriority, &self.extra_deny).is_some();
        }
        self.protected_cache.insert(key, b);
        b
    }

    /// One tick: measure, decide, act. Returns the changes made (empty most of the time).
    /// `act` = false logs decisions without changing anything (dry run / paused).
    pub fn tick(&mut self, now: f64, act: bool) -> Vec<Change> {
        let procs = sys::snapshot(&mut self.buf);
        let fg = sys::foreground_pid();
        if now - self.win_at >= 5.0 { (self.windowed, self.onscreen) = sys::windows(); self.win_at = now; }
        let mut seen = Vec::with_capacity(procs.len());
        let mut busy_sum = 0.0;
        let mut next_prev = HashMap::with_capacity(procs.len());
        for p in &procs {
            let key = (p.pid, p.created);
            let core_pct = match self.prev.get(&key) {
                Some(&(c0, t0)) if now > t0 => (p.cpu.saturating_sub(c0) as f64 / 1e7) / (now - t0) * 100.0,
                _ => 0.0,
            };
            next_prev.insert(key, (p.cpu, now));
            if p.pid == 0 { continue; } // idle
            busy_sum += core_pct;
            let foreground = Some(p.pid) == fg;
            if foreground { crate::inuse::note_foreground(&p.name, now); }
            // Cheap filter before the protected check, which may read a command line.
            let protected = if core_pct >= self.tuning.min_core_pct || self.demoted.contains_key(&key) { self.protected(p) } else { true };
            let protected = protected || self.undone.contains(&key);
            seen.push(Seen { key, name: p.name.clone(), core_pct, foreground, protected, windowed: self.windowed.contains(&p.pid), onscreen: self.onscreen.contains(&p.pid) });
        }
        self.last_pct = seen.iter().map(|s| (s.key.0, s.core_pct)).collect();
        self.prev = next_prev;
        self.protected_cache.retain(|k, _| self.prev.contains_key(k));
        self.undone.retain(|k| self.prev.contains_key(k));
        let total_pct = (busy_sum / self.cores).min(100.0);
        self.total_pct = total_pct;
        self.mood.update(total_pct, &self.tuning, self.game.is_some(), self.feels_slow);
        match self.user_says {
            Some(true) => self.mood.busy_ticks = self.mood.busy_ticks.max(self.tuning.busy_ticks),
            Some(false) => self.mood.busy_ticks = 0,
            None => {}
        }
        self.fg_cpu_pct = seen.iter().find(|s| s.foreground).map(|s| s.core_pct);
        let dwm_now: f64 = seen.iter().filter(|s| s.name.eq_ignore_ascii_case("dwm.exe")).map(|s| s.core_pct).sum();
        // Smoothed over ~10 s: a one-second redraw burst is normal, a pegged compositor is not.
        self.dwm_pct = self.dwm_pct * 0.9 + dwm_now * 0.1;
        let mut top: Vec<(String, f64)> = seen.iter().filter(|s| s.core_pct >= 5.0).map(|s| (s.name.to_lowercase(), s.core_pct)).collect();
        top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        top.truncate(5);
        self.top = top;

        let mut changes = Vec::new();
        self.hints.tuning = self.tuning.clone();
        // Rules with governor=on / max_level=n let the governor manage that app, but only so far.
        for s in &seen {
            let n = s.name.to_lowercase();
            if let Some(m) = crate::rules::rule_max_level(&self.rules, &n) { self.hints.max_level.insert(n.clone(), m); }
        }
        self.hints.keep = seen.iter().map(|s| s.name.to_lowercase()).filter(|n| crate::rules::rule_keeps(&self.rules, n)).collect();
        if let Some(g) = &self.game { self.hints.protect.insert(g.clone()); }
        for step in decide_with(total_pct, &self.mood, &seen, &self.demoted, &self.hints) {
            match step {
                Step::Forget { key } => { self.demoted.remove(&key); }
                Step::Demote { key, name, to_level, reason } => {
                    let from = self.demoted.get(&key).map_or(0, |d| d.level);
                    let orig = self.demoted.get(&key).map(|d| d.orig_priority).or_else(|| sys::get_priority(key.0)).unwrap_or(sys::NORMAL);
                    let gentle = self.demoted.get(&key).map(|d| d.gentle).unwrap_or_else(|| self.hints.keep.contains(&name.to_lowercase()));
                    let result = if !act { Ok(()) } else if !sys::image_matches(key.0, key.1, &name) { Err("pid reused".into()) } else { apply_level(key.0, from, to_level, orig, gentle) };
                    if result.is_ok() { // dry run tracks levels too, so it logs each step once, not every tick
                        let e = self.demoted.entry(key).or_insert(Demotion { pid: key.0, created: key.1, name: name.clone(), level: 0, orig_priority: orig, since: now, reason: reason.clone(), gentle });
                        e.level = to_level; e.reason = reason.clone();
                    }
                    // A process we cannot open or verify (Windows Update, protected services) is left
                    // alone from now on instead of being retried and logged as a failure every second.
                    if act && from == 0 && result.is_err() { self.undone.insert(key); self.demoted.remove(&key); continue; }
                    changes.push(Change { pid: key.0, name, from, to: to_level, reason, result });
                }
                Step::Restore { key, name, to_level, reason } => {
                    if now < self.hold_until && reason.starts_with("CPU is calm") { continue; }
                    let Some(d) = self.demoted.get(&key).cloned() else { continue };
                    let result = if !act { Ok(()) } else if !sys::image_matches(key.0, key.1, &name) { Err("pid reused".into()) } else { apply_level(key.0, d.level, to_level, d.orig_priority, d.gentle) };
                    if to_level == 0 { self.demoted.remove(&key); } else if let Some(e) = self.demoted.get_mut(&key) { e.level = to_level; }
                    changes.push(Change { pid: key.0, name, from: d.level, to: to_level, reason, result });
                }
            }
        }
        self.last_procs = procs;
        changes
    }

    /// Replace the user rules (processes they cover become pinned).
    pub fn set_rules(&mut self, rules: Vec<crate::rules::Rule>) { self.rules = rules; self.protected_cache.clear(); }

    /// The process snapshot taken by the latest tick.
    pub fn procs(&self) -> &[Proc] { &self.last_procs }

    /// Restore every demotion of this pid fully, and leave it alone from now on.
    pub fn undo(&mut self, pid: u32, act: bool) -> Vec<Change> {
        let keys: Vec<Key> = self.demoted.keys().filter(|k| k.0 == pid).cloned().collect();
        let mut out = Vec::new();
        for k in keys {
            let Some(d) = self.demoted.remove(&k) else { continue };
            self.undone.insert(k);
            let result = if !act { Ok(()) } else if !sys::image_matches(k.0, k.1, &d.name) { Err("pid reused".into()) } else { apply_level(k.0, d.level, 0, d.orig_priority, d.gentle) };
            out.push(Change { pid, name: d.name, from: d.level, to: 0, reason: "you asked to undo it".into(), result });
        }
        out
    }

    /// Take over a fix the trust ladder runs by itself, so it behaves like any other demotion:
    /// restored the moment the app comes to the front, undoable, put back when snifrig stops.
    /// None = nothing to do (gone, in front, undone by you, or already that low).
    pub fn adopt(&mut self, pid: u32, level: u8, reason: &str, act: bool) -> Option<Change> {
        let p = self.last_procs.iter().find(|p| p.pid == pid)?;
        let (key, name) = ((p.pid, p.created), p.name.clone());
        if Some(pid) == sys::foreground_pid() || self.undone.contains(&key) { return None; }
        let level = if self.onscreen.contains(&pid) { level.min(1) } else { level };
        let gentle = self.hints.keep.contains(&name.to_lowercase()) || crate::rules::rule_keeps(&self.rules, &name.to_lowercase());
        if gentle && self.onscreen.contains(&pid) { return None; }
        let level = if gentle { level.min(1) } else { level };
        let from = self.demoted.get(&key).map_or(0, |d| d.level);
        if from >= level { return None; }
        let orig = self.demoted.get(&key).map(|d| d.orig_priority).or_else(|| sys::get_priority(pid)).unwrap_or(sys::NORMAL);
        let result = if act { apply_level(pid, from, level, orig, gentle) } else { Ok(()) };
        if result.is_ok() { self.demoted.insert(key, Demotion { pid, created: key.1, name: name.clone(), level, orig_priority: orig, since: crate::now_unix(), reason: reason.into(), gentle }); }
        Some(Change { pid, name, from, to: level, reason: reason.into(), result })
    }

    /// Quick fix, for a full slowdown: every busy background app goes to the bottom of the ladder
    /// at once (apps showing on a screen only to efficiency mode), the app in front and snifrig
    /// itself are left alone, and nothing comes back on its own for `hold_secs`. Undo and
    /// switching to an app work as always.
    pub fn quick(&mut self, act: bool, hold_secs: f64, keep: Option<&str>) -> Vec<Change> {
        let fg = sys::foreground_pid();
        let fg_name = keep.map(|k| k.to_lowercase()).or_else(|| fg.and_then(|p| self.last_procs.iter().find(|x| x.pid == p)).map(|p| p.name.to_lowercase()));
        let on_names: HashSet<String> = self.last_procs.iter().filter(|p| self.onscreen.contains(&p.pid)).map(|p| p.name.to_lowercase()).collect();
        let me = std::process::id();
        let busy: Vec<Proc> = self.last_procs.iter()
            .filter(|p| p.pid > 4 && p.pid != me && self.last_pct.get(&p.pid).copied().unwrap_or(0.0) >= 2.0)
            .filter(|p| { let n = p.name.to_lowercase(); Some(n.clone()) != fg_name && !n.starts_with("snifrig") })
            .cloned().collect();
        let mut out = Vec::new();
        for p in busy {
            if self.protected(&p) { continue; }
            let level = if on_names.contains(&p.name.to_lowercase()) { 1 } else { 3 };
            if let Some(c) = self.adopt(p.pid, level, "quick fix: you asked for your PC back", act) { out.push(c); }
        }
        self.hold_until = crate::now_unix() + hold_secs;
        out
    }

    /// Share of a core used by every process of this image name on the latest tick.
    pub fn name_pct(&self, name: &str) -> f64 {
        self.last_procs.iter().filter(|p| p.name.eq_ignore_ascii_case(name)).map(|p| self.last_pct.get(&p.pid).copied().unwrap_or(0.0)).sum()
    }

    /// Cores this app may run on (its first process's affinity), and all cores.
    pub fn name_cores(&self, name: &str) -> (u32, u32) {
        let all = self.cores as u32;
        let n = self.last_procs.iter().find(|p| p.name.eq_ignore_ascii_case(name)).and_then(|p| sys::get_affinity(p.pid)).map(|(p, s)| (p & s).count_ones()).unwrap_or(all);
        (n.max(1), all)
    }

    /// Pids of this name the governor holds back, with their level.
    pub fn eased(&self, name: &str) -> Vec<(u32, u8)> {
        self.demoted.values().filter(|d| d.name.eq_ignore_ascii_case(name)).map(|d| (d.pid, d.level)).collect()
    }

    /// Write what is eased right now, so a crash can be undone at the next start.
    pub fn save_demoted(&self, path: &std::path::Path) {
        let body: String = self.demoted.values().map(|d| format!("{} {} {} {} {} {}\n", d.pid, d.created, d.level, d.orig_priority, d.gentle as u8, d.name)).collect();
        let _ = std::fs::write(path, body);
    }

    /// Put everything back, e.g. on exit or pause.
    pub fn restore_all(&mut self, act: bool) -> Vec<Change> {
        let all: Vec<(Key, Demotion)> = self.demoted.drain().collect();
        all.into_iter().map(|(k, d)| {
            let result = if !act { Ok(()) } else if sys::image_matches(k.0, k.1, &d.name) { apply_level(k.0, d.level, 0, d.orig_priority, d.gentle) } else { Ok(()) };
            Change { pid: k.0, name: d.name, from: d.level, to: 0, reason: "governor stopped or paused".into(), result }
        }).collect()
    }
}

#[cfg(test)]
mod gov_tests {
    use super::*;
    fn s(pid: u32, name: &str, pct: f64) -> Seen { Seen { key: (pid, 1), name: name.into(), core_pct: pct, foreground: false, protected: false, windowed: false, onscreen: false } }

    #[test]
    fn busy_tab_of_front_app_is_reined_in_lightly() {
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let mut fg = s(1, "comet.exe", 30.0); fg.foreground = true; fg.windowed = true;
        let mut other_window = s(3, "comet.exe", 90.0); other_window.windowed = true;
        let tab = s(2, "comet.exe", 120.0);
        let steps = decide(95.0, &m, &[fg.clone(), tab.clone(), other_window], &HashMap::new());
        assert_eq!(steps.len(), 1, "{:?}", steps);
        assert!(matches!(&steps[0], Step::Demote { key: (2, 1), to_level: 1, .. }));
        let mut d = HashMap::new();
        d.insert((2, 1), Demotion { pid: 2, created: 1, name: "comet.exe".into(), level: 1, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        assert!(decide(95.0, &m, &[fg.clone(), tab.clone()], &d).is_empty(), "already at the cap for a front app's helper, no flapping");
        d.get_mut(&(2, 1)).unwrap().level = 3;
        assert!(matches!(&decide(95.0, &m, &[fg, tab], &d)[0], Step::Restore { to_level: 1, .. }));
    }

    #[test]
    fn app_on_another_screen_only_gets_efficiency_mode() {
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let mut fg = s(1, "claude.exe", 10.0); fg.foreground = true; fg.windowed = true; fg.onscreen = true;
        let mut game = s(2, "legendsofidleon.exe", 150.0); game.windowed = true; game.onscreen = true;
        let mut hidden = s(3, "comet.exe", 140.0); hidden.windowed = true;
        let mut d = HashMap::new();
        d.insert((2, 1), Demotion { pid: 2, created: 1, name: "legendsofidleon.exe".into(), level: 1, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        d.insert((3, 1), Demotion { pid: 3, created: 1, name: "comet.exe".into(), level: 1, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        let steps = decide(95.0, &m, &[fg.clone(), game.clone(), hidden.clone()], &d);
        assert_eq!(steps.len(), 1, "{:?}", steps);
        assert!(matches!(&steps[0], Step::Demote { key: (3, 1), to_level: 2, .. }), "the hidden app goes further, the one on screen stays at 1");
        d.get_mut(&(2, 1)).unwrap().level = 3;
        assert!(decide(95.0, &m, &[fg.clone(), game.clone(), hidden.clone()], &d).iter().any(|x| matches!(x, Step::Restore { key: (2, 1), to_level: 1, .. })), "dragged back on screen: back to efficiency only");
        // The windowless renderer of an app on screen draws what you see: efficiency mode at most.
        let renderer = s(4, "legendsofidleon.exe", 200.0);
        let mut d2 = HashMap::new();
        d2.insert((4, 1), Demotion { pid: 4, created: 1, name: "legendsofidleon.exe".into(), level: 1, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        d2.insert((2, 1), Demotion { pid: 2, created: 1, name: "legendsofidleon.exe".into(), level: 1, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        d2.insert((3, 1), Demotion { pid: 3, created: 1, name: "comet.exe".into(), level: 3, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        assert!(decide(95.0, &m, &[fg, game, hidden, renderer], &d2).is_empty(), "renderer of an on-screen app stays at 1, hidden app already at the bottom");
    }

    #[test]
    fn tuning_file_parses_and_reports_errors() {
        let (t, e) = Tuning::parse("busy_pct = 60 # comment\nbusy_secs=2\nmin_core_pct=40%\nmax_level=2\ngame_mode=off\nnonsense=1\nmax_per_tick=99\n");
        assert_eq!((t.busy_pct, t.busy_ticks, t.min_core_pct, t.max_level, t.game_mode, t.max_per_tick), (60.0, 2, 40.0, 2, false, MAX_DEMOTIONS_PER_TICK));
        assert_eq!(e.len(), 2, "{:?}", e);
        let (t, e) = Tuning::parse("busy_pct=40\ncalm_pct=60");
        assert_eq!((t.busy_pct, t.calm_pct), (BUSY_PCT, CALM_PCT));
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn tuned_thresholds_and_game_mode_change_decisions() {
        let mut h = Hints::default();
        h.tuning.min_core_pct = 100.0;
        h.tuning.max_level = 1;
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let mut d = HashMap::new();
        d.insert((2, 1), Demotion { pid: 2, created: 1, name: "b.exe".into(), level: 1, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        let steps = decide_with(95.0, &m, &[s(1, "a.exe", 80.0), s(2, "b.exe", 300.0), s(3, "c.exe", 200.0)], &d, &h);
        assert_eq!(steps.len(), 1, "a under min_core_pct, b already at max_level 1: {:?}", steps);
        assert!(matches!(&steps[0], Step::Demote { key: (3, 1), to_level: 1, .. }));
        let t = Tuning::default();
        let mut mood = Mood::default();
        mood.update(60.0, &t, false, true);
        assert_eq!(mood.busy_ticks, 0);
        mood.update(60.0, &t, true, true);
        assert_eq!(mood.busy_ticks, 1, "60% counts as busy while a game runs (game_busy_pct 50)");
        let mut m2 = Mood::default();
        m2.update(85.0, &t, false, false);
        assert_eq!(m2.busy_ticks, 0, "85% CPU while it still feels fast: leave apps alone");
        m2.update(95.0, &t, false, false);
        assert_eq!(m2.busy_ticks, 1, "nearly maxed counts even if it feels fine");
        m2.update(85.0, &t, false, true);
        assert_eq!(m2.busy_ticks, 2, "85% and it feels slow: step in");
    }

    #[test]
    fn quiet_machine_does_nothing() {
        let m = Mood { busy_ticks: 0, calm_ticks: 0 };
        assert!(decide(40.0, &m, &[s(1, "ffmpeg.exe", 400.0)], &HashMap::new()).is_empty());
    }

    #[test]
    fn busy_demotes_biggest_background_first_one_step() {
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let seen = vec![s(1, "ffmpeg.exe", 450.0), s(2, "comet.exe", 70.0), s(3, "small.exe", 10.0)];
        let steps = decide(95.0, &m, &seen, &HashMap::new());
        assert_eq!(steps.len(), 2);
        assert!(matches!(&steps[0], Step::Demote { key: (1, 1), to_level: 1, .. }));
        assert!(matches!(&steps[1], Step::Demote { key: (2, 1), to_level: 1, .. }));
    }

    #[test]
    fn never_touches_foreground_name_or_protected() {
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let mut fg = s(1, "comet.exe", 80.0); fg.foreground = true;
        let mut other_tab = s(2, "comet.exe", 90.0); other_tab.windowed = true;
        let mut prot = s(3, "dwm.exe", 90.0); prot.protected = true;
        let steps = decide(95.0, &m, &[fg, other_tab, prot], &HashMap::new());
        assert!(steps.is_empty(), "{:?}", steps);
    }

    #[test]
    fn foreground_restores_at_once_and_calm_restores_stepwise() {
        let mut d = HashMap::new();
        d.insert((1, 1), Demotion { pid: 1, created: 1, name: "ffmpeg.exe".into(), level: 3, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        let mut fg = s(1, "ffmpeg.exe", 300.0); fg.foreground = true;
        let steps = decide(90.0, &Mood { busy_ticks: 5, calm_ticks: 0 }, &[fg], &d);
        assert!(matches!(&steps[0], Step::Restore { to_level: 0, .. }));
        let steps = decide(20.0, &Mood { busy_ticks: 0, calm_ticks: CALM_TICKS + (RESTORE_EVERY - CALM_TICKS % RESTORE_EVERY) % RESTORE_EVERY }, &[s(1, "ffmpeg.exe", 5.0)], &d);
        assert!(matches!(&steps[0], Step::Restore { to_level: 2, .. }));
    }

    #[test]
    fn hints_protect_block_and_weight() {
        let m = Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 };
        let seen = vec![s(1, "A.exe", 300.0), s(2, "b.exe", 200.0), s(3, "c.exe", 100.0)];
        let mut h = Hints::default();
        h.protect.insert("a.exe".into());
        let steps = decide_with(95.0, &m, &seen, &HashMap::new(), &h);
        assert!(matches!(&steps[0], Step::Demote { key: (2, 1), .. }) && matches!(&steps[1], Step::Demote { key: (3, 1), .. }));
        let mut h = Hints::default();
        h.blocked.insert("b.exe".into());
        let steps = decide_with(95.0, &m, &seen, &HashMap::new(), &h);
        assert!(!steps.iter().any(|x| matches!(x, Step::Demote { key: (2, 1), .. })));
        // a never-helpful big user (0.2) is tried after a smaller one that always helps (0.9)
        let mut h = Hints::default();
        h.prob.insert("a.exe".into(), 0.2);
        h.prob.insert("c.exe".into(), 0.9);
        let steps = decide_with(95.0, &m, &seen, &HashMap::new(), &h);
        assert!(matches!(&steps[0], Step::Demote { key: (2, 1), .. }), "b 200*1.0 first");
        assert!(matches!(&steps[1], Step::Demote { key: (3, 1), .. }), "c 100*0.9 beats a 300*0.2");
    }

    #[test]
    fn gone_process_is_forgotten_and_max_level_not_exceeded() {
        let mut d = HashMap::new();
        d.insert((9, 1), Demotion { pid: 9, created: 1, name: "gone.exe".into(), level: 1, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        d.insert((1, 1), Demotion { pid: 1, created: 1, name: "ffmpeg.exe".into(), level: MAX_LEVEL, orig_priority: sys::NORMAL, since: 0.0, reason: String::new(), gentle: false });
        let steps = decide(95.0, &Mood { busy_ticks: BUSY_TICKS, calm_ticks: 0 }, &[s(1, "ffmpeg.exe", 400.0)], &d);
        assert!(steps.contains(&Step::Forget { key: (9, 1) }));
        assert!(!steps.iter().any(|x| matches!(x, Step::Demote { key: (1, 1), .. })));
    }
}

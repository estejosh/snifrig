//! The safety gate. Every action goes through Policy::decide (automatic path)
//! or Policy::approve_check (human-approved path). Order of checks matters:
//! hard refusals first (never-touch list, too young), then soft gates
//! (pause, dry run, license), then mode and rate.

use crate::{Action, Mode, Target, Verdict};

/// Processes the fixer never acts on, by exact lowercase image name.
/// Killing or trimming any of these can crash the session, the desktop, WSL, or security tooling.
pub const DENY_NAMES: &[&str] = &[
    "system", "registry", "memory compression", "secure system", "idle",
    "smss.exe", "csrss.exe", "wininit.exe", "services.exe", "lsass.exe", "lsaiso.exe",
    "winlogon.exe", "dwm.exe", "explorer.exe", "fontdrvhost.exe", "sihost.exe",
    "ctfmon.exe", "audiodg.exe", "spoolsv.exe", "taskhostw.exe", "conhost.exe",
    "msmpeng.exe", "nissrv.exe", "securityhealthservice.exe", "mpdefendercoreservice.exe",
    "vmmem", "vmmemwsl", "vmcompute.exe", "wslservice.exe", "wsl.exe", "wslhost.exe",
];
/// Any image name starting with these (our own binaries).
pub const DENY_PREFIX: &[&str] = &["snifrig"];
/// Any command line containing these, lowercase. Protects scripts that run under
/// generic hosts like python.exe or node.exe.
pub const DENY_CMDLINE: &[&str] = &["gpu_arbiter", "ferryman"];
/// Safe to restart a service inside, never safe to terminate: one svchost can host many services.
pub const NO_TERMINATE: &[&str] = &["svchost.exe"];

pub const MIN_AGE_SECS: f64 = 120.0;
pub const MAX_AUTO_PER_HOUR: usize = 3;

pub struct Policy {
    pub mode: Mode,
    pub now: f64,
    /// Unix seconds; 0 when not paused. Written by `snifrig pause`.
    pub paused_until: f64,
    pub licensed: bool,
    /// User entries from fix-allow.txt (lowercase). Lets auto mode terminate or restart these.
    pub allow: Vec<String>,
    /// User entries from fix-deny.txt (lowercase). Added to the never-touch list.
    pub extra_deny: Vec<String>,
    /// Unix times of actions executed automatically in the past hour or so.
    pub recent_auto: Vec<f64>,
}

/// A list entry matches the image name exactly, or is a substring of the command line.
fn entry_matches(entry: &str, name: &str, cmdline: &str) -> bool {
    let e = entry.trim().to_lowercase();
    !e.is_empty() && (e == name || cmdline.contains(&e))
}

/// Why this target must never get this action, or None if it may.
pub fn denied(t: &Target, action: &Action, extra: &[String]) -> Option<String> {
    let name = t.name.to_lowercase();
    let cmd = t.cmdline.to_lowercase();
    if t.pid == 0 || t.pid == 4 || t.pid == std::process::id() {
        return Some(format!("{} (pid {}) is a system or snifrig process", t.name, t.pid));
    }
    if DENY_NAMES.contains(&name.as_str()) || DENY_PREFIX.iter().any(|p| name.starts_with(p)) {
        return Some(format!("{} is on the never-touch list", t.name));
    }
    if let Some(p) = DENY_CMDLINE.iter().find(|p| cmd.contains(*p)) {
        return Some(format!("{} is running {} which is on the never-touch list", t.name, p));
    }
    if let Some(e) = extra.iter().find(|e| entry_matches(e, &name, &cmd)) {
        return Some(format!("{} matches '{}' in fix-deny.txt", t.name, e.trim()));
    }
    if *action == Action::Terminate && NO_TERMINATE.contains(&name.as_str()) {
        return Some(format!("{} can host several services; restart the service instead of terminating it", t.name));
    }
    None
}

impl Policy {
    pub fn allowed(&self, t: &Target) -> bool {
        let (name, cmd) = (t.name.to_lowercase(), t.cmdline.to_lowercase());
        self.allow.iter().any(|e| entry_matches(e, &name, &cmd))
    }

    fn auto_budget_left(&self) -> bool {
        self.recent_auto.iter().filter(|&&x| self.now - x < 3600.0).count() < MAX_AUTO_PER_HOUR
    }

    /// Checks that hold no matter who asks: never-touch list and minimum age.
    fn hard(&self, action: &Action, t: &Target) -> Option<String> {
        if let Some(why) = denied(t, action, &self.extra_deny) { return Some(why); }
        if t.age_secs < MIN_AGE_SECS {
            return Some(format!("{} is only {:.0} s old; the fixer waits until a process is 2 min old", t.name, t.age_secs));
        }
        None
    }

    /// Automatic path: what to do with a planned action on a live target.
    pub fn decide(&self, action: &Action, target: Option<&Target>) -> Verdict {
        if self.mode == Mode::Off { return Verdict::Refuse("fixer is off".into()); }
        if *action == Action::Report { return Verdict::LogOnly("report only".into()); }
        let t = match target { Some(t) => t, None => return Verdict::Refuse("no live process to act on".into()) };
        if let Some(why) = self.hard(action, t) { return Verdict::Refuse(why); }
        if self.now < self.paused_until { return Verdict::LogOnly("fixing is paused".into()); }
        if self.mode == Mode::DryRun { return Verdict::LogOnly("dry run".into()); }
        if !self.licensed { return Verdict::LogOnly("fixing needs a license key; showing what it would do".into()); }
        match self.mode {
            Mode::Ask => Verdict::Queue,
            Mode::Auto if !self.auto_budget_left() => Verdict::Queue,
            Mode::Auto if action.mild() || self.allowed(t) => Verdict::Execute,
            Mode::Auto => Verdict::Queue,
            Mode::Off | Mode::DryRun => Verdict::Refuse("unreachable mode".into()),
        }
    }

    /// Human-approved path (tray or `snifrig-fix approve`). The person has decided, so the
    /// mode and the auto rate limit do not apply, but the never-touch list, minimum age,
    /// pause and license still do.
    pub fn approve_check(&self, action: &Action, t: &Target) -> Result<(), String> {
        if let Some(why) = self.hard(action, t) { return Err(why); }
        if self.now < self.paused_until { return Err("fixing is paused; run `snifrig resume` first".into()); }
        if !self.licensed { return Err("fixing needs a license key".into()); }
        Ok(())
    }
}

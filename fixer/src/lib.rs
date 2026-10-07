//! snifrig-fix acts on the alerts the free snifrig monitor writes.
//! It is licensed separately from the monitor: see LICENSE in this folder.
//!
//! Flow: alerts.jsonl -> plan (alert -> Intent) -> proc (Intent -> Targets)
//! -> policy (Target -> Verdict) -> actions (Execute) | ledger (Queue / log).
//! Safety lives in policy.rs. Nothing acts without passing Policy::decide,
//! and approvals re-check through Policy::approve_check.

pub mod actions;
pub mod json;
pub mod ledger;
pub mod license;
pub mod plan;
pub mod policy;
pub mod proc;

/// What the fixer can do, mildest first.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Log it, change nothing.
    Report,
    /// EmptyWorkingSet: pushes the process's pages out of RAM. Nothing is closed.
    Trim,
    /// Drop the process to below-normal priority.
    LowerPriority,
    /// Stop and start the named Windows service that owns the process.
    RestartService(String),
    /// TerminateProcess. Unsaved work in that process is lost.
    Terminate,
}

impl Action {
    /// Mild actions lose no data and can run in auto mode without an allowlist entry.
    pub fn mild(&self) -> bool { matches!(self, Action::Report | Action::Trim | Action::LowerPriority) }
    pub fn label(&self) -> String {
        match self {
            Action::Report => "report".into(),
            Action::Trim => "trim".into(),
            Action::LowerPriority => "lower-priority".into(),
            Action::RestartService(s) => format!("restart-service:{}", s),
            Action::Terminate => "terminate".into(),
        }
    }
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "report" => Action::Report,
            "trim" => Action::Trim,
            "lower-priority" => Action::LowerPriority,
            "terminate" => Action::Terminate,
            _ => Action::RestartService(s.strip_prefix("restart-service:")?.to_string()),
        })
    }
}

/// Which process an intent is about, before it is resolved against the live system.
#[derive(Clone, Debug, PartialEq)]
pub enum Subject {
    None,
    /// A specific process named in the alert. Resolving must confirm the name still matches the pid.
    Pid { name: String, pid: u32 },
    /// The n processes holding the most private memory right now.
    TopPrivate(usize),
}

/// One line of alerts.jsonl.
#[derive(Clone, Debug, Default)]
pub struct Alert { pub t: String, pub key: String, pub msg: String }

/// What the planner wants to do about one alert.
#[derive(Clone, Debug)]
pub struct Intent { pub alert: Alert, pub subject: Subject, pub action: Action, pub why: String }

/// A live process, resolved by proc.rs.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub pid: u32,
    pub name: String,
    pub age_secs: f64,
    /// Empty if unreadable. Policy lowercases it.
    pub cmdline: String,
    /// Windows service hosted by this process, if it hosts exactly one.
    pub service: Option<String>,
    pub private_mb: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode { Off, DryRun, Ask, Auto }

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s.trim() { "off" => Mode::Off, "dry-run" => Mode::DryRun, "ask" => Mode::Ask, "auto" => Mode::Auto, _ => return None })
    }
    pub fn as_str(&self) -> &'static str {
        match self { Mode::Off => "off", Mode::DryRun => "dry-run", Mode::Ask => "ask", Mode::Auto => "auto" }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Do it now.
    Execute,
    /// Put it in pending.json for the user to approve or dismiss.
    Queue,
    /// Log what would happen, change nothing (dry run, paused, unlicensed, report-only).
    LogOnly(String),
    /// Never do this; the string says why.
    Refuse(String),
}

pub fn now_unix() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests;

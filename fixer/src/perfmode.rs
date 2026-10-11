//! Performance mode: when a game is the app in front, switch to a fast power plan and keep the
//! background quieter (the governor treats a lower CPU level as busy and never demotes the game,
//! even when you alt-tab). When the game exits, the power plan you had goes back.
//!
//! A program counts as a game when a rule says game=yes, or (unless a rule says game=no or
//! deliberately slows it) the local brain classified it as a game, or it runs Direct3D full screen.
//! The plan we replaced is saved to perf-plan.txt first, so a crash or reboot cannot leave it changed.

use crate::governor::Tuning;
use crate::rules::{self, Rule};
use crate::sys::{self, Proc};
use std::path::{Path, PathBuf};

const SAVED: &str = "perf-plan.txt";
const ENTER_TICKS: u32 = 3;
const ULTIMATE: &str = "e9a42b02-d5df-448d-aa00-03f14749eb61";
const HIGH: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";

/// Pure: is this foreground app a game?
pub fn is_game(name: &str, rules: &[Rule], brain_says_game: bool, fullscreen_d3d: bool) -> bool {
    match rules::game_override(rules, name) {
        Some(g) => g,
        None => brain_says_game || fullscreen_d3d,
    }
}

/// Pure: pick the plan GUID to switch to from `powercfg /list` output.
pub fn pick_plan(setting: &str, list: &str) -> Option<String> {
    let plans = parse_plans(list);
    match setting {
        "none" | "off" => None,
        "auto" | "" => plans.iter().find(|(g, n)| g == ULTIMATE || n.to_lowercase().contains("ultimate"))
            .or_else(|| plans.iter().find(|(g, n)| g == HIGH || n.to_lowercase() == "high performance"))
            .map(|(g, _)| g.clone()),
        guid => plans.iter().find(|(g, n)| g == guid || n.to_lowercase() == guid).map(|(g, _)| g.clone()),
    }
}

/// (guid, name) pairs and the active one from `powercfg /list`.
pub fn parse_plans(list: &str) -> Vec<(String, String)> {
    list.lines().filter_map(|l| {
        let i = l.find("GUID:")?;
        let rest = l[i + 5..].trim();
        let (g, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let name = tail.trim().trim_end_matches('*').trim().trim_start_matches('(').trim_end_matches(')').to_string();
        Some((g.to_lowercase(), name))
    }).collect()
}

pub fn parse_active(list: &str) -> Option<String> {
    list.lines().find(|l| l.trim_end().ends_with('*') && l.contains("GUID:")).and_then(|l| parse_plans(l).into_iter().next()).map(|(g, _)| g)
}

fn powercfg(args: &[&str]) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powercfg").args(args).creation_flags(0x0800_0000).output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub struct PerfMode {
    dir: PathBuf,
    /// (pid, created, lowercase name) of the game we are running for.
    pub game: Option<(u32, u64, String)>,
    candidate: Option<(u32, u64, u32)>,
    last_fg: Option<(u32, u64)>,
    /// Plan GUID we switched to, if we switched.
    ours: Option<String>,
}

impl PerfMode {
    /// Also undoes a plan switch left behind by a crash.
    pub fn new(dir: &Path) -> (PerfMode, Option<String>) {
        let mut note = None;
        if let Ok(saved) = std::fs::read_to_string(dir.join(SAVED)) {
            let saved = saved.trim().to_string();
            if !saved.is_empty() && powercfg(&["/setactive", &saved]).is_some() {
                note = Some(format!("performance mode: put the power plan back to {} (left over from last run)", saved));
            }
            let _ = std::fs::remove_file(dir.join(SAVED));
        }
        (PerfMode { dir: dir.to_path_buf(), game: None, candidate: None, last_fg: None, ours: None }, note)
    }

    /// One tick. `brain_game(name)` says whether the brain classified the name as a game.
    /// Returns notes for the ledger when something changed.
    pub fn tick(&mut self, procs: &[Proc], fg: Option<u32>, rules: &[Rule], t: &Tuning, act: bool, brain_game: &dyn Fn(&str) -> bool) -> Vec<String> {
        let mut notes = Vec::new();
        if !t.game_mode {
            if self.game.is_some() { notes.extend(self.exit(act, "performance mode turned off")); }
            return notes;
        }
        if let Some((pid, created, name)) = self.game.clone() {
            if !procs.iter().any(|p| p.pid == pid && p.created == created) {
                notes.extend(self.exit(act, &format!("{} closed", name)));
            }
            return notes;
        }
        let Some(p) = fg.and_then(|fp| procs.iter().find(|p| p.pid == fp)) else { self.candidate = None; self.last_fg = None; return notes };
        let key = (p.pid, p.created);
        if self.last_fg != Some(key) {
            // Checking costs a little (brain cache, a shell call), so only when the front app changes.
            self.last_fg = Some(key);
            let name = p.name.to_lowercase();
            let denied = crate::policy::DENY_NAMES.contains(&name.as_str()) || crate::policy::DENY_PREFIX.iter().any(|x| name.starts_with(x));
            self.candidate = if !denied && is_game(&name, rules, brain_game(&name), sys::fullscreen_d3d()) { Some((p.pid, p.created, 1)) } else { None };
            return notes;
        }
        if let Some((pid, c, n)) = self.candidate {
            if n + 1 < ENTER_TICKS { self.candidate = Some((pid, c, n + 1)); return notes; }
            self.candidate = None;
            notes.push(self.enter(pid, c, p.name.to_lowercase(), t, act));
        }
        notes
    }

    fn enter(&mut self, pid: u32, created: u64, name: String, t: &Tuning, act: bool) -> String {
        self.game = Some((pid, created, name.clone()));
        let list = powercfg(&["/list"]).unwrap_or_default();
        let (active, target) = (parse_active(&list), pick_plan(&t.game_plan, &list));
        let plan = match (&active, &target) {
            (Some(a), Some(g)) if a != g => {
                if !act { format!(", would switch power plan to {}", g) }
                else if std::fs::write(self.dir.join(SAVED), a).is_ok() && powercfg(&["/setactive", g]).is_some() {
                    self.ours = Some(g.clone());
                    format!(", power plan switched to {}", plan_name(&list, g))
                } else { let _ = std::fs::remove_file(self.dir.join(SAVED)); ", could not switch power plan".into() }
            }
            (_, None) => String::new(),
            _ => ", power plan already fast".into(),
        };
        format!("performance mode: on for {}{}; background apps are eased sooner and the game is never slowed", name, plan)
    }

    fn exit(&mut self, act: bool, why: &str) -> Vec<String> {
        let mut msg = format!("performance mode: off ({})", why);
        if let Some(ours) = self.ours.take() {
            let saved = std::fs::read_to_string(self.dir.join(SAVED)).unwrap_or_default().trim().to_string();
            let active = powercfg(&["/list"]).and_then(|l| parse_active(&l));
            // Only put it back if nobody changed the plan meanwhile.
            if act && !saved.is_empty() && active.as_deref() == Some(ours.as_str()) && powercfg(&["/setactive", &saved]).is_some() {
                msg.push_str(", power plan put back");
            }
            let _ = std::fs::remove_file(self.dir.join(SAVED));
        }
        self.game = None;
        self.last_fg = None;
        vec![msg]
    }

    /// Leave performance mode (fixer stopping or paused).
    pub fn stop(&mut self, act: bool) -> Vec<String> { if self.game.is_some() { self.exit(act, "fixer stopped or paused") } else { Vec::new() } }
}

fn plan_name(list: &str, guid: &str) -> String {
    parse_plans(list).into_iter().find(|(g, _)| g == guid).map(|(_, n)| n).unwrap_or_else(|| guid.to_string())
}

#[cfg(test)]
mod perf_tests {
    use super::*;
    const LIST: &str = "Existing Power Schemes (* Active)\r\n-----------------------------------\r\nPower Scheme GUID: 381b4222-f694-41f0-9685-ff5bb260df2e  (Balanced)\r\nPower Scheme GUID: 5604813e-9a73-49e2-87c8-f320974f6082  (Ultimate Performance)\r\nPower Scheme GUID: 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c  (High performance) *\r\n";

    #[test]
    fn reads_plans_and_picks_fastest() {
        assert_eq!(parse_active(LIST).as_deref(), Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c"));
        assert_eq!(pick_plan("auto", LIST).as_deref(), Some("5604813e-9a73-49e2-87c8-f320974f6082"));
        assert_eq!(pick_plan("high performance", LIST).as_deref(), Some(HIGH));
        assert_eq!(pick_plan("none", LIST), None);
        assert_eq!(pick_plan("auto", &LIST.replace("Ultimate Performance", "Power saver")).as_deref(), Some(HIGH));
    }

    #[test]
    fn rules_override_game_detection() {
        let rs: Vec<Rule> = ["idle.exe background=yes affinity=0x8000", "tool.exe game=yes", "vid.exe game=no"].iter().map(|l| rules::parse_line(l).unwrap().unwrap()).collect();
        assert!(!is_game("idle.exe", &rs, true, true), "a rule that slows it on purpose wins");
        assert!(is_game("tool.exe", &rs, false, false));
        assert!(!is_game("vid.exe", &rs, false, true));
        assert!(is_game("new.exe", &rs, true, false) && is_game("d3d.exe", &rs, false, true) && !is_game("app.exe", &rs, false, false));
    }
}

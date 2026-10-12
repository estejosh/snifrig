//! Everything the window shows or changes lives in snifrig's data folder. This module reads
//! those files, and writes the two the user edits here: fix-rules.txt and fix-governor.txt.
use crate::json::{self, J};
use std::path::{Path, PathBuf};

pub fn now() -> f64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }

fn read(dir: &Path, f: &str) -> String { std::fs::read_to_string(dir.join(f)).unwrap_or_default() }
fn read_json(dir: &Path, f: &str) -> J { json::parse(&read(dir, f)).unwrap_or(J::Null) }

pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp-app");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---------- monitor and fixer state ----------

#[derive(Clone, Default)]
pub struct Held { pub pid: u32, pub name: String, pub level: u8, pub reason: String, pub since: f64 }

#[derive(Clone, Default)]
pub struct Pending { pub id: String, pub name: String, pub pid: u32, pub action: String, pub why: String, pub msg: String }

#[derive(Clone, Default)]
pub struct Event { pub unix: f64, pub key: String, pub name: String, pub action: String, pub status: String, pub note: String }

#[derive(Clone, Default)]
pub struct Gpu { pub ok: bool, pub temp_c: f64, pub power_w: f64, pub limit_w: f64, pub mhz: f64, pub max_mhz: f64, pub used_mb: f64, pub total_mb: f64, pub reasons: String }

#[derive(Clone, Default)]
pub struct Brain { pub name: String, pub kind: String, pub confidence: f64 }

#[derive(Clone, Default)]
pub struct State {
    pub monitor_live: bool,
    pub headline: String,
    pub causes: Vec<String>,
    pub level: String,
    pub paused_until: f64,
    pub fixer_live: bool,
    pub mode: String,
    pub has_key: bool,
    pub acting: bool,
    pub busy: bool,
    pub game: Option<String>,
    pub held: Vec<Held>,
    pub suggestions: Vec<(String, u32)>,
    pub pending: Vec<Pending>,
    pub events: Vec<Event>,
    pub gpu: Gpu,
    pub brain: Vec<Brain>,
    /// (unix, commit GB, available GB), one per monitor cycle, last 24 h.
    pub mem_hist: Vec<(f64, f32, f32)>,
}

pub fn load_state(dir: &Path, with_history: bool, old: &State) -> State {
    let t = now();
    let st = read_json(dir, "status.json");
    let gov = read_json(dir, "governor.json");
    let gpu = read_json(dir, "gpu.json");
    let mut s = State {
        monitor_live: t - st.get("unix").num() < 180.0,
        headline: st.get("headline").str().to_string(),
        causes: st.get("causes").arr().iter().map(|c| c.str().to_string()).collect(),
        level: st.get("level").str().to_string(),
        paused_until: st.get("paused_until").num(),
        fixer_live: t - gov.get("unix").num() < 30.0,
        mode: read(dir, "fix-mode.txt").trim().to_string(),
        has_key: dir.join("snifrig-fix.key").exists(),
        acting: gov.get("acting").bool(),
        busy: gov.get("busy").bool(),
        game: Some(gov.get("game").str().to_string()).filter(|g| !g.is_empty()),
        held: gov.get("demoted").arr().iter().map(|d| Held { pid: d.get("pid").num() as u32, name: d.get("name").str().into(), level: d.get("level").num() as u8, reason: d.get("reason").str().into(), since: d.get("since").num() }).collect(),
        suggestions: gov.get("suggestions").arr().iter().map(|d| (d.get("name").str().to_string(), d.get("count").num() as u32)).collect(),
        pending: read_json(dir, "pending.json").get("items").arr().iter().map(|p| Pending { id: p.get("id").str().into(), name: p.get("name").str().into(), pid: p.get("pid").num() as u32, action: p.get("action").str().into(), why: p.get("why").str().into(), msg: p.get("msg").str().into() }).collect(),
        events: Vec::new(),
        gpu: Gpu { ok: t - gpu.get("unix").num() < 900.0, temp_c: gpu.get("temp_c").num(), power_w: gpu.get("power_w").num(), limit_w: gpu.get("limit_w").num(), mhz: gpu.get("sm_mhz").num(), max_mhz: gpu.get("sm_max_mhz").num(), used_mb: gpu.get("used_mb").num(), total_mb: gpu.get("total_mb").num(), reasons: gpu.get("reasons").str().into() },
        brain: read(dir, "brain-cache.json").lines().filter_map(json::parse).map(|j| Brain { name: j.get("name").str().into(), kind: j.get("kind").str().into(), confidence: j.get("confidence").num() }).collect(),
        mem_hist: Vec::new(),
    };
    if s.mode.is_empty() { s.mode = "dry-run".into(); }
    let fixes = read(dir, "fixes.jsonl");
    let lines: Vec<&str> = fixes.lines().collect();
    s.events = lines.iter().rev().take(600).filter_map(|l| json::parse(l)).map(|j| Event {
        unix: j.get("unix").num(), key: j.get("key").str().into(), name: j.get("name").str().into(),
        action: j.get("action").str().into(), status: j.get("status").str().into(), note: j.get("note").str().into(),
    }).collect();
    if with_history {
        let h = read(dir, "snifrig.jsonl");
        let since = t - 86400.0;
        s.mem_hist = h.lines().filter_map(|l| {
            let j = json::parse(l)?;
            let u = iso_unix(j.get("t").str())?;
            if u < since { return None; }
            Some((u, (j.get("commit_mb").num() / 1024.0) as f32, (j.get("avail_mb").num() / 1024.0) as f32))
        }).collect();
    } else {
        s.mem_hist = old.mem_hist.clone();
    }
    s
}

pub fn iso_unix(t: &str) -> Option<f64> {
    let n = |a: usize, l: usize| t.get(a..a + l)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0, 4)?, n(5, 2)?, n(8, 2)?, n(11, 2)?, n(14, 2)?, n(17, 2)?);
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + (153 * m2 + 2) / 5 + d - 1;
    Some(((era * 146097 + doe - 719468) * 86400 + hh * 3600 + mm * 60 + ss) as f64)
}

// ---------- rules (fix-rules.txt) ----------

#[derive(Clone, Default, PartialEq, Debug)]
pub struct Rule {
    pub pattern: String,
    pub priority: String,   // "", idle, below_normal, normal
    pub efficiency: String, // "", on, off
    pub io: String,         // "", very_low, low, normal
    pub memory: String,
    pub affinity: u64,      // 0 = not set
    pub cap: u32,           // 0 = not set
    pub max_instances: u32, // 0 = not set
    pub governor: String,   // "", exempt, on
    pub max_level: Option<u8>,
    pub game: String,       // "", yes, no
    pub extra: Vec<String>, // tokens this window does not understand, kept as written
}

pub fn parse_affinity(v: &str) -> Option<u64> {
    if v.contains('-') || v.contains(',') || v.chars().all(|c| c.is_ascii_digit()) {
        let mut m = 0u64;
        for part in v.split(',') {
            let (a, b) = part.split_once('-').unwrap_or((part, part));
            let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
            if a > b || b > 63 { return None; }
            for c in a..=b { m |= 1 << c; }
        }
        return Some(m);
    }
    u64::from_str_radix(v.trim_start_matches("0x"), 16).ok().filter(|m| *m != 0)
}

pub fn core_list(m: u64) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut c = 0;
    while c < 64 {
        if m >> c & 1 == 1 {
            let s = c;
            while c + 1 < 64 && m >> (c + 1) & 1 == 1 { c += 1; }
            out.push(if s == c { s.to_string() } else { format!("{}-{}", s, c) });
        }
        c += 1;
    }
    out.join(",")
}

impl Rule {
    pub fn parse(line: &str) -> Option<Rule> {
        let l = line.split(" #").next().unwrap_or("").trim();
        if l.is_empty() || l.starts_with('#') { return None; }
        let mut it = l.split_whitespace();
        let mut r = Rule { pattern: it.next()?.to_lowercase(), ..Default::default() };
        for tok in it {
            let Some((k, v)) = tok.split_once('=') else { r.extra.push(tok.into()); continue };
            let v = v.to_lowercase();
            match k.to_lowercase().as_str() {
                "priority" => r.priority = v,
                "efficiency" => r.efficiency = v,
                "io" => r.io = v,
                "memory" => r.memory = v,
                "affinity" => match parse_affinity(&v) { Some(m) => r.affinity = m, None => r.extra.push(tok.into()) },
                "cap" => r.cap = v.parse().unwrap_or(0),
                "max_instances" => r.max_instances = v.parse().unwrap_or(0),
                "governor" => r.governor = if v == "on" || v == "yes" { "on".into() } else { "exempt".into() },
                "max_level" => r.max_level = v.parse().ok(),
                "game" => r.game = if v == "yes" || v == "on" { "yes".into() } else { "no".into() },
                "background" if v == "yes" => { r.efficiency = "on".into(); r.io = "low".into(); r.priority = "below_normal".into(); }
                _ => r.extra.push(tok.into()),
            }
        }
        Some(r)
    }

    pub fn line(&self) -> String {
        let mut p = vec![self.pattern.clone()];
        let bg = self.efficiency == "on" && self.io == "low" && self.priority == "below_normal";
        if bg { p.push("background=yes".into()); } else {
            if !self.priority.is_empty() { p.push(format!("priority={}", self.priority)); }
            if !self.efficiency.is_empty() { p.push(format!("efficiency={}", self.efficiency)); }
            if !self.io.is_empty() { p.push(format!("io={}", self.io)); }
        }
        if !self.memory.is_empty() { p.push(format!("memory={}", self.memory)); }
        if self.affinity != 0 { p.push(format!("affinity={}", core_list(self.affinity))); }
        if self.cap > 0 { p.push(format!("cap={}", self.cap)); }
        if self.max_instances > 0 { p.push(format!("max_instances={}", self.max_instances)); }
        if !self.governor.is_empty() { p.push(format!("governor={}", self.governor)); }
        if let Some(m) = self.max_level { p.push(format!("max_level={}", m)); }
        if !self.game.is_empty() { p.push(format!("game={}", self.game)); }
        p.extend(self.extra.iter().cloned());
        p.join(" ")
    }

    pub fn is_empty(&self) -> bool { self.line() == self.pattern }

    /// Short plain-words chips for the rules list.
    pub fn chips(&self) -> Vec<String> {
        let mut c = Vec::new();
        let bg = self.efficiency == "on" && self.io == "low" && self.priority == "below_normal";
        if bg { c.push("always background".into()); } else {
            match self.priority.as_str() { "idle" => c.push("idle priority".into()), "below_normal" => c.push("below normal".into()), "normal" => c.push("normal priority".into()), _ => {} }
            match self.efficiency.as_str() { "on" => c.push("efficiency mode".into()), "off" => c.push("no efficiency mode".into()), _ => {} }
            if !self.io.is_empty() { c.push(format!("disk {}", self.io.replace('_', " "))); }
        }
        if !self.memory.is_empty() { c.push(format!("memory {}", self.memory.replace('_', " "))); }
        if self.affinity != 0 { let n = self.affinity.count_ones(); c.push(format!("{} core{} ({})", n, if n == 1 { "" } else { "s" }, core_list(self.affinity))); }
        if self.cap > 0 { c.push(format!("max {}% CPU", self.cap)); }
        if self.max_instances > 0 { c.push(format!("max {} running", self.max_instances)); }
        match (self.governor.as_str(), self.max_level) {
            ("exempt", _) | (_, Some(0)) => c.push("governor never touches it".into()),
            (_, Some(n)) => c.push(format!("governor up to {}", level_name(n))),
            ("on", _) => c.push("governor manages it".into()),
            _ => {}
        }
        match self.game.as_str() { "yes" => c.push("is a game".into()), "no" => c.push("never a game".into()), _ => {} }
        for e in &self.extra { c.push(format!("{} (kept as written)", e)); }
        c
    }
}

pub fn level_name(n: u8) -> &'static str { match n { 0 => "nothing", 1 => "efficiency mode", 2 => "below normal", _ => "idle" } }

pub struct RulesFile { pub head: Vec<String>, pub rules: Vec<Rule> }

pub fn load_rules(dir: &Path) -> RulesFile {
    let text = read(dir, "fix-rules.txt");
    let mut head = Vec::new();
    let mut rules = Vec::new();
    for l in text.lines() {
        match Rule::parse(l) { Some(r) => rules.push(r), None => if rules.is_empty() && l.trim().starts_with('#') { head.push(l.to_string()) } }
    }
    RulesFile { head, rules }
}

pub fn save_rules(dir: &Path, f: &RulesFile) -> Result<(), String> {
    let mut s = String::new();
    for h in &f.head { s.push_str(h); s.push_str("\r\n"); }
    for r in &f.rules { s.push_str(&r.line()); s.push_str("\r\n"); }
    write_atomic(&dir.join("fix-rules.txt"), &s)
}

// ---------- governor settings (fix-governor.txt) ----------

#[derive(Clone, PartialEq, Debug)]
pub struct Tuning {
    pub busy_pct: f32, pub busy_secs: u32, pub calm_pct: f32, pub calm_secs: u32, pub restore_every_secs: u32,
    pub min_core_pct: f32, pub max_per_tick: u32, pub max_level: u8,
    pub game_mode: bool, pub game_plan: String, pub game_busy_pct: f32, pub reapply_secs: u32,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { busy_pct: 75.0, busy_secs: 3, calm_pct: 50.0, calm_secs: 10, restore_every_secs: 5, min_core_pct: 25.0, max_per_tick: 2, max_level: 3,
            game_mode: true, game_plan: "auto".into(), game_busy_pct: 50.0, reapply_secs: 30 }
    }
}

pub fn load_tuning(dir: &Path) -> Tuning {
    let mut t = Tuning::default();
    for raw in read(dir, "fix-governor.txt").lines() {
        let l = raw.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim().trim_end_matches('%'));
        let f = v.parse::<f32>().ok();
        let u = v.parse::<u32>().ok();
        match k {
            "busy_pct" => if let Some(x) = f { t.busy_pct = x },
            "busy_secs" => if let Some(x) = u { t.busy_secs = x },
            "calm_pct" => if let Some(x) = f { t.calm_pct = x },
            "calm_secs" => if let Some(x) = u { t.calm_secs = x },
            "restore_every_secs" => if let Some(x) = u { t.restore_every_secs = x },
            "min_core_pct" => if let Some(x) = f { t.min_core_pct = x },
            "max_per_tick" => if let Some(x) = u { t.max_per_tick = x },
            "max_level" => if let Some(x) = u { t.max_level = x.min(3) as u8 },
            "game_mode" => t.game_mode = matches!(v, "on" | "yes"),
            "game_plan" => t.game_plan = v.to_string(),
            "game_busy_pct" => if let Some(x) = f { t.game_busy_pct = x },
            "reapply_secs" => if let Some(x) = u { t.reapply_secs = x },
            _ => {}
        }
    }
    t
}

pub fn save_tuning(dir: &Path, t: &Tuning) -> Result<(), String> {
    let s = format!("# snifrig-fix governor settings, written by the snifrig app. The fixer re-reads this within 30 seconds.\r\n\
busy_pct = {:.0}\r\nbusy_secs = {}\r\ncalm_pct = {:.0}\r\ncalm_secs = {}\r\nrestore_every_secs = {}\r\nmin_core_pct = {:.0}\r\nmax_per_tick = {}\r\nmax_level = {}\r\n\
game_mode = {}\r\ngame_plan = {}\r\ngame_busy_pct = {:.0}\r\nreapply_secs = {}\r\n",
        t.busy_pct, t.busy_secs, t.calm_pct.min(t.busy_pct), t.calm_secs, t.restore_every_secs, t.min_core_pct, t.max_per_tick, t.max_level,
        if t.game_mode { "on" } else { "off" }, if t.game_plan.trim().is_empty() { "auto" } else { t.game_plan.trim() }, t.game_busy_pct, t.reapply_secs);
    write_atomic(&dir.join("fix-governor.txt"), &s)
}

// ---------- power plans and commands ----------

pub fn power_plans() -> Vec<(String, String, bool)> {
    let Ok(out) = cmd("powercfg", &["/list"]) else { return Vec::new() };
    out.lines().filter_map(|l| {
        let i = l.find("GUID:")?;
        let rest = l[i + 5..].trim();
        let (g, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let active = tail.trim_end().ends_with('*');
        let name = tail.trim().trim_end_matches('*').trim().trim_start_matches('(').trim_end_matches(')').to_string();
        Some((g.to_lowercase(), name, active))
    }).collect()
}

pub fn cmd(exe: &str, args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new(exe).args(args).creation_flags(0x0800_0000).output().map_err(|e| format!("{}: {}", exe, e))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() { Ok(text) } else { Err(if err.is_empty() { text } else { err }) }
}

pub fn bin_dir() -> PathBuf { std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())).unwrap_or_default() }

pub fn tool(name: &str, dir: &Path) -> PathBuf {
    let here = bin_dir().join(name);
    if here.exists() { here } else { dir.join("bin").join(name) }
}

#[cfg(test)]
mod t {
    use super::*;
    #[test]
    fn rules_round_trip() {
        let r = Rule::parse("LegendsOfIdleon.exe background=yes affinity=0x8000").unwrap();
        assert_eq!(r.line(), "legendsofidleon.exe background=yes affinity=15");
        assert_eq!(r.chips(), vec!["always background", "1 core (15)"]);
        let r = Rule::parse("ffmpeg*.exe priority=idle cap=60 max_level=2 weird=1").unwrap();
        assert_eq!(r.line(), "ffmpeg*.exe priority=idle cap=60 max_level=2 weird=1");
        assert_eq!(core_list(0b1011_0000_1111), "0-3,8-9,11");
        assert_eq!(parse_affinity("0-3,8"), Some(0x10f));
        assert!(Rule::parse("# c").is_none());
    }
}

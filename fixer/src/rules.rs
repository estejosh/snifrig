//! Per-app rules (Process Lasso style), applied once to each matching process.
//!
//! File: `fix-rules.txt` in the data dir. One rule per line, '#' starts a comment:
//!
//! ```text
//! # <image pattern> key=value ...        (pattern: image name, case-insensitive, '*' and '?' wildcards)
//! ffmpeg*.exe     priority=below_normal io=low cap=60
//! handbrake*.exe  background=yes max_instances=1
//! obs64.exe       priority=normal efficiency=off affinity=0-7
//! ```
//!
//! Keys: priority=idle|below_normal|normal  efficiency=on|off  io=very_low|low|normal
//! memory=very_low|low|normal  affinity=<hex mask (ff, 0xff) or core list (0-7, 0,2,4-5)>
//! cap=<1..100 percent of CPU, hard cap>  max_instances=<n>
//! background=yes (shorthand for efficiency=on io=low priority=below_normal; later keys win).
//! Priority never goes above normal. max_instances only reports the extra, newest copies and
//! queues a terminate for your approval; it never kills anything by itself.
//! A process named by a rule is "pinned": the automatic governor leaves it alone.

use crate::{policy, sys, Action, Target};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const FILE: &str = "fix-rules.txt";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rule {
    /// Lowercase image pattern.
    pub pattern: String,
    pub priority: Option<u32>,
    pub efficiency: Option<bool>,
    pub io: Option<u32>,
    pub memory: Option<u32>,
    pub affinity: Option<u64>,
    pub cap: Option<u32>,
    pub max_instances: Option<usize>,
    /// The rule as written (whitespace normalised, comment removed).
    pub text: String,
}

fn parse_affinity(v: &str) -> Result<u64, String> {
    if v.contains('-') || v.contains(',') {
        let mut m = 0u64;
        for part in v.split(',') {
            let (a, b) = match part.split_once('-') {
                Some((a, b)) => (a, b),
                None => (part, part),
            };
            let a: u32 = a.parse().map_err(|_| format!("bad core list '{}'", v))?;
            let b: u32 = b.parse().map_err(|_| format!("bad core list '{}'", v))?;
            if a > b || b > 63 { return Err(format!("bad core range '{}' (cores 0-63)", part)); }
            for c in a..=b { m |= 1u64 << c; }
        }
        return Ok(m);
    }
    let h = v.strip_prefix("0x").unwrap_or(v);
    match u64::from_str_radix(h, 16) {
        Ok(0) | Err(_) => Err(format!("bad affinity '{}': use a nonzero hex mask or a core list like 0-7", v)),
        Ok(m) => Ok(m),
    }
}

/// Ok(None) for blank lines and comments.
pub fn parse_line(line: &str) -> Result<Option<Rule>, String> {
    let mut l = line.trim();
    if let Some(i) = l.find(" #") { l = l[..i].trim(); }
    if l.is_empty() || l.starts_with('#') { return Ok(None); }
    let mut it = l.split_whitespace();
    let pat = it.next().unwrap_or("").to_lowercase();
    if pat.contains('=') { return Err("start the rule with the image name, e.g. ffmpeg*.exe".into()); }
    let mut r = Rule { pattern: pat, ..Default::default() };
    let mut any = false;
    for tok in it {
        let (k, v) = tok.split_once('=').ok_or_else(|| format!("'{}' is not key=value", tok))?;
        let v = v.to_lowercase();
        let bad = |what: &str| format!("{}: '{}' is not valid ({})", k, v, what);
        match k.to_lowercase().as_str() {
            "priority" => r.priority = Some(match v.as_str() {
                "idle" => sys::IDLE, "below_normal" => sys::BELOW_NORMAL, "normal" => sys::NORMAL,
                _ => return Err(bad("idle, below_normal or normal")),
            }),
            "efficiency" => r.efficiency = Some(match v.as_str() { "on" => true, "off" => false, _ => return Err(bad("on or off")) }),
            "io" => r.io = Some(match v.as_str() { "very_low" => 0, "low" => 1, "normal" => 2, _ => return Err(bad("very_low, low or normal")) }),
            "memory" => r.memory = Some(match v.as_str() { "very_low" => 1, "low" => 2, "normal" => 5, _ => return Err(bad("very_low, low or normal")) }),
            "affinity" => r.affinity = Some(parse_affinity(&v)?),
            "cap" => r.cap = Some(match v.parse::<u32>() { Ok(n) if (1..=100).contains(&n) => n, _ => return Err(bad("1 to 100")) }),
            "max_instances" => r.max_instances = Some(match v.parse::<usize>() { Ok(n) if n >= 1 => n, _ => return Err(bad("1 or more")) }),
            "background" => {
                if v != "yes" { return Err(bad("yes")); }
                r.efficiency = Some(true); r.io = Some(1); r.priority = Some(sys::BELOW_NORMAL);
            }
            other => return Err(format!("unknown key '{}'", other)),
        }
        any = true;
    }
    if !any { return Err("a rule needs at least one key=value".into()); }
    r.text = l.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(Some(r))
}

/// Case-insensitive match with '*' (any run) and '?' (one char).
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) { pi += 1; ni += 1; }
        else if pi < p.len() && p[pi] == '*' { star = Some(pi); mark = ni; pi += 1; }
        else if let Some(s) = star { pi = s + 1; mark += 1; ni = mark; }
        else { return false; }
    }
    while pi < p.len() && p[pi] == '*' { pi += 1; }
    pi == p.len()
}

fn read_rules_text(dir: &Path) -> String { std::fs::read_to_string(dir.join(FILE)).unwrap_or_default() }

/// Valid rules in file order. Bad lines are skipped (`rule list` shows them).
pub fn load(dir: &Path) -> Vec<Rule> {
    read_rules_text(dir).lines().filter_map(|l| parse_line(l).ok().flatten()).collect()
}

/// Literal (non-wildcard) lowercase names from the rules.
pub fn pinned_names(dir: &Path) -> Vec<String> {
    load(dir).into_iter().map(|r| r.pattern).filter(|p| !p.contains('*') && !p.contains('?')).collect()
}

/// True if any rule (wildcard or literal) covers this image name.
pub fn is_pinned(rules: &[Rule], name: &str) -> bool { rules.iter().any(|r| glob_match(&r.pattern, name)) }

pub fn mtime(dir: &Path) -> Option<std::time::SystemTime> { std::fs::metadata(dir.join(FILE)).and_then(|m| m.modified()).ok() }

pub fn write_atomic(path: &Path, content: &str) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    if std::fs::write(&tmp, content).is_ok() { let _ = std::fs::rename(&tmp, path); }
}

/// Non-blank, non-comment lines of the file, in order (what `rule list` numbers).
pub fn rule_lines(dir: &Path) -> Vec<String> {
    read_rules_text(dir).lines().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect()
}

/// Validate and append a rule. Returns the normalised rule text.
pub fn add(dir: &Path, text: &str) -> Result<String, String> {
    let r = parse_line(text)?.ok_or("empty rule")?;
    let mut s = read_rules_text(dir);
    if !s.is_empty() && !s.ends_with('\n') { s.push('\n'); }
    s.push_str(&r.text);
    s.push('\n');
    write_atomic(&dir.join(FILE), &s);
    Ok(r.text)
}

/// Remove rule number `n` (1-based, as `rule list` shows). Returns the removed line.
pub fn remove(dir: &Path, n: usize) -> Result<String, String> {
    let s = read_rules_text(dir);
    let mut k = 0;
    let mut removed = None;
    let mut out = String::new();
    for l in s.lines() {
        let t = l.trim();
        if !t.is_empty() && !t.starts_with('#') {
            k += 1;
            if k == n { removed = Some(t.to_string()); continue; }
        }
        out.push_str(l);
        out.push('\n');
    }
    let r = removed.ok_or_else(|| format!("no rule number {} (there are {})", n, k))?;
    write_atomic(&dir.join(FILE), &out);
    Ok(r)
}

/// Names the governor demoted 5+ times in the last 7 days that have no rule yet.
pub fn suggestions(dir: &Path) -> Vec<(String, usize)> {
    let text = std::fs::read_to_string(dir.join("fixes.jsonl")).unwrap_or_default();
    suggestions_from(&text, crate::now_unix(), &load(dir))
}

pub fn suggestions_from(fixes: &str, now: f64, rules: &[Rule]) -> Vec<(String, usize)> {
    use crate::json::{num_field, str_field};
    let mut counts: HashMap<String, usize> = HashMap::new();
    for l in fixes.lines() {
        let Some(unix) = num_field(l, "unix") else { continue };
        if unix < now - 7.0 * 86400.0 { continue; }
        if !str_field(l, "note").map_or(false, |n| n.starts_with("governor: level 0 -> 1")) { continue; }
        let Some(name) = str_field(l, "name") else { continue };
        if name.is_empty() || is_pinned(rules, &name) { continue; }
        *counts.entry(name.to_lowercase()).or_insert(0) += 1;
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().filter(|(_, c)| *c >= 5).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// One thing the enforcer did (or would do), for the ledger.
pub struct Applied { pub pid: u32, pub name: String, pub kind: &'static str, pub note: String, pub result: Result<(), String> }

/// Applies rules once per process (pid + create time).
pub struct Enforcer {
    done: HashSet<(u32, u64)>,
    reported: HashSet<(u32, u64)>,
    extra_deny: Vec<String>,
    /// false = dry run: nothing is changed, results are Ok.
    pub acting: bool,
}

impl Enforcer {
    pub fn new(extra_deny: Vec<String>) -> Self { Enforcer { done: HashSet::new(), reported: HashSet::new(), extra_deny, acting: false } }

    /// Forget what was applied (rules reloaded, or dry run turned into real runs).
    pub fn reset(&mut self) { self.done.clear(); self.reported.clear(); }

    fn denied(&self, p: &sys::Proc) -> Option<String> {
        let t = Target { pid: p.pid, name: p.name.clone(), age_secs: f64::MAX, cmdline: sys::cmdline(p.pid), service: None, private_mb: 0.0 };
        policy::denied(&t, &Action::LowerPriority, &self.extra_deny)
    }

    pub fn tick(&mut self, procs: &[sys::Proc], rules: &[Rule]) -> Vec<Applied> {
        let mut out = Vec::new();
        if rules.is_empty() { self.reset(); return out; }
        let live: HashSet<(u32, u64)> = procs.iter().map(|p| (p.pid, p.created)).collect();
        self.done.retain(|k| live.contains(k));
        self.reported.retain(|k| live.contains(k));
        let acting = self.acting;
        for p in procs {
            if p.pid <= 4 { continue; }
            let key = (p.pid, p.created);
            if self.done.contains(&key) { continue; }
            let matching: Vec<&Rule> = rules.iter().filter(|r| glob_match(&r.pattern, &p.name)).collect();
            if matching.is_empty() { continue; }
            self.done.insert(key);
            if let Some(why) = self.denied(p) {
                out.push(Applied { pid: p.pid, name: p.name.clone(), kind: "refused", note: why.clone(), result: Err(why) });
                continue;
            }
            let (pid, created) = key;
            let mut apply = |kind: &'static str, note: String, f: &dyn Fn() -> Result<(), String>| {
                let result = if !acting { Ok(()) } else if !sys::image_matches(pid, created, &p.name) { Err("pid reused".to_string()) } else { f() };
                out.push(Applied { pid, name: p.name.clone(), kind, note, result });
            };
            for r in matching {
                let who = &r.pattern;
                if let Some(c) = r.priority { apply("priority", format!("rule {}: priority class {:#x}", who, c), &|| sys::set_priority(pid, c)); }
                if let Some(e) = r.efficiency { apply("efficiency", format!("rule {}: efficiency mode {}", who, if e { "on" } else { "off" }), &|| sys::set_efficiency(pid, e)); }
                if let Some(v) = r.io { apply("io", format!("rule {}: I/O priority {}", who, v), &|| sys::set_io_priority(pid, v)); }
                if let Some(v) = r.memory { apply("memory", format!("rule {}: memory priority {}", who, v), &|| sys::set_memory_priority(pid, v)); }
                if let Some(m) = r.affinity {
                    apply("affinity", format!("rule {}: cores mask {:#x}", who, m), &|| {
                        let (_, system) = sys::get_affinity(pid).ok_or("cannot read affinity")?;
                        let mask = m & system;
                        if mask == 0 { return Err("none of those cores exist on this PC".into()); }
                        sys::set_affinity(pid, mask)
                    });
                }
                if let Some(c) = r.cap { apply("cap", format!("rule {}: CPU cap {}%", who, c), &|| sys::cap_cpu(pid, c)); }
            }
        }
        // max_instances: report (never kill) the newest extra copies, once each.
        for r in rules {
            let Some(n) = r.max_instances else { continue };
            let mut m: Vec<&sys::Proc> = procs.iter().filter(|p| p.pid > 4 && glob_match(&r.pattern, &p.name)).collect();
            if m.len() <= n { continue; }
            m.sort_by_key(|p| p.created);
            let total = m.len();
            for p in &m[n..] {
                let key = (p.pid, p.created);
                if !self.reported.insert(key) { continue; }
                if self.denied(p).is_some() { continue; }
                out.push(Applied { pid: p.pid, name: p.name.clone(), kind: "max_instances", note: format!("{} copies running, rule allows {}", total, n), result: Ok(()) });
            }
        }
        out
    }
}

#[cfg(test)]
mod rule_tests {
    use super::*;

    fn rule(l: &str) -> Rule { parse_line(l).unwrap().unwrap() }

    #[test]
    fn parses_all_keys() {
        let r = rule("FFmpeg*.exe priority=idle efficiency=on io=very_low memory=low affinity=0-3 cap=50 max_instances=2 # note");
        assert_eq!(r.pattern, "ffmpeg*.exe");
        assert_eq!(r.priority, Some(sys::IDLE));
        assert_eq!(r.efficiency, Some(true));
        assert_eq!(r.io, Some(0));
        assert_eq!(r.memory, Some(2));
        assert_eq!(r.affinity, Some(0xf));
        assert_eq!(r.cap, Some(50));
        assert_eq!(r.max_instances, Some(2));
    }

    #[test]
    fn background_shorthand_and_override() {
        let r = rule("x.exe background=yes");
        assert_eq!((r.efficiency, r.io, r.priority), (Some(true), Some(1), Some(sys::BELOW_NORMAL)));
        assert_eq!(rule("x.exe background=yes priority=idle").priority, Some(sys::IDLE));
    }

    #[test]
    fn affinity_forms() {
        assert_eq!(rule("a.exe affinity=ff").affinity, Some(0xff));
        assert_eq!(rule("a.exe affinity=0x0F").affinity, Some(0xf));
        assert_eq!(rule("a.exe affinity=0,2,4-5").affinity, Some(0b110101));
        assert!(parse_line("a.exe affinity=0").is_err());
        assert!(parse_line("a.exe affinity=5-2").is_err());
        assert!(parse_line("a.exe affinity=0-64").is_err());
    }

    #[test]
    fn rejects_bad_input() {
        for bad in ["a.exe", "a.exe priority=high", "a.exe cap=0", "a.exe cap=101", "a.exe max_instances=0", "a.exe nope=1", "a.exe efficiency", "priority=idle", "a.exe background=no"] {
            assert!(parse_line(bad).is_err(), "{}", bad);
        }
        assert_eq!(parse_line("  # comment").unwrap(), None);
        assert_eq!(parse_line("").unwrap(), None);
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("ffmpeg*.exe", "FFmpeg-x64.EXE"));
        assert!(glob_match("ffmpeg*.exe", "ffmpeg.exe"));
        assert!(!glob_match("ffmpeg*.exe", "ffmpeg.exe.bak"));
        assert!(glob_match("*", "anything.exe"));
        assert!(glob_match("a?c.exe", "abc.exe"));
        assert!(!glob_match("a?c.exe", "ac.exe"));
        assert!(glob_match("*hand*brake*", "my-handbrake-cli.exe"));
        assert!(!glob_match("comet.exe", "comet2.exe"));
    }

    #[test]
    fn pinned_covers_wildcards_and_files_roundtrip() {
        let rules = vec![rule("ffmpeg*.exe cap=50"), rule("obs64.exe priority=normal")];
        assert!(is_pinned(&rules, "FFMPEG-new.exe"));
        assert!(is_pinned(&rules, "obs64.exe"));
        assert!(!is_pinned(&rules, "chrome.exe"));
        let d = std::env::temp_dir().join(format!("snifrig-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(FILE), "# my rules\nffmpeg*.exe cap=50\nbroken line\nOBS64.exe priority=normal").unwrap();
        assert_eq!(load(&d).len(), 2);
        assert_eq!(pinned_names(&d), vec!["obs64.exe"]);
        assert_eq!(add(&d, "x.exe  background=yes").unwrap(), "x.exe background=yes");
        assert!(add(&d, "x.exe bogus=1").is_err());
        assert_eq!(rule_lines(&d).len(), 4);
        assert_eq!(remove(&d, 2).unwrap(), "broken line");
        assert!(remove(&d, 9).is_err());
        assert_eq!(load(&d).len(), 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn suggestions_count_recent_unruled_names() {
        let now = 1_800_000_000.0;
        let mut f = String::new();
        let line = |unix: f64, name: &str, note: &str| format!("{{\"unix\":{:.0},\"t\":\"\",\"key\":\"k\",\"action\":\"lower-priority\",\"pid\":5,\"name\":\"{}\",\"status\":\"done\",\"note\":\"{}\"}}\n", unix, name, note);
        for i in 0..5 { f.push_str(&line(now - 100.0 * i as f64, "Render.exe", "governor: level 0 -> 1: busy")); }
        for _ in 0..5 { f.push_str(&line(now - 100.0, "ruled.exe", "governor: level 0 -> 1: busy")); }
        for _ in 0..4 { f.push_str(&line(now - 100.0, "few.exe", "governor: level 0 -> 1: busy")); }
        for _ in 0..6 { f.push_str(&line(now - 100.0, "level2.exe", "governor: level 1 -> 2: busy")); }
        for _ in 0..6 { f.push_str(&line(now - 9.0 * 86400.0, "old.exe", "governor: level 0 -> 1: busy")); }
        let rules = vec![rule("ruled.exe cap=50")];
        assert_eq!(suggestions_from(&f, now, &rules), vec![("render.exe".to_string(), 5)]);
    }
}

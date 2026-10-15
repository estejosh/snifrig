//! Incident memory. A slowdown episode (the monitor says something is wrong, or it feels
//! sluggish) is fingerprinted by what was involved: alert kinds and the programs behind them
//! ("pm:vmmemwsl", "x:vram", "slow:paging", "dup:llama-server.exe", "feel:sluggish"...).
//! When it ends, snifrig records what was done during it and what most likely ended it.
//! When a new episode starts, it is compared with past ones: "this looks like Oct 12, when
//! ending the second llama-server fixed it", which the window, the popup and AI clients show.
use crate::json::{num_field, str_field};
use std::collections::BTreeSet;
use std::path::Path;

const CALM_TO_CLOSE: f64 = 600.0;
const MATCH_MIN: f64 = 0.4;
const FILE: &str = "incidents.jsonl";
const NOW_FILE: &str = "incident-now.json";
const CAP: u64 = 256 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Past { pub start: f64, pub end: f64, pub headline: String, pub tokens: BTreeSet<String>, pub fixed_by: String }

/// "pm:vmmemWSL#18508" -> "pm:vmmemwsl"; "slow:paging" stays.
pub fn token(key: &str) -> String { key.split('#').next().unwrap_or(key).to_lowercase() }

pub fn similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> (f64, usize) {
    let shared = a.intersection(b).count();
    let union = a.union(b).count().max(1);
    (shared as f64 / union as f64, shared)
}

/// Pure: the best earlier incident like this one, if any is close enough.
pub fn best_match<'a>(now: &BTreeSet<String>, past: &'a [Past]) -> Option<(&'a Past, f64)> {
    past.iter().map(|p| (p, similarity(now, &p.tokens))).filter(|(_, (j, n))| *j >= MATCH_MIN && *n >= 2)
        .max_by(|a, b| (a.1).0.partial_cmp(&(b.1).0).unwrap_or(std::cmp::Ordering::Equal).then(a.0.start.partial_cmp(&b.0.start).unwrap_or(std::cmp::Ordering::Equal)))
        .map(|(p, (j, _))| (p, j))
}

/// Pure: from ledger lines inside the incident, the action that most likely ended it: the last
/// change made (not a governor step back, not a dry run) before things calmed down.
pub fn resolution(ledger: &str, start: f64, calm_at: f64) -> String {
    let mut best = String::new();
    for l in ledger.lines() {
        let Some(u) = num_field(l, "unix") else { continue };
        if u < start || u > calm_at { continue; }
        if str_field(l, "status").as_deref() != Some("done") { continue; }
        let note = str_field(l, "note").unwrap_or_default();
        if note.contains("-> 0") || note.contains("level 1 -> 0") || note.contains("put back") { continue; }
        let name = str_field(l, "name").unwrap_or_default();
        let action = str_field(l, "action").unwrap_or_default();
        best = match action.as_str() {
            "terminate" => format!("ending {}", name),
            "restart-service" => format!("restarting the service in {}", name),
            "trim" => format!("freeing memory of {}", name),
            "lower-priority" if note.starts_with("governor") => format!("easing {} in the background", name),
            "lower-priority" => format!("lowering the priority of {}", name),
            _ => continue,
        };
    }
    best
}

fn esc(s: &str) -> String { crate::json::esc(s) }

/// Seconds since 1970 for "YYYY-MM-DDTHH:MM:SSZ".
pub fn iso_unix(t: &str) -> Option<f64> {
    let n = |a: usize, l: usize| t.get(a..a + l)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0, 4)?, n(5, 2)?, n(8, 2)?, n(11, 2)?, n(14, 2)?, n(17, 2)?);
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + (153 * m2 + 2) / 5 + d - 1;
    Some(((era * 146097 + doe - 719468) * 86400 + hh * 3600 + mm * 60 + ss) as f64)
}

/// Fingerprint tokens from the monitor's alerts of the last `secs` seconds (reads only the file's tail).
pub fn recent_alert_tokens(dir: &Path, now: f64, secs: f64) -> BTreeSet<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut out = BTreeSet::new();
    let Ok(mut f) = std::fs::File::open(dir.join("alerts.jsonl")) else { return out };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(32 * 1024)));
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    for l in s.lines() {
        let (Some(t), Some(k)) = (str_field(l, "t"), str_field(l, "key")) else { continue };
        if iso_unix(&t).map_or(false, |u| now - u <= secs) && k != "self" { out.insert(token(&k)); }
    }
    out
}

/// Append one line; when the file passes CAP, keep its newer half.
fn append(path: &Path, line: &str) {
    use std::io::Write;
    if std::fs::metadata(path).map_or(false, |m| m.len() > CAP) {
        if let Ok(t) = std::fs::read_to_string(path) { let keep: Vec<&str> = t.lines().collect(); let half = keep[keep.len() / 2..].join("\n"); crate::rules::write_atomic(path, &(half + "\n")); }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) { let _ = writeln!(f, "{}", line); }
}

fn load_past(dir: &Path) -> Vec<Past> {
    std::fs::read_to_string(dir.join(FILE)).unwrap_or_default().lines().filter_map(|l| {
        let toks = l.split("\"tokens\":[").nth(1)?.split(']').next()?;
        Some(Past {
            start: num_field(l, "start")?, end: num_field(l, "end").unwrap_or(0.0),
            headline: str_field(l, "headline").unwrap_or_default(), fixed_by: str_field(l, "fixed_by").unwrap_or_default(),
            tokens: toks.split(',').map(|t| t.trim().trim_matches('"').to_string()).filter(|t| !t.is_empty()).collect(),
        })
    }).collect()
}

struct Open { start: f64, tokens: BTreeSet<String>, headline: String, calm_since: Option<f64>, told: bool }

#[derive(Default)]
pub struct Tracker { open: Option<Open> }

impl Tracker {
    pub fn new() -> Self { Tracker { open: None } }

    /// Call every ~10 s. `trouble` = the monitor's current severity >= 1 or it feels sluggish.
    /// Returns a note for the ledger when something worth saying happens.
    pub fn tick(&mut self, dir: &Path, now: f64, trouble: bool, headline: &str, fresh_tokens: BTreeSet<String>, date: &dyn Fn(f64) -> String) -> Option<String> {
        if self.open.is_none() {
            if !trouble { return None; }
            self.open = Some(Open { start: now, tokens: fresh_tokens, headline: headline.to_string(), calm_since: None, told: false });
            return self.check_match(dir, date);
        }
        if trouble {
            let o = self.open.as_mut()?;
            let grew = fresh_tokens.iter().any(|t| !o.tokens.contains(t));
            o.tokens.extend(fresh_tokens);
            o.calm_since = None;
            if !headline.is_empty() { o.headline = headline.to_string(); }
            let ask = grew && !o.told;
            return if ask { self.check_match(dir, date) } else { None };
        }
        let calm = { let o = self.open.as_mut()?; *o.calm_since.get_or_insert(now) };
        if now - calm < CALM_TO_CLOSE { return None; }
        let o = self.open.take()?;
        let _ = std::fs::remove_file(dir.join(NOW_FILE));
        if o.tokens.is_empty() { return None; }
        let mut ledger = std::fs::read_to_string(dir.join("fixes.jsonl.old")).unwrap_or_default();
        ledger.push_str(&std::fs::read_to_string(dir.join("fixes.jsonl")).unwrap_or_default());
        let fixed_by = resolution(&ledger, o.start, calm + 30.0);
        let toks: Vec<String> = o.tokens.iter().map(|t| format!("\"{}\"", esc(t))).collect();
        let line = format!("{{\"start\":{:.0},\"end\":{:.0},\"minutes\":{:.0},\"headline\":\"{}\",\"fixed_by\":\"{}\",\"tokens\":[{}]}}",
            o.start, calm, (calm - o.start) / 60.0, esc(&o.headline), esc(&fixed_by), toks.join(","));
        append(&dir.join(FILE), &line);
        Some(format!("incident over after {:.0} min: {}{}", (calm - o.start) / 60.0, o.headline, if fixed_by.is_empty() { String::new() } else { format!("; most likely ended by {}", fixed_by) }))
    }

    fn check_match(&mut self, dir: &Path, date: &dyn Fn(f64) -> String) -> Option<String> {
        let o = self.open.as_mut()?;
        let past = load_past(dir);
        let (p, j) = best_match(&o.tokens, &past)?;
        o.told = true;
        let mins = ((p.end - p.start) / 60.0).max(1.0);
        let text = format!("This looks like {} ({}), which lasted {:.0} min{}.", date(p.start), if p.headline.is_empty() { "a slowdown" } else { p.headline.as_str() }, mins,
            if p.fixed_by.is_empty() { String::new() } else { format!("; {} ended it then", p.fixed_by) });
        let shared: Vec<String> = o.tokens.intersection(&p.tokens).map(|t| format!("\"{}\"", esc(t))).collect();
        let body = format!("{{\"start\":{:.0},\"similar_to\":{:.0},\"similarity\":{:.2},\"text\":\"{}\",\"fixed_by\":\"{}\",\"shared\":[{}]}}\n", o.start, p.start, j, esc(&text), esc(&p.fixed_by), shared.join(","));
        crate::rules::write_atomic(&dir.join(NOW_FILE), &body);
        Some(format!("seen before: {}", text))
    }
}

#[cfg(test)]
mod incident_tests {
    use super::*;
    fn set(v: &[&str]) -> BTreeSet<String> { v.iter().map(|s| s.to_string()).collect() }
    #[test]
    fn matches_a_similar_past_incident() {
        let past = vec![
            Past { start: 100.0, end: 1300.0, headline: "WSL holds 22 GB".into(), tokens: set(&["pm:vmmemwsl", "slow:paging", "x:vram", "dup:llama-server.exe"]), fixed_by: "ending llama-server.exe".into() },
            Past { start: 5000.0, end: 5600.0, headline: "Comet busy".into(), tokens: set(&["cpu:comet.exe", "feel:sluggish"]), fixed_by: String::new() },
        ];
        let (p, j) = best_match(&set(&["pm:vmmemwsl", "slow:paging", "x:vram"]), &past).unwrap();
        assert_eq!(p.headline, "WSL holds 22 GB");
        assert!(j >= 0.7);
        assert!(best_match(&set(&["cpu:comet.exe"]), &past).is_none(), "one shared signal is not enough");
        assert_eq!(token("pm:vmmemWSL#18508"), "pm:vmmemwsl");
    }
    #[test]
    fn finds_what_ended_it() {
        let ledger = "{\"unix\":150,\"key\":\"gov:x#1\",\"action\":\"lower-priority\",\"name\":\"node.exe\",\"status\":\"done\",\"note\":\"governor: level 0 -> 1: busy\"}\n\
{\"unix\":400,\"key\":\"dup:llama\",\"action\":\"terminate\",\"name\":\"llama-server.exe\",\"status\":\"done\",\"note\":\"approved: ended\"}\n\
{\"unix\":500,\"key\":\"gov:x#1\",\"action\":\"report\",\"name\":\"node.exe\",\"status\":\"done\",\"note\":\"governor: level 1 -> 0: calm\"}\n\
{\"unix\":900,\"key\":\"x\",\"action\":\"trim\",\"name\":\"late.exe\",\"status\":\"done\",\"note\":\"after calm\"}\n";
        assert_eq!(resolution(ledger, 100.0, 600.0), "ending llama-server.exe");
    }
}

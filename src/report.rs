//! `snifrig report [--days N]`: a plain summary of what the monitor saw, and what snifrig fix did
//! or would have done about it. Read-only: it only reads the monitor's own log files.
use std::collections::HashMap;
use std::path::Path;

/// Seconds since 1970 for "YYYY-MM-DDTHH:MM:SSZ".
pub fn iso_unix(t: &str) -> Option<f64> {
    let b = t.as_bytes();
    if b.len() < 19 { return None; }
    let n = |a: usize, l: usize| t.get(a..a + l)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0, 4)?, n(5, 2)?, n(8, 2)?, n(11, 2)?, n(14, 2)?, n(17, 2)?);
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + hh * 3600 + mm * 60 + ss) as f64)
}

fn field<'a>(l: &'a str, k: &str) -> Option<&'a str> {
    let p = format!("\"{}\":\"", k);
    let i = l.find(&p)? + p.len();
    let j = l[i..].find('"')?;
    Some(&l[i..i + j])
}

#[derive(Default)]
struct Bucket { count: usize, names: HashMap<String, usize> }

impl Bucket {
    fn add(&mut self, name: &str) { self.count += 1; if !name.is_empty() { *self.names.entry(name.to_string()).or_insert(0) += 1; } }
    fn top(&self) -> String {
        let mut v: Vec<(&String, &usize)> = self.names.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.iter().take(4).map(|(n, c)| format!("{} ({})", n, c)).collect::<Vec<_>>().join(", ")
    }
}

/// Pure: build the report text from the log contents.
pub fn build(alerts: &str, fixes: &str, slow_lines: &[String], now: f64, days: f64) -> String {
    let since = now - days * 86400.0;
    let mut b: HashMap<&'static str, Bucket> = HashMap::new();
    for l in alerts.lines() {
        let (Some(t), Some(key)) = (field(l, "t"), field(l, "key")) else { continue };
        if iso_unix(t).map_or(true, |u| u < since) { continue; }
        let (pre, rest) = key.split_once(':').unwrap_or((key, ""));
        let name = rest.split('#').next().unwrap_or("");
        let cat = match pre {
            "cpu" => "cpu", "dup" | "dupq" => "dup", "pm" | "ph" => "leak", "spawner" => "spawn",
            "x" if name == "vram" => "vram", "x" => "mem", "tag" | "big" | "g" => "kernel", _ => continue,
        };
        b.entry(cat).or_default().add(name);
    }
    let get = |k: &str| b.get(k).filter(|x| x.count > 0);
    let mut out = format!("snifrig report, last {} day{}\n", days, if days == 1.0 { "" } else { "s" });
    let mut any = false;
    let mut line = |out: &mut String, head: String, fix: &str| { any = true; out.push_str(&format!("\n- {}\n    snifrig fix: {}\n", head, fix)); };
    if let Some(x) = get("cpu") { line(&mut out, format!("Programs hogging the CPU: {} times. Most: {}.", x.count, x.top()), "eases background hogs in steps within seconds (efficiency mode, then lower priority), never the app you are using, and puts them back when the PC is calm."); }
    if let Some(x) = get("dup") { line(&mut out, format!("Duplicate copies of the same program: {} times. Most: {}.", x.count, x.top()), "offers to close the idle copy, with your OK."); }
    if let Some(x) = get("leak") { line(&mut out, format!("Programs whose memory or handles kept growing (possible leaks): {} times. Most: {}.", x.count, x.top()), "trims or restarts the program for you, asking first unless you allow it."); }
    if let Some(x) = get("spawn") { line(&mut out, format!("Programs starting processes in a loop: {} times. Most: {}.", x.count, x.top()), "slows the one doing it."); }
    if let Some(x) = get("vram") { line(&mut out, format!("Graphics memory full: {} times.", x.count), "names the biggest graphics users and can close a duplicate that is wasting it."); }
    if let Some(x) = get("mem") { line(&mut out, format!("System memory low or overcommitted: {} times.", x.count), "trims the biggest idle programs."); }
    if let Some(x) = get("kernel") { out.push_str(&format!("\n- Windows kernel memory grew past normal: {} times (the monitor names the driver tags; nothing to fix automatically).\n", x.count)); any = true; }
    if !slow_lines.is_empty() {
        any = true;
        out.push_str("\n- Windows itself slowing the PC (evidence):\n");
        for s in slow_lines { out.push_str(&format!("    {}\n", s)); }
    }
    if !any { out.push_str("\nNothing slowed this PC down in that time.\n"); }

    // What the fixer actually did, if it is installed.
    let (mut eased, mut rule, mut perf, mut would) = (0usize, 0usize, 0usize, 0usize);
    for l in fixes.lines() {
        let Some(u) = l.find("\"unix\":").and_then(|i| l[i + 7..].split(|c: char| c == ',' || c == '}').next()?.parse::<f64>().ok()) else { continue };
        if u < since { continue; }
        let (key, status, note) = (field(l, "key").unwrap_or(""), field(l, "status").unwrap_or(""), field(l, "note").unwrap_or(""));
        match status {
            "done" if note.starts_with("governor: level 0 -> 1") => eased += 1,
            "done" if key.starts_with("rule:") => rule += 1,
            "done" if key.starts_with("perf:") && note.contains(": on for") => perf += 1,
            "would" => would += 1,
            _ => {}
        }
    }
    if eased + rule + perf > 0 {
        out.push_str(&format!("\nsnifrig fix on this PC: eased {} busy background apps, applied your rules {} times, ran performance mode {} times.\n", eased, rule, perf));
    } else if would > 0 {
        out.push_str(&format!("\nsnifrig fix (dry run) would have acted {} times.\n", would));
    }
    out
}

pub fn run(dir: &Path, days: f64) -> String {
    let mut alerts = std::fs::read_to_string(dir.join("alerts.old")).unwrap_or_default();
    alerts.push_str(&std::fs::read_to_string(dir.join("alerts.jsonl")).unwrap_or_default());
    let fixes = std::fs::read_to_string(dir.join("fixes.jsonl")).unwrap_or_default();
    let slow = if days >= 1.0 { super::slowdown::summarize(dir) } else { Vec::new() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    build(&alerts, &fixes, &slow, now, days)
}

#[cfg(test)]
mod report_tests {
    use super::*;
    #[test]
    fn iso_dates() {
        assert_eq!(iso_unix("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(iso_unix("2026-10-11T22:01:38Z"), Some(1791756098.0));
    }
    #[test]
    fn groups_and_counts() {
        let now = iso_unix("2026-10-11T22:00:00Z").unwrap();
        let a = "{\"t\":\"2026-10-11T21:00:00Z\",\"key\":\"cpu:comet.exe#1\",\"msg\":\"x\"}\n{\"t\":\"2026-10-11T21:10:00Z\",\"key\":\"cpu:comet.exe#2\",\"msg\":\"x\"}\n{\"t\":\"2026-09-01T00:00:00Z\",\"key\":\"cpu:old.exe#3\",\"msg\":\"x\"}\n{\"t\":\"2026-10-11T21:00:00Z\",\"key\":\"dupq:llama-server.exe\",\"msg\":\"x\"}\n";
        let f = format!("{{\"unix\":{},\"key\":\"gov:a#1\",\"status\":\"done\",\"note\":\"governor: level 0 -> 1: busy\"}}\n", now - 60.0);
        let r = build(a, &f, &[], now, 7.0);
        assert!(r.contains("hogging the CPU: 2 times. Most: comet.exe (2)"), "{}", r);
        assert!(!r.contains("old.exe"));
        assert!(r.contains("Duplicate copies") && r.contains("eased 1 busy"));
        assert!(build("", "", &[], now, 7.0).contains("Nothing slowed"));
    }
}

//! Files the fixer reads and writes in the snifrig data dir. All writes go through
//! write-to-temp-then-rename except appends. Use crate::json helpers.
//!
//! fixes.jsonl  append-only log, one object per line, capped at 512 KB (rotate to fixes.jsonl.old):
//!   {"unix":N,"t":"<alert t>","key":"..","action":"<Action::label>","pid":N,"name":"..",
//!    "status":"done|failed|queued|would|refused|approved|dismissed","note":".."}
//!   pid 0 and name "" when there is no target.
//! pending.json  {"items":[{"id":"..","unix":N,"key":"..","msg":"..","action":"..","pid":N,"name":"..","why":".."},...]}
//!   at most 20 items (drop oldest); ids are short unique strings (e.g. hex of unix millis + pid).
//!   queue() skips an item if one with the same pid and action label is already pending.
//! fixer-state.json  {"offset":N}  byte offset into alerts.jsonl already processed.
//! fix-mode.txt  one word: off | dry-run | ask | auto. Missing or unknown -> dry-run.
//! fix-allow.txt / fix-deny.txt  one entry per line, '#' comments, lowercased on read.
//! mode.json (written by `snifrig pause`)  {"paused_until":N}; 0 if missing.

use crate::json::{esc, num_field, str_field};
use crate::{Action, Alert, Mode, Target};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const FIXES_CAP: u64 = 512 * 1024;
const PENDING_CAP: usize = 20;
const RECENT_AUTO_SECS: f64 = 7200.0;

#[derive(Clone, Debug, Default)]
pub struct Pending { pub id: String, pub unix: f64, pub key: String, pub msg: String, pub action: String, pub pid: u32, pub name: String, pub why: String }

fn read_text(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default()
}

/// Write to "<path>.tmp" then rename over the target.
fn write_atomic(path: &Path, content: &str) {
    if let Some(d) = path.parent() {
        let _ = fs::create_dir_all(d);
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if fs::write(&tmp, content).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

pub fn record(dir: &Path, alert: &Alert, action: &Action, target: Option<&Target>, status: &str, note: &str) {
    let _ = fs::create_dir_all(dir);
    let (pid, name) = match target {
        Some(t) => (t.pid, t.name.as_str()),
        None => (0, ""),
    };
    let line = format!(
        "{{\"unix\":{:.0},\"t\":\"{}\",\"key\":\"{}\",\"action\":\"{}\",\"pid\":{},\"name\":\"{}\",\"status\":\"{}\",\"note\":\"{}\"}}\n",
        crate::now_unix(),
        esc(&alert.t),
        esc(&alert.key),
        esc(&action.label()),
        pid,
        esc(name),
        esc(status),
        esc(note)
    );
    let path = dir.join("fixes.jsonl");
    if fs::metadata(&path).map(|m| m.len() > FIXES_CAP).unwrap_or(false) {
        let _ = fs::rename(&path, dir.join("fixes.jsonl.old"));
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Unix times of entries with status "done" whose note starts with "auto" in the last 2 hours.
pub fn recent_auto(dir: &Path) -> Vec<f64> {
    let cutoff = crate::now_unix() - RECENT_AUTO_SECS;
    read_text(&dir.join("fixes.jsonl"))
        .lines()
        .filter_map(|l| {
            let unix = num_field(l, "unix")?;
            let done = str_field(l, "status").as_deref() == Some("done");
            let auto = str_field(l, "note").map_or(false, |n| n.starts_with("auto"));
            (done && auto && unix >= cutoff).then_some(unix)
        })
        .collect()
}

/// Returns the new item's id, or None if an item with the same pid and action is already pending.
pub fn queue(dir: &Path, alert: &Alert, action: &Action, t: &Target, why: &str) -> Option<String> {
    let label = action.label();
    let mut items = load_pending(dir);
    if items.iter().any(|p| p.pid == t.pid && p.action == label) {
        return None;
    }
    let unix = crate::now_unix();
    let id = format!("{:x}-{}-{}", (unix * 1000.0) as u64, t.pid, items.len());
    items.push(Pending {
        id: id.clone(),
        unix,
        key: alert.key.clone(),
        msg: alert.msg.clone(),
        action: label,
        pid: t.pid,
        name: t.name.clone(),
        why: why.to_string(),
    });
    save_pending(dir, &items);
    Some(id)
}

/// Splits the body of a JSON array of flat objects into the object texts.
/// Tracks string state, so braces inside quoted values are ignored.
fn split_objects(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    let (mut in_str, mut esc_next) = (false, false);
    for (i, c) in s.char_indices() {
        if in_str {
            if esc_next {
                esc_next = false;
            } else if c == '\\' {
                esc_next = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_item(o: &str) -> Option<Pending> {
    Some(Pending {
        id: str_field(o, "id")?,
        unix: num_field(o, "unix").unwrap_or(0.0),
        key: str_field(o, "key").unwrap_or_default(),
        msg: str_field(o, "msg").unwrap_or_default(),
        action: str_field(o, "action").unwrap_or_default(),
        pid: num_field(o, "pid").unwrap_or(0.0) as u32,
        name: str_field(o, "name").unwrap_or_default(),
        why: str_field(o, "why").unwrap_or_default(),
    })
}

pub fn load_pending(dir: &Path) -> Vec<Pending> {
    let s = read_text(&dir.join("pending.json"));
    let pat = "\"items\":[";
    let body = match s.find(pat) {
        Some(i) => &s[i + pat.len()..],
        None => return Vec::new(),
    };
    split_objects(body).into_iter().filter_map(parse_item).collect()
}

/// Writes the last 20 items; anything older is dropped.
pub fn save_pending(dir: &Path, items: &[Pending]) {
    let start = items.len().saturating_sub(PENDING_CAP);
    let mut s = String::from("{\"items\":[");
    for (i, p) in items[start..].iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"id\":\"{}\",\"unix\":{},\"key\":\"{}\",\"msg\":\"{}\",\"action\":\"{}\",\"pid\":{},\"name\":\"{}\",\"why\":\"{}\"}}",
            esc(&p.id),
            p.unix,
            esc(&p.key),
            esc(&p.msg),
            esc(&p.action),
            p.pid,
            esc(&p.name),
            esc(&p.why)
        ));
    }
    s.push_str("]}\n");
    write_atomic(&dir.join("pending.json"), &s);
}

pub fn read_offset(dir: &Path) -> u64 {
    num_field(&read_text(&dir.join("fixer-state.json")), "offset").map_or(0, |n| n as u64)
}

pub fn write_offset(dir: &Path, off: u64) {
    write_atomic(&dir.join("fixer-state.json"), &format!("{{\"offset\":{}}}\n", off));
}

pub fn read_mode(dir: &Path) -> Mode {
    Mode::parse(&read_text(&dir.join("fix-mode.txt")).to_lowercase()).unwrap_or(Mode::DryRun)
}

pub fn write_mode(dir: &Path, m: Mode) {
    write_atomic(&dir.join("fix-mode.txt"), &format!("{}\n", m.as_str()));
}

/// Non-empty lines that are not '#' comments, trimmed and lowercased.
pub fn read_list(dir: &Path, file: &str) -> Vec<String> {
    read_text(&dir.join(file))
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

pub fn paused_until(dir: &Path) -> f64 {
    num_field(&read_text(&dir.join("mode.json")), "paused_until").unwrap_or(0.0)
}

/// New complete lines of alerts.jsonl after `offset`, and the new offset. If the file is
/// shorter than offset (rotated), start from 0. Ignore a trailing partial line.
pub fn new_alerts(dir: &Path, offset: u64) -> (Vec<Alert>, u64) {
    let data = match fs::read(dir.join("alerts.jsonl")) {
        Ok(d) => d,
        Err(_) => return (Vec::new(), 0),
    };
    let mut start = offset as usize;
    if data.len() < start {
        start = 0;
    }
    let rest = &data[start..];
    let end = match rest.iter().rposition(|&b| b == b'\n') {
        Some(p) => p + 1,
        None => return (Vec::new(), start as u64),
    };
    let text = String::from_utf8_lossy(&rest[..end]);
    let alerts = text
        .lines()
        .filter_map(|l| {
            let key = str_field(l, "key")?;
            Some(Alert {
                t: str_field(l, "t").unwrap_or_default(),
                key,
                msg: str_field(l, "msg").unwrap_or_default(),
            })
        })
        .collect();
    (alerts, (start + end) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("snifrig-ledger-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn alert(key: &str) -> Alert {
        Alert { t: "2026-10-07T12:00:00Z".into(), key: key.into(), msg: "m".into() }
    }

    fn target(pid: u32) -> Target {
        Target { pid, name: "x.exe".into(), ..Default::default() }
    }

    #[test]
    fn pending_roundtrip() {
        let d = tmp("pending");
        let tricky = "he said \"hi\", ok},{x\\y\nnew\tline";
        let items = vec![Pending {
            id: "a1".into(),
            unix: 1791296313.5,
            key: "pm:a#1".into(),
            msg: tricky.into(),
            action: "trim".into(),
            pid: 42,
            name: "we,ird\".exe".into(),
            why: "a, \"b\"".into(),
        }];
        save_pending(&d, &items);
        let back = load_pending(&d);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].msg, tricky);
        assert_eq!(back[0].why, "a, \"b\"");
        assert_eq!(back[0].name, "we,ird\".exe");
        assert_eq!(back[0].pid, 42);
        assert_eq!(back[0].id, "a1");
        assert_eq!(back[0].unix, 1791296313.5);
    }

    #[test]
    fn queue_dedupes_same_pid_and_action() {
        let d = tmp("dedupe");
        let a = alert("k");
        assert!(queue(&d, &a, &Action::Trim, &target(7), "why").is_some());
        assert!(queue(&d, &a, &Action::Trim, &target(7), "again").is_none());
        assert!(queue(&d, &a, &Action::Terminate, &target(7), "other action").is_some());
        assert!(queue(&d, &a, &Action::Trim, &target(8), "other pid").is_some());
        assert_eq!(load_pending(&d).len(), 3);
    }

    #[test]
    fn pending_capped_at_20_dropping_oldest() {
        let d = tmp("cap");
        for pid in 1..=25u32 {
            assert!(queue(&d, &alert("k"), &Action::Trim, &target(pid), "w").is_some());
        }
        let items = load_pending(&d);
        assert_eq!(items.len(), 20);
        assert_eq!(items[0].pid, 6);
        assert_eq!(items[19].pid, 25);
    }

    #[test]
    fn offset_roundtrip() {
        let d = tmp("offset");
        assert_eq!(read_offset(&d), 0);
        write_offset(&d, 777);
        assert_eq!(read_offset(&d), 777);
    }

    #[test]
    fn new_alerts_ignores_partial_last_line() {
        let d = tmp("alerts");
        let l1 = "{\"t\":\"t1\",\"key\":\"pm:a#1\",\"msg\":\"one\"}\n";
        let l2 = "{\"t\":\"t2\",\"key\":\"pm:b#2\",\"msg\":\"two, \\\"q\\\"\"}\n";
        let partial = "{\"t\":\"t3\",\"key\":\"pm:c#3\",\"msg\":\"thr";
        fs::write(d.join("alerts.jsonl"), format!("{}{}{}", l1, l2, partial)).unwrap();
        let (a, off) = new_alerts(&d, 0);
        assert_eq!(a.len(), 2);
        assert_eq!(a[1].msg, "two, \"q\"");
        assert_eq!(off, (l1.len() + l2.len()) as u64);
        let mut f = fs::OpenOptions::new().append(true).open(d.join("alerts.jsonl")).unwrap();
        f.write_all(b"ee\"}\n").unwrap();
        let (a2, off2) = new_alerts(&d, off);
        assert_eq!(a2.len(), 1);
        assert_eq!(a2[0].key, "pm:c#3");
        assert_eq!(a2[0].msg, "three");
        assert_eq!(off2, (l1.len() + l2.len() + partial.len() + 5) as u64);
    }

    #[test]
    fn new_alerts_restarts_after_rotation() {
        let d = tmp("rotate");
        let line = "{\"t\":\"t\",\"key\":\"k\",\"msg\":\"m\"}\n";
        fs::write(d.join("alerts.jsonl"), line.repeat(2)).unwrap();
        let (a, off) = new_alerts(&d, 100_000);
        assert_eq!(a.len(), 2);
        assert_eq!(off, (line.len() * 2) as u64);
    }

    #[test]
    fn mode_defaults_and_roundtrips() {
        let d = tmp("mode");
        assert_eq!(read_mode(&d), Mode::DryRun);
        write_mode(&d, Mode::Auto);
        assert_eq!(read_mode(&d), Mode::Auto);
        write_mode(&d, Mode::Ask);
        assert_eq!(read_mode(&d), Mode::Ask);
    }

    #[test]
    fn list_skips_comments_and_lowercases() {
        let d = tmp("list");
        fs::write(d.join("fix-allow.txt"), "# header\n\nFoo.EXE\n  bar.exe  \n#skip.exe\n").unwrap();
        assert_eq!(read_list(&d, "fix-allow.txt"), vec!["foo.exe", "bar.exe"]);
        assert!(read_list(&d, "missing.txt").is_empty());
    }

    #[test]
    fn paused_until_reads_mode_json() {
        let d = tmp("pause");
        assert_eq!(paused_until(&d), 0.0);
        fs::write(d.join("mode.json"), "{\"paused_until\":1791296313}").unwrap();
        assert_eq!(paused_until(&d), 1791296313.0);
    }
}

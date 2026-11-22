//! 30 days of history. The live logs stay small (fixes.jsonl, learn.jsonl roll over to .old at a
//! few hundred KB); before a .old file is replaced, it is kept in `logs\` under its date, so a
//! month of what snifrig did and whether it helped stays on disk to analyze. Older than 30 days,
//! or past 200 MB in total, the oldest go first.
use std::path::Path;

pub const DIR: &str = "logs";
pub const DAYS: f64 = 30.0;
pub const MAX_BYTES: u64 = 200 << 20;

/// Roll `live` over to `<live>.old`, keeping the previous .old in logs\ first.
pub fn roll(dir: &Path, live: &Path) {
    let name = live.file_name().and_then(|n| n.to_str()).unwrap_or("log").to_string();
    let old = dir.join(format!("{}.old", name));
    if old.exists() {
        let logs = dir.join(DIR);
        let _ = std::fs::create_dir_all(&logs);
        let stem = name.trim_end_matches(".jsonl");
        let _ = std::fs::rename(&old, logs.join(format!("{}-{:.0}.jsonl", stem, crate::now_unix())));
        prune(&logs, crate::now_unix());
    }
    let _ = std::fs::rename(live, &old);
}

/// Drop archives older than 30 days, then the oldest until the folder is under 200 MB.
pub fn prune(logs: &Path, now: f64) {
    let mut v: Vec<(f64, u64, std::path::PathBuf)> = std::fs::read_dir(logs).map(|d| d.flatten().filter_map(|e| {
        let m = e.metadata().ok()?;
        let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64();
        Some((t, m.len(), e.path()))
    }).collect()).unwrap_or_default();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total: u64 = v.iter().map(|x| x.1).sum();
    for (t, len, p) in v {
        if now - t > DAYS * 86400.0 || total > MAX_BYTES {
            if std::fs::remove_file(&p).is_ok() { total = total.saturating_sub(len); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_the_previous_old_file() {
        let d = std::env::temp_dir().join(format!("snif-arch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let live = d.join("fixes.jsonl");
        std::fs::write(&live, "a\n").unwrap();
        roll(&d, &live);
        std::fs::write(&live, "b\n").unwrap();
        roll(&d, &live);
        assert_eq!(std::fs::read_to_string(d.join("fixes.jsonl.old")).unwrap(), "b\n");
        let kept: Vec<_> = std::fs::read_dir(d.join(DIR)).unwrap().flatten().collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read_to_string(kept[0].path()).unwrap(), "a\n");
        let _ = std::fs::remove_dir_all(&d);
    }
}

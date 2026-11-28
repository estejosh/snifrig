//! Installed programs: which are needed, which are used, which are extra (27 Nov, Josh: "go
//! through all programs and figure out if they are wanted, necessary, extraneous, uninstallable").
//!
//! Evidence per program, all local:
//! - installed list (Windows' "Apps" list: the Uninstall registry keys, machine and user)
//! - running now (a process runs from its folder)
//! - last time it was opened from the Start menu or desktop (Explorer's UserAssist record)
//! - last time snifrig saw it running (seen.tsv, written by the fixer every 10 minutes, so
//!   background and command-line tools count too; this history grows from the day it is on)
//! - Steam's own last-played date for games
//! - what the user or an AI session decided (program-verdicts.tsv: keep | remove | needed)
//!
//! Judging is pure (`judge`) and tested. Nothing is removed here; removal runs the program's own
//! uninstaller, only when the user asks, and Windows asks the user for permission.

use std::collections::HashMap;
use std::path::Path;

pub const SEEN: &str = "seen.tsv";
pub const VERDICTS: &str = "program-verdicts.tsv";
const DAY: f64 = 86400.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    /// yyyymmdd as Windows records it, or "".
    pub installed: String,
    pub size_mb: f64,
    /// Lowercase folder, no trailing backslash ("" when unknown).
    pub location: String,
    pub uninstall: String,
    pub quiet_uninstall: String,
    /// Unix time it was last known to run (0 = no record), and where that came from.
    pub last_used: f64,
    pub used_how: String,
    pub running: bool,
    pub verdict: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind { Needed, InUse, Game, Extra, Unclear }

impl Kind {
    pub fn word(self) -> &'static str {
        match self { Kind::Needed => "needed", Kind::InUse => "in use", Kind::Game => "game", Kind::Extra => "probably not needed", Kind::Unclear => "not sure yet" }
    }
}

/// Runtimes and drivers other programs depend on. Removing them breaks things that look unrelated.
const NEEDED: &[&str] = &["visual c++", "redistributable", ".net runtime", "desktop runtime", "asp.net core", "windows app runtime", "webview2",
    "directx", "gameinput", "update health", "chipset", "driver", "nvidia", "realtek", "intel(r) management", "physx", "vulkan", "openal",
    "compatibility fix", "windows sdk", "microsoft edge", "bonjour", "visual studio installer", "snifrig"];

/// Hardware makers' helper apps: lighting, fan, mouse and update tools. Often installed by a
/// device once and never needed again.
const VENDOR: &[&str] = &["asus", "armoury", "aura", "rog ", "razer", "logitech", "logi ", "dell ", "hp ", "lenovo", "galax", "msi ", "corsair",
    "samsung", "crucial", "ballistix", "gamecaster", "overwolf", "driver easy", "driver booster", "jabra", "epson", "nzxt", "steelseries", "ryzen master",
    "processor identification", "ai suite", "ai charger"];

/// Developer tools and databases: projects (virtual environments, builds, data folders) depend on
/// a particular version, so an older copy next to a newer one is not a sign it can go.
const DEV: &[&str] = &["python", "jdk", " java", "postgresql", "mysql", "mariadb", "node.js", ".net sdk", "development kit", "driver kit", "visual studio",
    "build tools", "go programming", "llvm", " rust", " git ", "github", "cmake", "android studio", " unity ", "docker", "podman", " wsl", "subsystem for linux", "anaconda", "language pack"];

/// Software that rides along with other installs (antivirus trials, "advisor" add-ons, driver
/// updaters). Running is not a sign anyone wants it.
const BUNDLED: &[&str] = &["mcafee", "norton", "webadvisor", "driver easy", "driver booster", "segurazo", "bytefence", "wondershare helper", "avg secure", "opera gx"];

fn has(name: &str, list: &[&str]) -> bool { let n = format!(" {} ", name.to_ascii_lowercase().replace(['(', ')', ':', ','], " ")); list.iter().any(|w| n.contains(w)) }

/// Pure: the verdict and the plain reason for it.
pub fn judge(p: &Program, now: f64, history_days: f64, older_twin: bool) -> (Kind, String) {
    let ago = |t: f64| { let d = ((now - t) / DAY).floor(); if d < 1.0 { "today".to_string() } else if d < 2.0 { "yesterday".into() } else { format!("{:.0} days ago", d) } };
    let used = if p.last_used > 0.0 { format!("last used {} ({})", ago(p.last_used), p.used_how) } else { "no record of it being used".to_string() };
    match p.verdict.as_str() {
        "keep" => return (Kind::InUse, "you said keep it".into()),
        "needed" => return (Kind::Needed, "marked as needed".into()),
        "remove" => return (Kind::Extra, "you said it can go".into()),
        _ => {}
    }
    if has(&p.name, BUNDLED) { return (Kind::Extra, format!("bundled software that usually comes along with another install; {}", if p.running { "running right now".to_string() } else { used.clone() })); }
    if p.running { return (Kind::InUse, "running right now".into()); }
    if has(&p.name, NEEDED) && !has(&p.name, VENDOR) && !has(&p.name, &["kit"]) { return (Kind::Needed, "other programs or your hardware depend on it".into()); }
    let recent = p.last_used > 0.0 && now - p.last_used < 30.0 * DAY;
    if p.id.starts_with("Steam App ") { return (Kind::Game, if p.last_used > 0.0 { format!("Steam game, last played {}", ago(p.last_used)) } else { "Steam game, never played on this PC".into() }); }
    if recent { return (Kind::InUse, used); }
    if has(&p.name, DEV) { return (Kind::Unclear, format!("a developer tool or database; projects may rely on this exact version; {}", used)); }
    if has(&p.name, &["print"]) { return (Kind::Unclear, "printer software: keep it if you print to this printer".into()); }
    if older_twin { return (Kind::Extra, format!("a newer version is also installed; {}", used)); }
    if has(&p.name, VENDOR) { return (Kind::Extra, format!("a hardware maker's helper app; {}", used)); }
    if p.last_used > 0.0 && now - p.last_used > 180.0 * DAY { return (Kind::Extra, used); }
    // snifrig has watched long enough to know it never runs.
    if p.last_used == 0.0 && history_days >= 60.0 && installed_days(&p.installed, now) > 60.0 { return (Kind::Extra, format!("not seen running in {:.0} days of watching", history_days)); }
    (Kind::Unclear, used)
}

fn installed_days(yyyymmdd: &str, now: f64) -> f64 {
    let d: String = yyyymmdd.chars().filter(|c| c.is_ascii_digit()).collect();
    if d.len() != 8 { return f64::MAX; }
    let (y, m, dd) = (d[..4].parse::<i64>().unwrap_or(1970), d[4..6].parse::<i64>().unwrap_or(1), d[6..].parse::<i64>().unwrap_or(1));
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + dd - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    (now / DAY) - days as f64
}

/// Pure: names that differ only in version ("Audacity 3.0.2" and "Audacity 3.7.8 (64-bit)"):
/// returns the ids of the older ones.
pub fn older_twins(ps: &[Program]) -> Vec<String> {
    let base = |n: &str| n.to_ascii_lowercase().split(|c: char| c.is_ascii_digit() || c == '(').next().unwrap_or("").trim().trim_end_matches(['-', 'v', ' ']).to_string();
    let ver = |p: &Program| p.version.split(['.', '-']).map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut groups: HashMap<String, Vec<&Program>> = HashMap::new();
    for p in ps.iter().filter(|p| !p.id.starts_with("Steam App ") && !has(&p.name, NEEDED) && !has(&p.name, DEV) && !p.version.is_empty()) { let b = base(&p.name); if b.len() >= 4 { groups.entry(b).or_default().push(p); } }
    for (_, mut g) in groups { if g.len() < 2 { continue; } g.sort_by(|a, b| ver(a).cmp(&ver(b))); for p in &g[..g.len() - 1] { if ver(p) < ver(g[g.len() - 1]) { out.push(p.id.clone()); } } }
    out
}

/// Pure: `reg query <key> /s` text -> one map of value name -> data per subkey.
pub fn parse_reg(text: &str) -> Vec<(String, HashMap<String, String>)> {
    let mut out: Vec<(String, HashMap<String, String>)> = Vec::new();
    for line in text.lines() {
        if line.starts_with("HKEY_") { out.push((line.trim().to_string(), HashMap::new())); continue; }
        if let (Some(last), Some(rest)) = (out.last_mut(), line.strip_prefix("    ")) {
            let f: Vec<&str> = rest.splitn(3, "    ").collect();
            if f.len() == 3 { last.1.insert(f[0].trim().to_string(), f[2].trim().to_string()); } else if f.len() == 2 { last.1.insert(f[0].trim().to_string(), String::new()); }
        }
    }
    out
}

fn folder_of(cmd: &str) -> String {
    let c = cmd.trim().trim_start_matches('"');
    let exe = match c.to_ascii_lowercase().find(".exe") { Some(i) => &c[..i + 4], None => return String::new() };
    if exe.to_ascii_lowercase().ends_with("msiexec.exe") { return String::new(); }
    exe.rsplit_once('\\').map(|(d, _)| d.to_ascii_lowercase()).unwrap_or_default()
}

/// Pure: registry entries -> programs (updates and system components skipped).
pub fn programs_from(entries: &[(String, HashMap<String, String>)]) -> Vec<Program> {
    let mut out: Vec<Program> = Vec::new();
    for (key, v) in entries {
        let g = |k: &str| v.get(k).cloned().unwrap_or_default();
        let name = g("DisplayName");
        if name.is_empty() || v.contains_key("ParentKeyName") || g("SystemComponent") == "0x1" || g("ReleaseType").contains("Update") { continue; }
        if out.iter().any(|p| p.name == name) { continue; }
        let mut loc = g("InstallLocation").trim_matches('"').trim_end_matches('\\').to_ascii_lowercase();
        // A Steam game's icon and uninstaller are Steam's own, not the game's folder.
        let steam = key.rsplit('\\').next().unwrap_or("").starts_with("Steam App ");
        if loc.len() < 4 && !steam { loc = folder_of(&g("DisplayIcon")); }
        if loc.len() < 4 && !steam { loc = folder_of(&g("UninstallString")); }
        let size = u64::from_str_radix(g("EstimatedSize").trim_start_matches("0x"), 16).unwrap_or(0) as f64 / 1024.0;
        out.push(Program { id: key.rsplit('\\').next().unwrap_or("").to_string(), name, publisher: g("Publisher"), version: g("DisplayVersion"), installed: g("InstallDate"),
            size_mb: size, location: loc.replace('/', "\\"), uninstall: g("UninstallString"), quiet_uninstall: g("QuietUninstallString"), ..Default::default() });
    }
    out
}

fn rot13(s: &str) -> String {
    s.chars().map(|c| match c { 'a'..='m' | 'A'..='M' => (c as u8 + 13) as char, 'n'..='z' | 'N'..='Z' => (c as u8 - 13) as char, _ => c }).collect()
}

/// Pure: UserAssist `reg query /s` text -> (lowercase program path, unix time last opened).
pub fn parse_userassist(text: &str) -> Vec<(String, f64)> {
    let folders = [("{6d809377-6af0-444b-8957-a3773f02200e}", "c:\\program files"), ("{7c5a40ef-a0fb-4bfc-874a-c0f2e0b9fa8e}", "c:\\program files (x86)"),
        ("{1ac14e77-02e7-4e5d-b744-2eb1ae5198b7}", "c:\\windows\\system32"), ("{f38bf404-1d43-42f2-9305-67de0b28fc23}", "c:\\windows"),
        ("{f1b32785-6fba-4fcf-9d55-7b8e7f157091}", "%localappdata%")];
    let mut out = Vec::new();
    for (_, vals) in parse_reg(text) {
        for (k, hex) in vals {
            let mut path = rot13(&k).to_ascii_lowercase();
            for (g, f) in folders { if path.starts_with(g) { path = format!("{}{}", f, &path[g.len()..]); } }
            if hex.len() < 136 { continue; }
            let b = |i: usize| u64::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
            let ft = (0..8).fold(0u64, |a, i| a | (b(60 + i) << (8 * i)));
            if ft == 0 { continue; }
            out.push((path, ft as f64 / 1e7 - 11_644_473_600.0));
        }
    }
    out
}

/// Pure: Steam's localconfig.vdf -> appid -> unix time last played.
pub fn parse_steam_played(vdf: &str) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let mut app = String::new();
    for l in vdf.lines() {
        let t = l.trim();
        let q: Vec<&str> = t.split('"').filter(|s| !s.trim().is_empty()).collect();
        if q.len() == 1 && q[0].chars().all(|c| c.is_ascii_digit()) { app = q[0].to_string(); }
        if q.len() == 2 && q[0] == "LastPlayed" { if let Ok(v) = q[1].parse::<f64>() { if v > 0.0 { out.insert(app.clone(), v); } } }
    }
    out
}

fn read_tsv(dir: &Path, f: &str) -> Vec<Vec<String>> {
    std::fs::read_to_string(dir.join(f)).unwrap_or_default().lines().map(|l| l.split('\t').map(str::to_string).collect()).collect()
}

/// Remember which programs are running now (path -> last unix). Called every 10 minutes.
pub fn note_seen(dir: &Path, paths: &[String], now: f64) {
    let mut m: HashMap<String, f64> = read_tsv(dir, SEEN).into_iter().filter(|r| r.len() >= 2).map(|r| (r[0].clone(), r[1].parse().unwrap_or(0.0))).collect();
    for p in paths { m.insert(p.to_ascii_lowercase(), now); }
    let first = m.get("#since").copied().unwrap_or(now);
    m.insert("#since".into(), first);
    let mut s: Vec<_> = m.into_iter().collect();
    s.sort_by(|a, b| a.0.cmp(&b.0));
    let _ = std::fs::write(dir.join(SEEN), s.iter().map(|(k, v)| format!("{}\t{:.0}\n", k, v)).collect::<String>());
}

/// Record a decision for a program: keep | remove | needed (from the user or an AI session).
pub fn set_verdict(dir: &Path, name: &str, verdict: &str, why: &str, by: &str) {
    let mut rows: Vec<Vec<String>> = read_tsv(dir, VERDICTS).into_iter().filter(|r| !r.is_empty() && !r[0].eq_ignore_ascii_case(name)).collect();
    rows.push(vec![name.to_string(), verdict.to_string(), why.replace(['\t', '\n'], " "), by.to_string()]);
    let _ = std::fs::write(dir.join(VERDICTS), rows.iter().map(|r| r.join("\t") + "\n").collect::<String>());
}

/// Every installed program with its evidence filled in, and how many days snifrig has watched.
pub fn inventory(dir: &Path, now: f64) -> (Vec<Program>, f64) {
    let mut ps = programs_from(&crate::programs::win::uninstall_entries());
    let seen = read_tsv(dir, SEEN);
    let since = seen.iter().find(|r| r[0] == "#since").and_then(|r| r.get(1)?.parse::<f64>().ok()).unwrap_or(now);
    let seen: Vec<(String, f64)> = seen.into_iter().filter(|r| r.len() >= 2 && r[0] != "#since").map(|r| (r[0].clone(), r[1].parse().unwrap_or(0.0))).collect();
    let lad = std::env::var("LOCALAPPDATA").unwrap_or_default().to_ascii_lowercase();
    let ua: Vec<(String, f64)> = parse_userassist(&win::reg_query(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\UserAssist"))
        .into_iter().map(|(p, t)| (p.replace("%localappdata%", &lad), t)).collect();
    let steam = win::steam_played();
    let running = win::running_paths();
    let verdicts = read_tsv(dir, VERDICTS);
    for p in ps.iter_mut() {
        let loc = p.location.clone();
        let inside = |path: &str| loc.len() > 3 && path.starts_with(&format!("{}\\", loc));
        let best = |t: f64, how: &str, p: &mut Program| if t > p.last_used { p.last_used = t; p.used_how = how.to_string(); };
        // A path inside its folder, or a Start-menu shortcut / app id named like the program
        // ("...\Obsidian.lnk", "Chrome").
        let pname = p.name.to_ascii_lowercase();
        let named = |path: &str| { let s = path.rsplit(['\\', '/', '!']).next().unwrap_or("").trim_end_matches(".lnk").trim_end_matches(".exe"); s.len() >= 4 && !path.contains(":\\windows") && (pname.starts_with(s) || format!(" {} ", pname.replace(['(', ')'], " ")).contains(&format!(" {} ", s))) };
        let ua_t = ua.iter().filter(|(path, _)| inside(path) || named(path)).map(|x| x.1).fold(0.0, f64::max);
        let seen_t = seen.iter().filter(|(path, _)| inside(path)).map(|x| x.1).fold(0.0, f64::max);
        p.running = running.iter().any(|r| inside(r));
        best(ua_t, "opened from Start or the desktop", p);
        best(seen_t, "snifrig saw it running", p);
        if let Some(t) = p.id.strip_prefix("Steam App ").and_then(|id| steam.get(id)) { best(*t, "Steam", p); }
        if p.running { best(now, "running now", p); }
        if let Some(v) = verdicts.iter().find(|r| r[0].eq_ignore_ascii_case(&p.name)) { p.verdict = v.get(1).cloned().unwrap_or_default(); }
        if p.size_mb < 1.0 && p.location.len() > 3 { p.size_mb = win::folder_mb(&p.location); }
    }
    (ps, (now - since) / DAY)
}

/// The plain report: every program grouped by verdict, biggest first.
pub fn report(dir: &Path, now: f64) -> String {
    let (ps, days) = inventory(dir, now);
    let twins = older_twins(&ps);
    let mut rows: Vec<(Kind, &Program, String)> = ps.iter().map(|p| { let (k, why) = judge(p, now, days, twins.contains(&p.id)); (k, p, why) }).collect();
    rows.sort_by(|a, b| b.1.size_mb.partial_cmp(&a.1.size_mb).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = format!("{} programs installed. snifrig has watched what runs for {:.0} days; the longer it watches, the surer \"not sure yet\" gets.\n", ps.len(), days);
    for k in [Kind::Extra, Kind::Unclear, Kind::Game, Kind::InUse, Kind::Needed] {
        let g: Vec<_> = rows.iter().filter(|r| r.0 == k).collect();
        if g.is_empty() { continue; }
        let gb: f64 = g.iter().map(|r| r.1.size_mb).sum::<f64>() / 1024.0;
        out.push_str(&format!("\n{} ({}, {:.1} GB)\n", k.word(), g.len(), gb));
        for (_, p, why) in g {
            let by = if p.publisher.is_empty() { String::new() } else { format!(", {}", p.publisher) };
            out.push_str(&format!("  {}{} [{:.0} MB] {}{}\n", p.name, by, p.size_mb, why, if p.uninstall.is_empty() { "; no uninstaller" } else { "" }));
        }
    }
    out
}

/// One line per program for tools: name \t verdict \t reason \t MB \t publisher \t id.
pub fn tsv(dir: &Path, now: f64) -> String {
    let (ps, days) = inventory(dir, now);
    let twins = older_twins(&ps);
    ps.iter().map(|p| { let (k, why) = judge(p, now, days, twins.contains(&p.id)); format!("{}\t{}\t{}\t{:.0}\t{}\t{}\n", p.name, k.word(), why, p.size_mb, p.publisher, p.id) }).collect()
}

/// The command that removes a program, quiet when the program offers that.
pub fn uninstall_command(dir: &Path, name: &str) -> Option<String> {
    let (ps, _) = inventory(dir, 0.0);
    let p = ps.into_iter().find(|p| p.name.eq_ignore_ascii_case(name) || p.id.eq_ignore_ascii_case(name))?;
    Some(if !p.quiet_uninstall.is_empty() { p.quiet_uninstall } else { p.uninstall.replace("MsiExec.exe /I", "MsiExec.exe /X") }).filter(|s| !s.is_empty())
}

#[cfg(windows)]
pub mod win {
    use crate::os_ext::CommandExt as _;
    use std::collections::HashMap;

    pub fn reg_query(key: &str) -> String {
        std::process::Command::new("reg").args(["query", key, "/s"]).stdin(std::process::Stdio::null()).creation_flags(0x0800_0000).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    }

    pub fn uninstall_entries() -> Vec<(String, HashMap<String, String>)> {
        [r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall", r"HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall"]
            .iter().flat_map(|k| super::parse_reg(&reg_query(k))).collect()
    }

    pub fn running_paths() -> Vec<String> {
        crate::sys::snapshot(&mut Vec::new()).iter().filter_map(|p| crate::proc::image_path(p.pid)).map(|s| s.to_ascii_lowercase()).collect()
    }

    /// Steam's last-played times across every Steam user on this PC.
    pub fn steam_played() -> HashMap<String, f64> {
        let root = super::parse_reg(&reg_query(r"HKCU\Software\Valve\Steam")).into_iter().find_map(|(_, v)| v.get("SteamPath").cloned()).unwrap_or_default();
        let mut out = HashMap::new();
        if let Ok(d) = std::fs::read_dir(std::path::Path::new(&root).join("userdata")) {
            for u in d.flatten() {
                let vdf = std::fs::read_to_string(u.path().join("config").join("localconfig.vdf")).unwrap_or_default();
                for (k, v) in super::parse_steam_played(&vdf) { let e = out.entry(k).or_insert(0.0); if v > *e { *e = v; } }
            }
        }
        out
    }

    /// Size of a folder in MB (stops counting after 300,000 files).
    pub fn folder_mb(p: &str) -> f64 {
        let mut total = 0u64;
        let mut n = 0usize;
        let mut stack = vec![std::path::PathBuf::from(p)];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                n += 1;
                if n > 300_000 { return total as f64 / 1048576.0; }
                match e.file_type() { Ok(t) if t.is_dir() => stack.push(e.path()), Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0), _ => {} }
            }
        }
        total as f64 / 1048576.0
    }
}

#[cfg(not(windows))]
pub mod win {
    use std::collections::HashMap;
    pub fn reg_query(_: &str) -> String { String::new() }
    pub fn uninstall_entries() -> Vec<(String, HashMap<String, String>)> { Vec::new() }
    pub fn running_paths() -> Vec<String> { Vec::new() }
    pub fn steam_played() -> HashMap<String, f64> { HashMap::new() }
    pub fn folder_mb(_: &str) -> f64 { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: f64 = 1_795_800_000.0; // 27 Nov 2026

    fn p(name: &str) -> Program { Program { id: name.into(), name: name.into(), installed: "20240101".into(), uninstall: "x".into(), ..Default::default() } }

    #[test]
    fn reads_the_apps_list() {
        let t = "HKEY_LOCAL_MACHINE\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{A}\n    DisplayName    REG_SZ    Dell Core Services\n    EstimatedSize    REG_DWORD    0x3d400\n    UninstallString    REG_SZ    MsiExec.exe /I{A}\n\nHKEY_LOCAL_MACHINE\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\KB1\n    DisplayName    REG_SZ    Some update\n    ParentKeyName    REG_SZ    x\n\nHKEY_LOCAL_MACHINE\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Brave\n    DisplayName    REG_SZ    Brave\n    DisplayIcon    REG_SZ    \"C:\\Program Files\\BraveSoftware\\Brave-Browser\\Application\\brave.exe\",0\n";
        let ps = programs_from(&parse_reg(t));
        assert_eq!(ps.len(), 2, "the update is skipped");
        assert_eq!((ps[0].name.as_str(), ps[0].size_mb.round()), ("Dell Core Services", 245.0));
        assert_eq!(ps[1].location, r"c:\program files\bravesoftware\brave-browser\application");
    }

    #[test]
    fn verdicts() {
        let old = NOW - 400.0 * DAY;
        assert_eq!(judge(&Program { running: true, ..p("Brave") }, NOW, 0.0, false).0, Kind::InUse);
        assert_eq!(judge(&p("Microsoft Visual C++ 2015-2022 Redistributable (x64)"), NOW, 0.0, false).0, Kind::Needed);
        assert_eq!(judge(&p("AURA Service"), NOW, 0.0, false).0, Kind::Extra, "vendor helper, never used");
        assert_eq!(judge(&Program { last_used: NOW - 3.0 * DAY, used_how: "x".into(), ..p("Logitech G HUB") }, NOW, 0.0, false).0, Kind::InUse, "used this week wins");
        assert_eq!(judge(&Program { last_used: old, used_how: "x".into(), ..p("Canary Mail") }, NOW, 0.0, false).0, Kind::Extra, "not used in over a year");
        assert_eq!(judge(&p("Canary Mail"), NOW, 10.0, false).0, Kind::Unclear, "no record and only 10 days of watching");
        assert_eq!(judge(&p("Canary Mail"), NOW, 90.0, false).0, Kind::Extra, "90 days of watching and never seen");
        assert_eq!(judge(&Program { verdict: "keep".into(), ..p("AURA Service") }, NOW, 0.0, false).0, Kind::InUse, "the user's word wins");
        assert_eq!(judge(&p("Steam App 1234"), NOW, 0.0, false).0, Kind::Game);
        // Older Python or PostgreSQL next to a newer one: projects may need it, so never "extra" on its own.
        assert_eq!(judge(&p("Python 3.11.9 (64-bit)"), NOW, 0.0, true).0, Kind::Unclear);
        assert_eq!(judge(&p("PostgreSQL 16"), NOW, 0.0, true).0, Kind::Unclear);
        assert_eq!(judge(&p("EPSON ET-3760 Series Printer Uninstall"), NOW, 0.0, false).0, Kind::Unclear);
        // Word boundaries: "Logitech" is not "git", "Trust" is not "rust".
        assert_eq!(judge(&p("Logitech G HUB"), NOW, 0.0, false).1, "a hardware maker's helper app; no record of it being used");
        assert_eq!(judge(&p("Quicken WillMaker & Trust 2022"), NOW, 0.0, false).0, Kind::Unclear);
        assert!(!judge(&p("Quicken WillMaker & Trust 2022"), NOW, 0.0, false).1.contains("developer"));
        assert_eq!(judge(&p("Windows Driver Kit - Windows 10.0.26100.6584"), NOW, 0.0, false).0, Kind::Unclear, "a developer kit, not a driver");
        assert_eq!(judge(&Program { running: true, ..p("WebAdvisor by McAfee") }, NOW, 0.0, false).0, Kind::Extra, "running bundled software is still extra");
    }

    #[test]
    fn older_versions_of_the_same_program() {
        let a = Program { version: "3.0.2".into(), ..p("Audacity 3.0.2") };
        let b = Program { version: "3.7.8".into(), id: "b".into(), ..p("Audacity 3.7.8 (64-bit)") };
        assert_eq!(older_twins(&[a, b]), vec!["Audacity 3.0.2".to_string()]);
    }

    #[test]
    fn userassist_and_steam() {
        // ROT13 of "{6D809377-6AF0-444B-8957-A3773F02200E}\Dell\x.exe"; FILETIME at byte 60.
        let name = rot13(r"{6D809377-6AF0-444B-8957-A3773F02200E}\Dell\x.exe");
        let ft: u64 = ((NOW + 11_644_473_600.0) * 1e7) as u64;
        let mut hex = "00".repeat(60);
        for i in 0..8 { hex.push_str(&format!("{:02X}", (ft >> (8 * i)) & 0xff)); }
        hex.push_str(&"00".repeat(4));
        let t = format!("HKEY_CURRENT_USER\\...\\Count\n    {}    REG_BINARY    {}\n", name, hex);
        let u = parse_userassist(&t);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].0, r"c:\program files\dell\x.exe");
        assert!((u[0].1 - NOW).abs() < 1.0);
        let vdf = "\"apps\"\n{\n\"1234\"\n{\n\"LastPlayed\"\t\t\"1795000000\"\n}\n}\n";
        assert_eq!(parse_steam_played(vdf).get("1234"), Some(&1_795_000_000.0));
    }

    #[test]
    fn remembers_what_ran_and_decisions() {
        let d = std::env::temp_dir().join(format!("snifrig-programs-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        note_seen(&d, &[r"C:\Tools\A.exe".into()], 100.0);
        note_seen(&d, &[r"C:\Tools\B.exe".into()], 200.0);
        let s = std::fs::read_to_string(d.join(SEEN)).unwrap();
        assert!(s.contains("#since\t100") && s.contains("c:\\tools\\a.exe\t100") && s.contains("c:\\tools\\b.exe\t200"), "{}", s);
        set_verdict(&d, "AURA Service", "remove", "never used", "user");
        set_verdict(&d, "aura service", "keep", "changed mind", "user");
        assert_eq!(read_tsv(&d, VERDICTS).len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}

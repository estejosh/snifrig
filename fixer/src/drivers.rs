//! Drivers: what needs attention, and the updates the manufacturers publish for this exact
//! hardware. Shared with the free monitor (`snifrig drivers` is the scan); installing is snifrig fix.
//!
//! Where updates come from: Windows Update's driver catalog. Manufacturers (NVIDIA, Realtek,
//! Intel, AMD, board makers) publish their drivers there, each signed and tested by Microsoft
//! (WHQL) and matched to the exact hardware id, so nothing generic or repackaged is offered.
//! The rules (see `decide`):
//! * newer than what you have, or it is not offered;
//! * a part your PC or board maker customized (audio, touchpad, chipset, storage) keeps the
//!   maker's version unless the update comes from that maker;
//! * graphics follow your choice: newest, or stay on this version;
//! * installed one at a time while you are away, after a restore point; a device that shows a
//!   problem afterwards gets its previous driver back at once.
use std::path::Path;

pub const SCAN_PS1: &str = include_str!("drivers-scan.ps1");
pub const REPORT: &str = "drivers-scan.txt";
pub const GPU_CHOICE: &str = "drivers-gpu.txt";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Update {
    pub id: String, pub title: String, pub maker: String, pub class: String, pub date: String,
    pub bytes: u64, pub installed_version: String, pub installed_provider: String, pub installed_date: String, pub hwid: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Other { pub kind: String, pub name: String, pub state: String, pub last: String }

#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub unix: u64, pub gpu: String, pub board: String, pub pc: String, pub wu_error: String,
    /// (device, Windows problem code, class)
    pub problems: Vec<(String, u32, String)>,
    pub disabled: Vec<String>,
    pub updates: Vec<Update>,
    pub others: Vec<Other>,
    pub drivers: usize,
}

pub fn parse(text: &str) -> Scan {
    let mut s = Scan::default();
    for line in text.lines() {
        let f: Vec<&str> = line.trim_start_matches('\u{feff}').split('\t').collect();
        let g = |i: usize| f.get(i).map(|x| x.trim().to_string()).unwrap_or_default();
        match f[0] {
            "unix" => s.unix = g(1).parse().unwrap_or(0),
            "gpu" => s.gpu = g(1),
            "board" => s.board = g(1),
            "pc" => s.pc = g(1),
            "wu_error" => s.wu_error = g(1),
            "problem" => s.problems.push((g(1), g(2).parse().unwrap_or(0), g(3))),
            "disabled" => s.disabled.push(g(1)),
            "update" => s.updates.push(Update { id: g(1), title: g(2), maker: g(3), class: g(4), date: g(5), bytes: g(6).parse().unwrap_or(0),
                installed_version: g(7), installed_provider: g(8), installed_date: g(9), hwid: g(10) }),
            "other" => s.others.push(Other { kind: g(1), name: g(2), state: g(3), last: g(4) }),
            "driver" => s.drivers += 1,
            _ => {}
        }
    }
    s
}

/// Your choice for the graphics driver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gpu { Newest, Stay }

pub fn gpu_choice(dir: &Path) -> Gpu {
    match std::fs::read_to_string(dir.join(GPU_CHOICE)).unwrap_or_default().trim() { "stay" => Gpu::Stay, _ => Gpu::Newest }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision { Install, Hold(String) }

/// The short name a maker goes by ("ASUSTeK COMPUTER INC." is asus). None for a self-built PC.
pub fn maker_key(s: &str) -> Option<String> {
    let w = s.split_whitespace().next()?.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    let k = match w.as_str() {
        "" | "system" | "to" | "default" | "oem" => return None,
        "asustek" => "asus", "hewlett-packard" | "hp" => "hp", "micro-star" | "msi" => "msi",
        "gigabyte" => "gigabyte", "lenovo" => "lenovo", "dell" => "dell", "acer" => "acer", x => x,
    };
    Some(k.to_string())
}

const CUSTOMIZED: [&str; 6] = ["media", "hidclass", "mouse", "system", "scsiadapter", "hdc"];

pub fn decide(u: &Update, s: &Scan, gpu: Gpu) -> Decision {
    if !u.installed_date.is_empty() && !u.date.is_empty() && u.date <= u.installed_date {
        return Decision::Hold(format!("not newer than the one you have ({})", u.installed_date));
    }
    let class = u.class.to_lowercase();
    if class == "display" && gpu == Gpu::Stay {
        return Decision::Hold("you chose to stay on your current graphics driver".into());
    }
    if CUSTOMIZED.contains(&class.as_str()) {
        let have = u.installed_provider.to_lowercase();
        let from = u.maker.to_lowercase();
        for m in [maker_key(&s.board), maker_key(&s.pc)].into_iter().flatten() {
            if have.contains(&m) && !from.contains(&m) {
                return Decision::Hold(format!("your {} version is customized for this PC, so updates for it come from {}", m.to_uppercase(), m.to_uppercase()));
            }
        }
    }
    Decision::Install
}

/// What a Windows device problem code means, in plain words.
pub fn problem_text(code: u32) -> &'static str {
    match code {
        1 | 3 | 31 | 39 | 48 | 52 => "its driver is broken or blocked",
        10 => "it could not start",
        12 => "it is out of resources",
        14 | 21 => "it needs a restart",
        18 | 19 => "its driver needs reinstalling",
        24 | 45 => "it is not plugged in or not present",
        28 => "it has no driver",
        43 => "it reported a fault and Windows stopped it",
        _ => "it reports a problem",
    }
}

/// Run the scan (read-only, no administrator rights, low priority) and read what it found.
pub fn scan(dir: &Path) -> Result<Scan, String> {
    use std::os::windows::process::CommandExt;
    let script = dir.join("drivers-scan.ps1");
    std::fs::write(&script, SCAN_PS1).map_err(|e| e.to_string())?;
    let out = dir.join(REPORT);
    let st = std::process::Command::new("powershell.exe").args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&script).arg("-Out").arg(&out)
        .creation_flags(0x0800_0000 | 0x0000_4000).status().map_err(|e| e.to_string())?; // no window, below normal
    if !st.success() { return Err("the driver scan did not finish".into()); }
    Ok(parse(&std::fs::read_to_string(&out).map_err(|e| e.to_string())?))
}

/// Updates you said no to (or that snifrig took back): not offered again. Newer ones have new ids.
pub const HELD: &str = "drivers-held.txt";
pub fn held_by_you(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join(HELD)).unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}
pub fn hold(dir: &Path, ids: &[String]) {
    let mut h = held_by_you(dir);
    for i in ids { if !h.contains(i) { h.push(i.clone()); } }
    let _ = std::fs::write(dir.join(HELD), h.join("\n") + "\n");
}

pub fn load(dir: &Path) -> Option<Scan> { std::fs::read_to_string(dir.join(REPORT)).ok().map(|t| parse(&t)) }

/// The updates that pass the rules, and the ones held back with the reason.
pub fn sort(s: &Scan, gpu: Gpu) -> (Vec<&Update>, Vec<(&Update, String)>) {
    let (mut ok, mut held) = (vec![], vec![]);
    for u in &s.updates { match decide(u, s, gpu) { Decision::Install => ok.push(u), Decision::Hold(w) => held.push((u, w)) } }
    (ok, held)
}

/// The scan in plain words, for `snifrig drivers` and the app.
pub fn summary(s: &Scan, gpu: Gpu) -> Vec<String> {
    let mut l = vec![];
    if s.problems.is_empty() { l.push(format!("All {} devices with their own drivers are working.", s.drivers)); }
    for (n, c, _) in &s.problems { l.push(format!("Problem: {}: {} (Windows code {}).", n, problem_text(*c), c)); }
    for n in &s.disabled { l.push(format!("Turned off by you or Windows: {}.", n)); }
    if !s.gpu.is_empty() { l.push(format!("Graphics: {}{}.", s.gpu, if gpu == Gpu::Stay { " (staying on this version)" } else { "" })); }
    if !s.wu_error.is_empty() { l.push(format!("Windows Update could not be asked for driver updates: {}", s.wu_error)); }
    let (ok, held) = sort(s, gpu);
    if ok.is_empty() && held.is_empty() && s.wu_error.is_empty() { l.push("No driver updates from the manufacturers right now.".into()); }
    for u in &ok {
        l.push(format!("Update ready: {} from {}, {} (you have {}{}).", u.title, u.maker, u.date,
            if u.installed_version.is_empty() { "Windows' own driver".to_string() } else { u.installed_version.clone() },
            if u.installed_date.is_empty() { String::new() } else { format!(", {}", u.installed_date) }));
    }
    for (u, why) in &held { l.push(format!("Held back: {}: {}.", u.title, why)); }
    for o in &s.others {
        l.push(match o.kind.as_str() {
            "task" => format!("Other driver updater: scheduled task '{}' ({}{}).", o.name, o.state.to_lowercase(), if o.last.is_empty() { String::new() } else { format!(", last ran {}", o.last) }),
            "service" => format!("Other driver updater: service '{}' ({}).", o.name, o.state),
            _ => format!("Other driver updater running now: {} ({}).", o.name, o.state),
        });
    }
    if !s.others.is_empty() { l.push("snifrig covers what these do, so you can uninstall them if you like (Settings > Apps).".into()); }
    l
}

/// After driver updates: did the PC get slower? snifrig compares how often it felt slow in the
/// day before the updates with the day after, and offers to take them back if it got worse.
pub const WATCH: &str = "drivers-watch.txt";

#[derive(Default)]
pub struct Feel { samples: std::collections::VecDeque<(f64, bool)> }

impl Feel {
    pub fn add(&mut self, t: f64, slow: bool) {
        self.samples.push_back((t, slow));
        while self.samples.front().map_or(false, |s| t - s.0 > 2.0 * 86400.0) { self.samples.pop_front(); }
    }
    /// (samples, share that felt slow) between two times.
    pub fn rate(&self, from: f64, to: f64) -> (usize, f64) {
        let v: Vec<bool> = self.samples.iter().filter(|s| s.0 >= from && s.0 < to).map(|s| s.1).collect();
        (v.len(), if v.is_empty() { 0.0 } else { v.iter().filter(|x| **x).count() as f64 / v.len() as f64 })
    }
}

/// Enough measurements on both sides, and clearly worse after.
pub fn worse(before: (usize, f64), after: (usize, f64)) -> bool {
    before.0 >= 1000 && after.0 >= 2000 && after.1 > before.1 * 1.5 + 0.05
}

/// The watch file: installed at, update ids, titles, and once known the "before" share.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Watch { pub at: f64, pub ids: Vec<String>, pub titles: String, pub before: Option<(usize, f64)> }

pub fn read_watch(dir: &Path) -> Option<Watch> {
    let t = std::fs::read_to_string(dir.join(WATCH)).ok()?;
    let f: Vec<&str> = t.trim_end().split('\t').collect();
    Some(Watch { at: f.first()?.parse().ok()?, ids: f.get(1)?.split(',').filter(|s| !s.is_empty()).map(String::from).collect(), titles: f.get(2).unwrap_or(&"").to_string(),
        before: match (f.get(3).and_then(|x| x.parse().ok()), f.get(4).and_then(|x| x.parse().ok())) { (Some(n), Some(r)) => Some((n, r)), _ => None } })
}

pub fn write_watch(dir: &Path, w: &Watch) {
    let b = w.before.map(|(n, r)| format!("\t{}\t{:.4}", n, r)).unwrap_or_default();
    let _ = std::fs::write(dir.join(WATCH), format!("{:.0}\t{}\t{}{}\n", w.at, w.ids.join(","), w.titles.replace('\t', " "), b));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch() {
        let mut f = Feel::default();
        for i in 0..3000 { f.add(i as f64, i % 20 == 0); }
        for i in 3000..6000 { f.add(i as f64, i % 4 == 0); }
        let (b, a) = (f.rate(0.0, 3000.0), f.rate(3000.0, 6000.0));
        assert!(worse(b, a));
        assert!(!worse(b, b));
        let w = Watch { at: 5.0, ids: vec!["a".into(), "b".into()], titles: "x".into(), before: Some((1200, 0.05)) };
        let d = std::env::temp_dir().join(format!("snifrig-drv-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        write_watch(&d, &w);
        assert_eq!(read_watch(&d), Some(w));
        let _ = std::fs::remove_dir_all(&d);
    }
    fn up(class: &str, maker: &str, date: &str, have: &str, have_date: &str) -> Update {
        Update { id: "x".into(), title: "t".into(), maker: maker.into(), class: class.into(), date: date.into(), installed_provider: have.into(), installed_date: have_date.into(), ..Default::default() }
    }
    fn scan_asus() -> Scan { Scan { board: "ASUSTeK COMPUTER INC. ROG STRIX X570-E GAMING".into(), pc: "System manufacturer System Product Name".into(), ..Default::default() } }

    #[test]
    fn rules() {
        let s = scan_asus();
        assert_eq!(decide(&up("Net", "Intel", "2026-10-01", "Intel", "2026-01-01"), &s, Gpu::Newest), Decision::Install);
        assert!(matches!(decide(&up("Net", "Intel", "2025-10-01", "Intel", "2026-01-01"), &s, Gpu::Newest), Decision::Hold(_)));
        // ASUS-customized audio keeps ASUS's version unless ASUS ships the update.
        assert!(matches!(decide(&up("MEDIA", "Realtek", "2026-10-01", "ASUSTeK Computer Inc.", "2025-01-01"), &s, Gpu::Newest), Decision::Hold(_)));
        assert_eq!(decide(&up("MEDIA", "ASUSTeK Computer Inc.", "2026-10-01", "ASUSTeK Computer Inc.", "2025-01-01"), &s, Gpu::Newest), Decision::Install);
        assert!(matches!(decide(&up("Display", "NVIDIA", "2026-10-01", "NVIDIA", "2025-01-01"), &s, Gpu::Stay), Decision::Hold(_)));
        assert_eq!(decide(&up("Display", "NVIDIA", "2026-10-01", "NVIDIA", "2025-01-01"), &s, Gpu::Newest), Decision::Install);
        assert_eq!(maker_key("System manufacturer"), None);
    }

    #[test]
    fn parses() {
        let t = "unix\t5\ngpu\tRTX, 1\nupdate\tid1\tTitle\tMS\tOtherHardware\t2026-10-16\t100\t1.0\tMS\t2026-06-15\thw\nother\ttask\tDriver Easy Scheduled Scan\tReady\t2026-10-26 13:08\t\ndriver\ta\ndriver\tb\n";
        let s = parse(t);
        assert_eq!((s.unix, s.updates.len(), s.others.len(), s.drivers), (5, 1, 1, 2));
        assert_eq!(s.updates[0].installed_date, "2026-06-15");
        assert!(summary(&s, Gpu::Newest).iter().any(|l| l.starts_with("Update ready")));
    }
}

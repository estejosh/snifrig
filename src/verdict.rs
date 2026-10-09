// Plain-language verdict: one sentence a non-technical user understands, plus up to 3 biggest causes,
// each with a suggested action. Pure functions, no Win32, so it is unit-testable.

pub struct ProcMem { pub name: String, pub pid: u32, pub mb: f64, pub growth_mb_h: Option<f64> }

pub struct VerdictInput {
    pub commit_pct: f64,
    pub avail_mb: f64,
    pub total_mb: f64,
    pub page_reads_per_s: Option<f64>,
    pub procs: Vec<ProcMem>,
    pub vram_mb: Option<(f64, f64)>, // used, total
    pub evidence: Vec<String>,       // slow:* kinds seen in the last 15 min
    pub dupes: Vec<String>,          // duplicate-process findings, already phrased with an action
    pub cpu_pct: Option<f64>,        // total CPU %, set only after 3 consecutive busy cycles (>= 60)
    pub cpu_hogs: Vec<CpuHog>,       // sustained CPU-hog findings
}

/// A process (or same-name group) that has averaged a lot of CPU for 10+ minutes.
pub struct CpuHog { pub name: String, pub pid: u32, pub pct: f64, pub minutes: f64 }

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict { pub severity: u8, pub headline: String, pub causes: Vec<String> }

fn friendly(name: &str) -> String {
    match name.to_ascii_lowercase().trim_end_matches(".exe") {
        "vmmemwsl" | "vmmem" => "WSL (vmmemWSL)".into(),
        "msmpeng" => "Windows Defender".into(),
        "dwm" => "Windows desktop (dwm)".into(),
        _ => name.to_string(),
    }
}

fn action(name: &str, pid: u32) -> String {
    match name.to_ascii_lowercase().trim_end_matches(".exe") {
        "vmmemwsl" | "vmmem" => "Run wsl --shutdown to give the memory back.".into(),
        "msmpeng" => "Let the scan finish, or schedule scans for later.".into(),
        "dwm" => "Sign out and back in to reset it.".into(),
        _ => format!("Close or restart it (pid {}).", pid),
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn cpu_causes(i: &VerdictInput) -> Vec<String> {
    let mut h: Vec<&CpuHog> = i.cpu_hogs.iter().collect();
    h.sort_by(|a, b| b.pct.partial_cmp(&a.pct).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<String> = h.iter().take(3).map(|x| {
        let act = if crate::cpuhog::auto_restarts(&x.name) { "End it; Windows restarts it." } else { "Close or restart it." };
        format!("{} is using about {:.0}% of a CPU core ({:.0} min, pid {}). {}", cap(&crate::cpuhog::friendly(&x.name)), x.pct, x.minutes, x.pid, act)
    }).collect();
    if out.is_empty() { out.push("Open Task Manager, sort by CPU, and close what you are not using.".into()); }
    out
}

fn size(mb: f64) -> String {
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{:.0} MB", mb) }
}

fn mem_causes(i: &VerdictInput) -> Vec<String> {
    let mut out: Vec<String> = i.dupes.iter().take(3).cloned().collect();
    let mut p: Vec<&ProcMem> = i.procs.iter().filter(|p| p.mb >= 1024.0 || p.growth_mb_h.map_or(false, |g| g > 200.0)).collect();
    p.sort_by(|a, b| b.mb.partial_cmp(&a.mb).unwrap_or(std::cmp::Ordering::Equal));
    for x in p {
        if out.len() >= 3 { break; }
        let grow = match x.growth_mb_h { Some(g) if g > 200.0 => format!(", growing {}/h", size(g)), _ => String::new() };
        out.push(format!("{} {}{}. {}", friendly(&x.name), size(x.mb), grow, action(&x.name, x.pid)));
    }
    out
}

pub fn verdict(i: &VerdictInput) -> Option<Verdict> {
    let has = |k: &str| i.evidence.iter().any(|e| e == k || e == &format!("slow:{}", k));
    let v = |s: u8, h: &str, c: Vec<String>| Some(Verdict { severity: s, headline: h.to_string(), causes: c });
    if i.commit_pct >= 90.0 || (i.avail_mb < 1024.0 && i.total_mb > 0.0) {
        return v(2, "Your PC is short on memory, so Windows is swapping to disk. That is why it feels slow.", mem_causes(i));
    }
    if has("paging") || i.page_reads_per_s.map_or(false, |r| r > 500.0) {
        return v(1, "Windows is reading memory back from disk, which makes everything lag.", mem_causes(i));
    }
    if i.cpu_pct.map_or(false, |c| c >= 60.0) || !i.cpu_hogs.is_empty() {
        let top = i.cpu_hogs.iter().max_by(|a, b| a.pct.partial_cmp(&b.pct).unwrap_or(std::cmp::Ordering::Equal));
        let h = match top { Some(t) => format!("Your CPU is busy, mostly with {}.", crate::cpuhog::friendly(&t.name)), None => "Your CPU is busy.".to_string() };
        return v(1, &h, cpu_causes(i));
    }
    if has("cpu-throttle") || has("thermal") {
        return v(1, "Your CPU is being slowed down (power or heat).", vec!["Check fans, dust and the power plan (use Balanced or High performance).".into()]);
    }
    if has("os-maintenance") {
        return v(1, "Windows background work (Defender, indexing or updates) is using a lot of CPU.", vec!["Let it finish, usually under an hour. Nothing to close.".into()]);
    }
    if has("disk-saturated") {
        return v(1, "Your disk is maxed out, so everything waits on it.", vec!["Pause big copies, downloads or scans, and close apps that are reading lots of files.".into()]);
    }
    if has("driver-latency") {
        return v(1, "A hardware driver is hogging the CPU, which causes stutter.", vec!["Update or disable recently changed drivers (network, audio, GPU).".into()]);
    }
    if let Some((u, t)) = i.vram_mb {
        if t > 0.0 && u / t >= 0.95 {
            return v(1, "Your graphics memory is full, so GPU apps spill into system RAM.", vec![format!("Graphics memory {} of {} used. Close GPU apps you are not using (AI models, games, browsers).", size(u), size(t))]);
        }
    }
    None
}

/// Reads the "causes" array back out of status.json text (strings written with the monitor's esc helper).
pub fn causes_from_status(s: &str) -> Vec<String> {
    let Some(i) = s.find("\"causes\":[") else { return Vec::new() };
    let r = &s[i + 10..];
    let (mut out, mut cur, mut inq, mut esc) = (Vec::new(), String::new(), false, false);
    for c in r.chars() {
        if inq {
            if esc { cur.push(c); esc = false; }
            else if c == '\\' { esc = true; }
            else if c == '"' { inq = false; out.push(std::mem::take(&mut cur)); }
            else { cur.push(c); }
        } else if c == '"' { inq = true; }
        else if c == ']' { break; }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> VerdictInput {
        VerdictInput { commit_pct: 40.0, avail_mb: 20000.0, total_mb: 65536.0, page_reads_per_s: None, procs: vec![], vram_mb: None, evidence: vec![], dupes: vec![], cpu_pct: None, cpu_hogs: vec![] }
    }
    fn p(n: &str, pid: u32, mb: f64, g: Option<f64>) -> ProcMem { ProcMem { name: n.into(), pid, mb, growth_mb_h: g } }

    #[test]
    fn quiet_is_none() { assert!(verdict(&base()).is_none()); }

    #[test]
    fn commit_90_is_memory() {
        let mut i = base();
        i.commit_pct = 90.5;
        i.procs = vec![p("comet.exe", 7, 9500.0, None), p("vmmemWSL", 8, 25088.0, Some(1536.0)), p("a.exe", 1, 3000.0, None), p("b.exe", 2, 2000.0, None)];
        let v = verdict(&i).unwrap();
        assert_eq!(v.severity, 2);
        assert!(v.headline.starts_with("Your PC is short on memory"));
        assert_eq!(v.causes.len(), 3);
        assert_eq!(v.causes[0], "WSL (vmmemWSL) 24.5 GB, growing 1.5 GB/h. Run wsl --shutdown to give the memory back.");
        assert!(v.causes[1].starts_with("comet.exe 9.3 GB. Close or restart it (pid 7)"));
    }

    #[test]
    fn low_avail_is_memory() {
        let mut i = base();
        i.avail_mb = 500.0;
        assert_eq!(verdict(&i).unwrap().severity, 2);
    }

    #[test]
    fn duplicates_come_first() {
        let mut i = base();
        i.commit_pct = 95.0;
        i.dupes = vec!["llama-server.exe is running twice (10.0 GB idle copy). End the idle duplicate (pid 99).".into()];
        i.procs = vec![p("MsMpEng.exe", 4, 1500.0, None)];
        let v = verdict(&i).unwrap();
        assert!(v.causes[0].contains("pid 99"));
        assert!(v.causes[1].starts_with("Windows Defender 1.5 GB"));
    }

    #[test]
    fn paging_by_reads_or_evidence() {
        let mut i = base();
        i.page_reads_per_s = Some(1500.0);
        let v = verdict(&i).unwrap();
        assert_eq!(v.severity, 1);
        assert!(v.headline.starts_with("Windows is reading memory back from disk"));
        let mut j = base();
        j.evidence = vec!["slow:paging".into()];
        assert_eq!(verdict(&j).unwrap().headline, v.headline);
        j.page_reads_per_s = Some(100.0);
        assert!(verdict(&j).is_some());
        let mut k = base();
        k.page_reads_per_s = Some(100.0);
        assert!(verdict(&k).is_none());
    }

    #[test]
    fn cpu_thermal_maintenance_disk_driver() {
        for (k, start) in [("cpu-throttle", "Your CPU is being slowed"), ("thermal", "Your CPU is being slowed"), ("os-maintenance", "Windows background work"),
                           ("disk-saturated", "Your disk is maxed out"), ("driver-latency", "A hardware driver")] {
            let mut i = base();
            i.evidence = vec![k.into()];
            let v = verdict(&i).unwrap();
            assert_eq!(v.severity, 1, "{}", k);
            assert!(v.headline.starts_with(start), "{}", k);
            assert_eq!(v.causes.len(), 1);
        }
    }

    fn hog(n: &str, pid: u32, pct: f64) -> CpuHog { CpuHog { name: n.into(), pid, pct, minutes: 10.0 } }

    #[test]
    fn cpu_hog_names_top_process_and_lists_actions() {
        let mut i = base();
        i.cpu_hogs = vec![hog("comet.exe", 7, 90.0), hog("StartMenuExperienceHost.exe", 5, 98.0)];
        let v = verdict(&i).unwrap();
        assert_eq!(v.severity, 1);
        assert_eq!(v.headline, "Your CPU is busy, mostly with the Windows Start menu.");
        assert_eq!(v.causes.len(), 2);
        assert_eq!(v.causes[0], "The Windows Start menu is using about 98% of a CPU core (10 min, pid 5). End it; Windows restarts it.");
        assert_eq!(v.causes[1], "Comet browser is using about 90% of a CPU core (10 min, pid 7). Close or restart it.");
    }

    #[test]
    fn sustained_total_cpu_is_busy_and_memory_wins() {
        let mut i = base();
        i.cpu_pct = Some(64.0);
        let v = verdict(&i).unwrap();
        assert_eq!(v.headline, "Your CPU is busy.");
        assert_eq!(v.causes.len(), 1);
        i.cpu_pct = Some(59.0);
        assert!(verdict(&i).is_none());
        i.cpu_pct = Some(70.0);
        i.commit_pct = 95.0;
        assert!(verdict(&i).unwrap().headline.starts_with("Your PC is short on memory"));
    }

    #[test]
    fn vram_full() {
        let mut i = base();
        i.vram_mb = Some((23900.0, 24576.0));
        let v = verdict(&i).unwrap();
        assert!(v.headline.starts_with("Your graphics memory is full"));
        i.vram_mb = Some((10000.0, 24576.0));
        assert!(verdict(&i).is_none());
    }

    #[test]
    fn memory_beats_others_and_causes_capped() {
        let mut i = base();
        i.commit_pct = 91.0;
        i.evidence = vec!["thermal".into(), "paging".into()];
        i.dupes = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let v = verdict(&i).unwrap();
        assert!(v.headline.starts_with("Your PC is short"));
        assert_eq!(v.causes.len(), 3);
    }

    #[test]
    fn causes_roundtrip() {
        let s = r#"{"headline":"x","causes":["a \"q\" b","c\\d"],"paused_until":0}"#;
        assert_eq!(causes_from_status(s), vec!["a \"q\" b".to_string(), "c\\d".to_string()]);
        assert!(causes_from_status("{}").is_empty());
    }
}

//! `--demo`: a made-up PC for store screenshots and videos. It writes a sample data folder in
//! %TEMP% (refreshed every few seconds so the window looks live) and fakes the live CPU and
//! process numbers. Nothing on this PC is read or changed, and none of the user's data is shown.
use crate::sys::{Live, ProcNow};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub fn dir() -> PathBuf { std::env::temp_dir().join("snifrig-demo") }

fn now() -> f64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }

/// UTC "YYYY-MM-DDTHH:MM:SSZ" for a unix time (the monitor's history format).
fn iso(u: f64) -> String {
    let s = u as i64;
    let (days, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

fn w(d: &Path, f: &str, s: &str) { let _ = std::fs::write(d.join(f), s); }

/// Write the sample data folder and keep its live files fresh.
pub fn prepare(d: &Path) {
    let _ = std::fs::create_dir_all(d);
    let t = now();
    w(d, "fix-mode.txt", "ask\n");
    w(d, "snifrig-fix.key", "demo\n");
    w(d, "fix-accepted.json", r#"{"license":"UFL-3.7","component":"snifrig-fix","unix":0}"#);
    w(d, "fix-rules.txt", "# snifrig fix rules (demo)\nobs64.exe governor=exempt\nonedrive.exe background=yes\nsteam.exe background=yes at_launch=yes\nffmpeg*.exe background=yes max_instances=2\n");
    let ev: [(f64, &str, &str, &str, &str, &str); 12] = [
        (40.0, "gov:chrome.exe#9120", "chrome.exe", "lower-priority", "done", "governor: level 0 -> 1: CPU at 81% and a background part of chrome.exe (a tab or helper, pid 9120) was using 46% of a core; the window you are using is untouched"),
        (95.0, "fix:mem", "Discord.exe", "trim", "done", "did it without asking because you said yes 2 times and it helped 3 of 4 times: freed 1.4 GB; undo it in the app if that was wrong"),
        (160.0, "gov:OneDrive.exe#7712", "OneDrive.exe", "lower-priority", "done", "governor: level 1 -> 2: CPU at 84% and OneDrive.exe was using 31% of a core in the background"),
        (300.0, "feel:you", "", "report", "report", "you said the PC feels slow (the meter said sluggish): easing background apps now"),
        (420.0, "rule:onedrive.exe#7712", "OneDrive.exe", "lower-priority", "done", "rule: rule onedrive.exe: efficiency mode on"),
        (900.0, "gov:python.exe#5560", "python.exe", "lower-priority", "done", "governor: level 0 -> 1: CPU at 79% and Python script train.py was using 92% of a core in the background"),
        (1500.0, "perf:mode", "", "report", "done", "performance mode on for a game: switched to the High performance plan; background apps are eased sooner"),
        (2400.0, "perf:mode", "", "report", "done", "game closed: power plan back to Balanced"),
        (3600.0, "learn:rule", "", "report", "done", "searchindexer.exe was eased 21 times this week and you never undid it, so it now always runs in the background (a rule was added; remove it on the App rules page)"),
        (5400.0, "gov:Code.exe#3301", "Code.exe", "report", "done", "governor: level 1 -> 0: Code.exe is now the app you are using"),
        (7200.0, "incident", "", "report", "report", "slowdown over after 12 min: easing chrome.exe in the background ended it; it looks like Tuesday's"),
        (9000.0, "fix:in-use", "Spotify.exe", "trim", "withdrawn", "withdrew the question to end Spotify.exe: you are using it (it has a window open); snifrig reins it in instead"),
    ];
    let lines: Vec<String> = ev.iter().rev().map(|(ago, key, name, action, status, note)|
        format!(r#"{{"unix":{:.0},"key":"{}","name":"{}","action":"{}","status":"{}","note":"{}"}}"#, t - ago, key, name, action, status, note)).collect();
    w(d, "fixes.jsonl", &(lines.join("\n") + "\n"));
    w(d, "incidents.jsonl", &format!("{}\n{}\n{}\n",
        format!(r#"{{"start":{:.0},"minutes":18,"headline":"Your CPU is busy, mostly with OneDrive.exe.","fixed_by":"easing OneDrive.exe in the background"}}"#, t - 3.0 * 86400.0),
        format!(r#"{{"start":{:.0},"minutes":12,"headline":"Your CPU is busy, mostly with chrome.exe.","fixed_by":"easing chrome.exe in the background"}}"#, t - 2.0 * 86400.0),
        format!(r#"{{"start":{:.0},"minutes":6,"headline":"The desktop is stuttering (dwm).","fixed_by":"restarting the desktop"}}"#, t - 7200.0)));
    w(d, "incident-now.json", r#"{"text":"This looks like Tuesday 21:10 (Your CPU is busy, mostly with chrome.exe.), which lasted 12 min; easing chrome.exe in the background ended it then."}"#);
    w(d, "pending.json", r#"{"items":[{"id":"d3m0","name":"Discord.exe","pid":4412,"action":"trim","why":"Discord.exe grew to 2.1 GB over 6 hours in the background. Freeing its memory closes nothing.","msg":""}]}"#);
    w(d, "brain-cache.json", "{\"name\":\"chrome.exe\",\"kind\":\"interactive\",\"confidence\":0.92}\n{\"name\":\"python.exe\",\"kind\":\"batch\",\"confidence\":0.81}\n{\"name\":\"steam.exe\",\"kind\":\"launcher\",\"confidence\":0.9}\n");
    let mut hist = String::new();
    for i in 0..144 {
        let u = t - (143 - i) as f64 * 600.0;
        let x = i as f64 / 10.0;
        hist.push_str(&format!("{{\"t\":\"{}\",\"commit_mb\":{:.0},\"avail_mb\":{:.0}}}\n", iso(u), 19000.0 + 3500.0 * x.sin() + 40.0 * i as f64, 13000.0 - 2500.0 * x.sin()));
    }
    w(d, "snifrig.jsonl", &hist);
    let d = d.to_path_buf();
    std::thread::spawn(move || loop {
        let t = now();
        let cpu = 42.0 + 9.0 * (t / 7.0).sin();
        w(&d, "status.json", &format!(r#"{{"unix":{:.0},"level":"ok","headline":"","causes":[],"severity":0}}"#, t));
        w(&d, "gpu.json", &format!(r#"{{"unix":{:.0},"reasons":"","temp_c":54,"power_w":96,"limit_w":320,"sm_mhz":1410,"sm_max_mhz":2100,"used_mb":6400,"total_mb":12288}}"#, t));
        w(&d, "governor.json", &format!(concat!(r#"{{"unix":{:.0},"acting":true,"cpu":{:.0},"feel":{{"word":"snappy","ping_ms":9,"lag_ms":2,"hung":false,"desktop_pct":6,"input_ms":7,"input_events":40}},"busy":false,"game":null,"busy_pct":75,"#,
            r#""demoted":[{{"pid":9120,"name":"chrome.exe","level":1,"reason":"CPU at 81% and a background part of chrome.exe (a tab or helper) was using 46% of a core; the window you are using is untouched","since":{:.0}}},"#,
            r#"{{"pid":7712,"name":"OneDrive.exe","level":2,"reason":"CPU at 84% and OneDrive.exe was using 31% of a core in the background","since":{:.0}}},"#,
            r#"{{"pid":5560,"name":"python.exe","level":1,"reason":"Python script train.py was using 92% of a core in the background","since":{:.0}}}],"#,
            r#""suggestions":[{{"name":"searchindexer.exe","count":9}}]}}"#), t, cpu, t - 40.0, t - 160.0, t - 900.0));
        std::thread::sleep(std::time::Duration::from_secs(3));
    });
}

/// Fake live numbers: a 16-core, 32 GB PC with ordinary apps.
pub fn live() -> Arc<Mutex<Live>> {
    let live = Arc::new(Mutex::new(Live { cores: 16, ram_total_mb: 32768.0, ..Default::default() }));
    let l = live.clone();
    std::thread::spawn(move || {
        let apps: [(&str, f64, f64); 11] = [("chrome.exe", 46.0, 1850.0), ("python.exe", 92.0, 2400.0), ("OneDrive.exe", 31.0, 210.0), ("Code.exe", 18.0, 960.0), ("Discord.exe", 6.0, 2100.0),
            ("obs64.exe", 9.0, 420.0), ("Spotify.exe", 3.0, 380.0), ("dwm.exe", 5.0, 160.0), ("MsMpEng.exe", 4.0, 240.0), ("explorer.exe", 2.0, 190.0), ("steam.exe", 1.0, 310.0)];
        let mut i = 0u64;
        loop {
            if Arc::strong_count(&l) < 2 { return; }
            let t = now();
            let cpu = 42.0 + 9.0 * (t / 7.0).sin() + 3.0 * (t / 1.7).sin();
            let ram = 17800.0 + 300.0 * (t / 30.0).sin();
            {
                let mut g = l.lock().unwrap();
                if g.cpu_hist.is_empty() { for k in 0..120 { let tt = t - (120 - k) as f64; g.cpu_hist.push_back((42.0 + 9.0 * (tt / 7.0).sin() + 3.0 * (tt / 1.7).sin()) as f32); g.ram_hist.push_back(((17800.0 + 300.0 * (tt / 30.0).sin()) / 32768.0 * 100.0) as f32); } }
                g.cpu_pct = cpu;
                g.ram_used_mb = ram;
                g.cpu_hist.push_back(cpu as f32); if g.cpu_hist.len() > 120 { g.cpu_hist.pop_front(); }
                g.ram_hist.push_back((ram / 32768.0 * 100.0) as f32); if g.ram_hist.len() > 120 { g.ram_hist.pop_front(); }
                g.procs = apps.iter().enumerate().map(|(n, (name, pct, mb))| ProcNow { pid: 1000 + n as u32 * 37, name: name.to_string(), core_pct: pct * (1.0 + 0.15 * ((t + n as f64) / 2.0).sin()), ws_mb: *mb }).collect();
            }
            i += 1;
            let _ = i;
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
    });
    live
}

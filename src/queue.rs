//! The heavy-work line (SYSTEMDIRECTIVES 4.4, Ferryman's heavy.rs). Ferryman decides what runs;
//! snifrig only answers whether this machine has room and makes heavy jobs take turns.
//!
//!   snifrig can-i-start [--cores N] [--ram-gb N] [--gpu-mem-gb N] [--expected-minutes N]
//!       prints `go` (exit 0), `wait <secs> <why>` (exit 1) or `no <why>` (exit 2)
//!   snifrig run [--name X] [same flags] -- <program> <args...>
//!       waits its turn, runs the program, exits with its code
//!   snifrig queue
//!       what runs and what waits
//!
//! Turns are files in `<data dir>\queue`: `<ticket>.wait` while waiting, `<ticket>.run` while
//! running, each holding the waiting snifrig's pid. A ticket whose process is gone is dropped,
//! so a killed job never blocks the line. Oldest ticket goes first. One heavy job at a time
//! (rule 8: one build per machine); `slots=N` in `<data dir>\queue.txt` allows more.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Need { pub cores: f64, pub ram_gb: f64, pub gpu_gb: f64 }

/// What the machine looks like right now. `None` where it could not be read.
#[derive(Clone, Debug, Default)]
pub struct Machine {
    pub cores: f64,
    pub ram_total_gb: f64,
    pub ram_free_gb: f64,
    pub commit_gb: f64,
    pub gpu: Option<(f64, f64)>,       // (total, free) GB
    pub monitor_alert: Option<String>, // the monitor's headline when it is alarmed right now
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer { Go, Wait(u32, String), No(String) }

/// Free RAM kept for everything else, on top of what the job says it needs.
const RAM_RESERVE_GB: f64 = 4.0;

/// Pure: room for this job now?
pub fn decide(need: Need, m: &Machine, running: usize, slots: usize) -> Answer {
    if m.ram_total_gb > 0.0 && need.ram_gb > m.ram_total_gb - RAM_RESERVE_GB {
        return Answer::No(format!("needs {:.0} GB RAM; this machine has {:.0}", need.ram_gb, m.ram_total_gb));
    }
    if m.cores > 0.0 && need.cores > m.cores { return Answer::No(format!("needs {:.0} cores; this machine has {:.0}", need.cores, m.cores)); }
    if need.gpu_gb > 0.0 {
        match m.gpu {
            None => return Answer::No("needs a GPU; none found".into()),
            Some((t, _)) if need.gpu_gb > t => return Answer::No(format!("needs {:.0} GB of GPU memory; the card has {:.0}", need.gpu_gb, t)),
            _ => {}
        }
    }
    if running >= slots { return Answer::Wait(60, format!("{} heavy job{} running", running, if running == 1 { "" } else { "s" })); }
    if let Some(h) = &m.monitor_alert { return Answer::Wait(60, format!("the PC is struggling: {}", h)); }
    if m.ram_total_gb > 0.0 && m.ram_free_gb < RAM_RESERVE_GB + need.ram_gb {
        return Answer::Wait(30, format!("{:.1} GB RAM free, needs {:.0}", m.ram_free_gb, RAM_RESERVE_GB + need.ram_gb));
    }
    if m.ram_total_gb > 0.0 && m.commit_gb > m.ram_total_gb * 1.2 {
        return Answer::Wait(30, format!("committed memory is {:.0} GB, over 120% of {:.0} GB RAM", m.commit_gb, m.ram_total_gb));
    }
    if let (true, Some((_, free))) = (need.gpu_gb > 0.0, m.gpu) {
        if free < need.gpu_gb { return Answer::Wait(60, format!("{:.1} GB of GPU memory free, needs {:.0}", free, need.gpu_gb)); }
    }
    Answer::Go
}

fn now() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }

fn parse(args: &[String]) -> (Need, Option<String>, Vec<String>) {
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (opts, cmd) = (&args[..split], args.get(split + 1..).unwrap_or(&[]).to_vec());
    let num = |f: &str| opts.iter().position(|a| a == f).and_then(|i| opts.get(i + 1)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let name = opts.iter().position(|a| a == "--name").and_then(|i| opts.get(i + 1)).cloned();
    (Need { cores: num("--cores"), ram_gb: num("--ram-gb"), gpu_gb: num("--gpu-mem-gb") }, name, cmd)
}

fn slots(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("queue.txt")).ok()
        .and_then(|t| t.lines().find_map(|l| l.trim().strip_prefix("slots=").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(1usize).max(1)
}

/// Tickets still alive, oldest first: (file stem, running?, contents). Dead ones are removed.
fn tickets(q: &Path) -> Vec<(String, bool, String)> {
    let mut out: Vec<(String, bool, String)> = std::fs::read_dir(q).map(|d| d.flatten().filter_map(|e| {
        let p = e.path();
        let run = match p.extension().and_then(|x| x.to_str()) { Some("run") => true, Some("wait") => false, _ => return None };
        let text = std::fs::read_to_string(&p).unwrap_or_default();
        let pid: u32 = text.split('\t').next().and_then(|v| v.parse().ok()).unwrap_or(0);
        if !alive(pid) { let _ = std::fs::remove_file(&p); return None; }
        Some((p.file_stem()?.to_string_lossy().into_owned(), run, text))
    }).collect()).unwrap_or_default();
    out.sort();
    out
}

/// `snifrig can-i-start`, `snifrig run`, `snifrig queue`. None when `args` is something else.
pub fn cli(args: &[String]) -> Option<i32> {
    let cmd = args.first()?.as_str();
    if !matches!(cmd, "can-i-start" | "run" | "queue") { return None; }
    let rest = &args[1..];
    let before = &rest[..rest.iter().position(|a| a == "--").unwrap_or(rest.len())];
    if before.iter().any(|a| a == "--help" || a == "-h") {
        println!("snifrig can-i-start [--cores N] [--ram-gb N] [--gpu-mem-gb N] [--expected-minutes N]\n  go | wait <secs> <why> | no <why>  (exit 0, 1, 2)\nsnifrig run [--name X] [same flags] -- <program> <args...>\n  waits its turn, then runs the program and exits with its code\nsnifrig queue\n  what runs and what waits");
        return Some(0);
    }
    let dir = before.iter().position(|a| a == "--dir").and_then(|i| before.get(i + 1)).map(PathBuf::from).unwrap_or_else(crate::default_dir);
    let q = dir.join("queue");
    let _ = std::fs::create_dir_all(&q);
    let (need, name, program) = parse(rest);
    Some(match cmd {
        "can-i-start" => {
            let running = tickets(&q).iter().filter(|t| t.1).count();
            match decide(need, &machine(&dir, need.gpu_gb > 0.0), running, slots(&dir)) {
                Answer::Go => { println!("go"); 0 }
                Answer::Wait(s, why) => { println!("wait {} {}", s, why); 1 }
                Answer::No(why) => { println!("no {}", why); 2 }
            }
        }
        "queue" => {
            let t = tickets(&q);
            if t.is_empty() { println!("Nothing heavy is running or waiting."); }
            for (_, run, text) in t {
                let f: Vec<&str> = text.split('\t').collect();
                let mins = (now() - f.get(2).and_then(|v| v.parse::<f64>().ok()).unwrap_or(now())) / 60.0;
                println!("{} {} ({:.0} min)", if run { "running" } else { "waiting" }, f.get(1).copied().unwrap_or("?"), mins);
            }
            0
        }
        _ => run(&dir, &q, need, name, &program),
    })
}

fn run(dir: &Path, q: &Path, need: Need, name: Option<String>, program: &[String]) -> i32 {
    let Some((exe, args)) = program.split_first() else { eprintln!("usage: snifrig run [--name X] -- <program> <args...>"); return 2 };
    let me = std::process::id();
    let name = name.unwrap_or_else(|| exe.clone()).replace(['\t', '\n'], " ");
    let stem = format!("{:015.0}-{}", now() * 1000.0, me);
    let wait = q.join(format!("{}.wait", stem));
    let text = format!("{}\t{}\t{:.0}", me, name, now());
    if std::fs::write(&wait, &text).is_err() { eprintln!("snifrig: cannot write {}", wait.display()); return 2; }
    let mut said = String::new();
    loop {
        let t = tickets(q);
        let running = t.iter().filter(|t| t.1).count();
        let ahead = t.iter().take_while(|x| x.0 != stem).filter(|x| !x.1).count();
        let ans = if ahead > 0 { Answer::Wait(15, format!("{} waiting ahead", ahead)) } else { decide(need, &machine(dir, need.gpu_gb > 0.0), running, slots(dir)) };
        match ans {
            Answer::Go => break,
            Answer::No(why) => { let _ = std::fs::remove_file(&wait); eprintln!("snifrig: {} cannot run here: {}", name, why); return 2; }
            Answer::Wait(s, why) => {
                if why != said { eprintln!("snifrig: {} waits: {}", name, why); said = why; }
                std::thread::sleep(Duration::from_secs(s.min(15) as u64));
            }
        }
    }
    let runf = q.join(format!("{}.run", stem));
    let _ = std::fs::write(&runf, &text);
    let _ = std::fs::remove_file(&wait);
    eprintln!("snifrig: {} starts", name);
    let code = match std::process::Command::new(exe).args(args).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => { eprintln!("snifrig: could not start {}: {}", exe, e); 127 }
    };
    let _ = std::fs::remove_file(&runf);
    code
}

/// The monitor's headline when status.json is fresh and says the PC is in trouble.
fn monitor_alert(dir: &Path) -> Option<String> {
    let st = std::fs::read_to_string(dir.join("status.json")).ok()?;
    let fresh = now() - (crate::json_num(&st, "unix") as f64) < 120.0;
    (fresh && crate::json_num(&st, "severity") >= 2).then(|| crate::json_str(&st, "headline").unwrap_or_else(|| "alert".into()))
}

/// (total, free) GB on the first NVIDIA card, from nvidia-smi.
fn gpu() -> Option<(f64, f64)> {
    let o = std::process::Command::new("nvidia-smi").args(["--query-gpu=memory.total,memory.free", "--format=csv,noheader,nounits"])
        .stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    let line = String::from_utf8_lossy(&o.stdout).lines().next()?.to_string();
    let mut v = line.split(',').map(|x| x.trim().parse::<f64>().ok());
    Some((v.next()?? / 1024.0, v.next()?? / 1024.0))
}

fn cores() -> f64 { std::thread::available_parallelism().map(|n| n.get() as f64).unwrap_or(0.0) }

#[cfg(windows)]
fn machine(dir: &Path, want_gpu: bool) -> Machine {
    use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    let mut pi: PERFORMANCE_INFORMATION = unsafe { std::mem::zeroed() };
    pi.cb = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
    let ok = unsafe { GetPerformanceInfo(&mut pi, pi.cb) } != 0;
    let gb = |pages: usize| (pages as f64) * (pi.PageSize as f64) / 1e9;
    Machine {
        cores: cores(),
        ram_total_gb: if ok { gb(pi.PhysicalTotal) } else { 0.0 },
        ram_free_gb: if ok { gb(pi.PhysicalAvailable) } else { 0.0 },
        commit_gb: if ok { gb(pi.CommitTotal) } else { 0.0 },
        gpu: if want_gpu { gpu() } else { None },
        monitor_alert: monitor_alert(dir),
    }
}

#[cfg(not(windows))]
fn machine(dir: &Path, want_gpu: bool) -> Machine {
    let mi = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |k: &str| mi.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) * 1024.0 / 1e9;
    Machine { cores: cores(), ram_total_gb: kb("MemTotal:"), ram_free_gb: kb("MemAvailable:"), commit_gb: kb("Committed_AS:"),
        gpu: if want_gpu { gpu() } else { None }, monitor_alert: monitor_alert(dir) }
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 { return false; }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() { return false; }
    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(h, &mut code) } != 0;
    unsafe { CloseHandle(h); }
    ok && code == 259 // STILL_ACTIVE
}

#[cfg(not(windows))]
fn alive(pid: u32) -> bool { pid != 0 && Path::new(&format!("/proc/{}", pid)).exists() }

#[cfg(test)]
mod tests {
    use super::*;
    fn pc() -> Machine { Machine { cores: 16.0, ram_total_gb: 64.0, ram_free_gb: 30.0, commit_gb: 50.0, gpu: Some((24.0, 20.0)), monitor_alert: None } }

    #[test]
    fn room_means_go() { assert_eq!(decide(Need { cores: 8.0, ram_gb: 8.0, gpu_gb: 0.0 }, &pc(), 0, 1), Answer::Go); }

    #[test]
    fn one_heavy_job_at_a_time() {
        assert!(matches!(decide(Need::default(), &pc(), 1, 1), Answer::Wait(..)));
        assert_eq!(decide(Need::default(), &pc(), 1, 2), Answer::Go);
    }

    #[test]
    fn short_on_memory_or_gpu_waits() {
        let low = Machine { ram_free_gb: 0.4, ..pc() };
        assert!(matches!(decide(Need::default(), &low, 0, 1), Answer::Wait(30, _)));
        assert!(matches!(decide(Need { ram_gb: 28.0, ..Need::default() }, &pc(), 0, 1), Answer::Wait(..)), "30 free < 4 + 28");
        let over = Machine { commit_gb: 80.0, ..pc() };
        assert!(matches!(decide(Need::default(), &over, 0, 1), Answer::Wait(..)));
        assert!(matches!(decide(Need { gpu_gb: 22.0, ..Need::default() }, &pc(), 0, 1), Answer::Wait(..)));
        let alarmed = Machine { monitor_alert: Some("short on memory".into()), ..pc() };
        assert!(matches!(decide(Need::default(), &alarmed, 0, 1), Answer::Wait(60, _)));
    }

    #[test]
    fn impossible_says_no() {
        assert!(matches!(decide(Need { ram_gb: 100.0, ..Need::default() }, &pc(), 0, 1), Answer::No(_)));
        assert!(matches!(decide(Need { gpu_gb: 48.0, ..Need::default() }, &pc(), 0, 1), Answer::No(_)));
        assert!(matches!(decide(Need { gpu_gb: 4.0, ..Need::default() }, &Machine { gpu: None, ..pc() }, 1, 1), Answer::No(_)), "no beats wait");
    }

    #[test]
    fn flags_and_command() {
        let a: Vec<String> = ["--name", "ord-1", "--ram-gb", "8", "--", "cargo", "build", "--help"].iter().map(|s| s.to_string()).collect();
        let (need, name, cmd) = parse(&a);
        assert_eq!((need.ram_gb, name.as_deref(), cmd), (8.0, Some("ord-1"), vec!["cargo".to_string(), "build".into(), "--help".into()]));
    }

    #[test]
    fn dead_tickets_are_dropped_and_oldest_goes_first() {
        let q = std::env::temp_dir().join(format!("snifrig-q-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&q);
        std::fs::write(q.join("000000000000002-1.wait"), format!("{}\tlater\t2", std::process::id())).unwrap();
        std::fs::write(q.join("000000000000001-1.wait"), format!("{}\tfirst\t1", std::process::id())).unwrap();
        std::fs::write(q.join("000000000000000-9.run"), "4000000000\tgone\t0").unwrap();
        let t = tickets(&q);
        assert_eq!(t.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["000000000000001-1", "000000000000002-1"]);
        assert!(!q.join("000000000000000-9.run").exists());
        let _ = std::fs::remove_dir_all(&q);
    }
}

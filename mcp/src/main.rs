//! snifrig-mcp: MCP server (stdio, newline-delimited JSON-RPC) for snifrig.
//! Read-only tools plus `snifrig_suggest`, which only queues a suggestion for the user.
mod json;
mod sys;

use json::J;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SUPPORTED: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const INSTRUCTIONS: &str = "snifrig can explain why this PC is slow and suggest changes; the user approves every change in snifrig. \
There is no tool that applies fixes, approves pending fixes, or edits rules. Start with snifrig_status, then snifrig_why_slow, \
snifrig_top_processes and snifrig_recent_activity. If snifrig is not installed yet, snifrig_install opens its setup window; \
the person accepts the license there themselves (never type or click it for them). Use snifrig_suggest to propose a gentle rule (efficiency, background, cap) with a plain-words reason; never propose closing apps the user is using.";

const TOOLS_JSON: &str = r##"[
{"name":"snifrig_install_status","description":"Is snifrig installed for this Windows user, is its license accepted, is the monitor running, and is a setup program included with this extension. Read-only. Use it first when snifrig_status says snifrig is not installed.","inputSchema":{"type":"object","properties":{}}},
{"name":"snifrig_install","description":"Install (or update) snifrig on this PC: opens snifrig's setup window on the person's screen. No administrator rights, no download (the setup ships with this extension), installs for this Windows user only. The person must read and accept the license in that window themselves; you cannot accept it for them and must not type or click it for them. Ask the person before calling this.","inputSchema":{"type":"object","properties":{}}},
{"name":"snifrig_status","description":"Start here. One-screen summary of this Windows PC right now: snifrig's plain-English headline and likely causes, whether the monitor and fixer are running, fixer mode, whether snifrig fix is licensed, live CPU % and RAM, GPU, apps the governor is currently easing, performance-mode game, and how many fixes await the user's OK. Read-only.","inputSchema":{"type":"object","properties":{}}},
{"name":"snifrig_why_slow","description":"Runs snifrig's own diagnosis (`snifrig why`) and returns its explanation of why the PC is slow or laggy right now, based on its recent measurements (paging, commit, CPU, pool leaks, spawn churn). Read-only.","inputSchema":{"type":"object","properties":{}}},
{"name":"snifrig_report","description":"Runs `snifrig report` and returns a summary of slowdowns, alerts and leak findings over the last N days. Read-only.","inputSchema":{"type":"object","properties":{"days":{"type":"number","description":"How many days back to cover (default 7, 1-90)."}}}},
{"name":"snifrig_top_processes","description":"Live process list sampled over about 1 second: per-process CPU % (share of the whole machine, all cores), working set in MB, handle count, pid and parent pid. Use it to find what is eating CPU or RAM right now. Read-only.","inputSchema":{"type":"object","properties":{"limit":{"type":"number","description":"How many processes to return (default 15, max 100)."},"sort":{"type":"string","enum":["cpu","memory"],"description":"Sort by cpu (default) or memory."}}}},
{"name":"snifrig_recent_activity","description":"What snifrig's fixer and governor have recently done, newest first: apps eased or restored (governor), rule matches (rules), accepted fixes (fixes), and game performance mode (performance). Read-only.","inputSchema":{"type":"object","properties":{"limit":{"type":"number","description":"Max entries (default 30, max 200)."},"kind":{"type":"string","enum":["all","governor","rules","fixes","performance"],"description":"Filter by kind (default all)."}}}},
{"name":"snifrig_alerts","description":"Recent snifrig alerts (memory leaks, kernel pool growth, slowdowns), newest first. Read-only.","inputSchema":{"type":"object","properties":{"limit":{"type":"number","description":"Max alerts (default 20, max 200)."}}}},
{"name":"snifrig_rules","description":"Lists the user's current per-app fix rules (fix-rules.txt) and the governor settings in effect (with defaults filled in). Read-only; to propose a change use snifrig_suggest.","inputSchema":{"type":"object","properties":{}}},
{"name":"snifrig_pending","description":"Lists fixes snifrig is holding for the user's OK. You cannot approve or reject them; only the user can, in the snifrig window. Read-only.","inputSchema":{"type":"object","properties":{}}},
{"name":"snifrig_suggest","description":"Propose a change for the user to review. NOTHING is changed: the suggestion appears in the snifrig window with Accept and Dismiss. Requires snifrig fix (paid). kind=rule needs `rule` like \"comet.exe background=yes\" (keys: priority=idle|below_normal|normal, efficiency=on|off, io=very_low|low|normal, memory=very_low|low|normal, affinity=<hex or core list>, cap=1-100, max_instances=n, background=yes, governor=exempt|on, max_level=0-3, game=yes|no). kind=ease needs `app` (ask the governor to ease that app when the PC is busy). kind=setting needs `setting` like \"busy_pct=80\" (busy_pct, busy_secs, calm_pct, calm_secs, restore_every_secs, min_core_pct, max_per_tick, max_level, game_mode, game_plan, game_busy_pct, reapply_secs). kind=note is a free-text recommendation. Always give a plain-words `reason`. Prefer gentle rules (efficiency, background, cap) over anything that stops an app.","inputSchema":{"type":"object","properties":{"kind":{"type":"string","enum":["rule","ease","setting","note"]},"app":{"type":"string","description":"Executable name, e.g. comet.exe."},"rule":{"type":"string","description":"Full rule line: pattern then key=value pairs."},"setting":{"type":"string","description":"Governor setting as key=value."},"reason":{"type":"string","description":"Plain words for the user: why this helps."}},"required":["kind","reason"]}}
]"##;

pub struct Ctx {
    pub dir: PathBuf,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ---------- JSON-RPC ----------

fn ok_resp(id: &J, r: J) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}", id.ser(), r.ser())
}
fn err_resp(id: &J, code: i32, msg: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":{},\"message\":{}}}}}",
        id.ser(),
        code,
        J::s(msg).ser()
    )
}

pub fn handle(line: &str, cx: &Ctx) -> Option<String> {
    let req = match json::parse(line) {
        Ok(v) => v,
        Err(e) => return Some(err_resp(&J::Null, -32700, &format!("parse error: {e}"))),
    };
    if !matches!(req, J::Obj(_)) {
        return Some(err_resp(&J::Null, -32600, "invalid request"));
    }
    let id = req.get("id").cloned();
    let method = match req.get("method").and_then(|m| m.str()) {
        Some(m) => m.to_string(),
        None => {
            if req.get("result").is_some() || req.get("error").is_some() {
                return None;
            }
            return Some(err_resp(id.as_ref().unwrap_or(&J::Null), -32600, "missing method"));
        }
    };
    let params = req.get("params");
    let res: Result<J, (i32, String)> = match method.as_str() {
        "initialize" => Ok(init(params)),
        "ping" => Ok(J::Obj(vec![])),
        "tools/list" => Ok(J::obj(vec![("tools", json::parse(TOOLS_JSON).unwrap())])),
        "tools/call" => call(params, cx),
        m if m.starts_with("notifications/") => return None,
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    let id = id?;
    Some(match res {
        Ok(r) => ok_resp(&id, r),
        Err((c, m)) => err_resp(&id, c, &m),
    })
}

fn init(params: Option<&J>) -> J {
    let want = params.and_then(|p| p.get("protocolVersion")).and_then(|v| v.str()).unwrap_or("");
    let ver = if SUPPORTED.contains(&want) { want } else { "2025-06-18" };
    J::obj(vec![
        ("protocolVersion", J::s(ver)),
        ("capabilities", J::obj(vec![("tools", J::obj(vec![]))])),
        ("serverInfo", J::obj(vec![("name", J::s("snifrig")), ("version", J::s(VERSION))])),
        ("instructions", J::s(INSTRUCTIONS)),
    ])
}

fn call(params: Option<&J>, cx: &Ctx) -> Result<J, (i32, String)> {
    let p = params.ok_or((-32602, "missing params".to_string()))?;
    let name = p.get("name").and_then(|n| n.str()).ok_or((-32602, "missing tool name".to_string()))?;
    let empty = J::Obj(vec![]);
    let args = p.get("arguments").unwrap_or(&empty);
    let r = match name {
        "snifrig_install_status" => Ok(install_status(cx)),
        "snifrig_install" => install(cx),
        "snifrig_status" if !installed(cx) => Ok("snifrig is not installed on this PC yet. Ask the person whether to install it; snifrig_install opens its setup (free monitor, no administrator rights, no download).".into()),
        "snifrig_status" => Ok(status(cx)),
        "snifrig_why_slow" => run_tool(cx, "snifrig.exe", &["why"]),
        "snifrig_report" => {
            let d = num_arg(args, "days", 7, 1, 90);
            run_tool(cx, "snifrig.exe", &["report", "--days", &d.to_string()])
        }
        "snifrig_top_processes" => Ok(top_processes(args)),
        "snifrig_recent_activity" => Ok(recent_activity(cx, args)),
        "snifrig_alerts" => Ok(alerts(cx, args)),
        "snifrig_rules" => Ok(rules(cx)),
        "snifrig_pending" => Ok(pending(cx)),
        "snifrig_suggest" => suggest(cx, args),
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    let (text, is_err) = match r {
        Ok(t) => (t, false),
        Err(t) => (t, true),
    };
    Ok(J::obj(vec![
        ("content", J::Arr(vec![J::obj(vec![("type", J::s("text")), ("text", J::Str(text))])])),
        ("isError", J::Bool(is_err)),
    ]))
}

fn num_arg(a: &J, k: &str, def: i64, lo: i64, hi: i64) -> i64 {
    a.get(k).and_then(|v| v.f64()).map(|n| n as i64).unwrap_or(def).clamp(lo, hi)
}

// ---------- files ----------

fn read(cx: &Ctx, f: &str) -> Option<String> {
    fs::read(cx.dir.join(f)).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}
fn read_json(cx: &Ctx, f: &str) -> Option<J> {
    json::parse(read(cx, f)?.trim_start_matches('\u{feff}')).ok()
}
fn tail_text(path: &Path, max: u64) -> String {
    let Ok(mut f) = fs::File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max);
    let _ = io::Seek::seek(&mut f, io::SeekFrom::Start(start));
    let mut b = Vec::new();
    let _ = f.take(max).read_to_end(&mut b);
    let s = String::from_utf8_lossy(&b).into_owned();
    if start > 0 {
        s.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default()
    } else {
        s
    }
}
/// Parsed JSONL objects, newest first.
fn jsonl_newest(cx: &Ctx, f: &str) -> Vec<J> {
    tail_text(&cx.dir.join(f), 1 << 20)
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| json::parse(l.trim_start_matches('\u{feff}')).ok())
        .collect()
}
fn gs<'a>(j: &'a J, k: &str) -> &'a str {
    j.get(k).and_then(|v| v.str()).unwrap_or("")
}
fn gn(j: &J, k: &str) -> Option<f64> {
    j.get(k).and_then(|v| v.f64())
}
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push_str("...");
        t
    }
}

pub fn fmt_ts(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    let s = unix.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", s / 3600, s % 3600 / 60, s % 60)
}

// ---------- running snifrig tools ----------

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf()))
}
fn find_tool(cx: &Ctx, name: &str) -> Option<PathBuf> {
    let mut c = Vec::new();
    if let Some(d) = exe_dir() {
        c.push(d.join(name));
    }
    c.push(cx.dir.join("bin").join(name));
    c.into_iter().find(|p| p.exists())
}

fn run_tool(cx: &Ctx, name: &str, args: &[&str]) -> Result<String, String> {
    let exe = find_tool(cx, name).ok_or_else(|| {
        format!("{name} was not found next to snifrig-mcp.exe or in {}. Reinstall snifrig.", cx.dir.join("bin").display())
    })?;
    let mut cmd = Command::new(&exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x08000000);
    let mut child = cmd.spawn().map_err(|e| format!("could not start {name}: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    let mut er = child.stderr.take().unwrap();
    let t1 = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut out).take(1 << 20).read_to_end(&mut b);
        b
    });
    let t2 = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut er).take(1 << 16).read_to_end(&mut b);
        b
    });
    let t0 = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) if t0.elapsed() > Duration::from_secs(60) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} took longer than 60 seconds and was stopped."));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(format!("waiting for {name} failed: {e}")),
        }
    };
    let o = String::from_utf8_lossy(&t1.join().unwrap_or_default()).into_owned();
    let e = String::from_utf8_lossy(&t2.join().unwrap_or_default()).into_owned();
    let mut text = if o.trim().is_empty() { e.clone() } else { o };
    if text.chars().count() > 30000 {
        text = clip(&text, 30000);
    }
    if code == Some(0) {
        Ok(if text.trim().is_empty() { "(no output)".into() } else { text })
    } else {
        Err(format!("{name} exited with code {:?}.\n{}", code, text))
    }
}


// ---------- installing snifrig from the AI client ----------

/// The setup program shipped beside this server (in the Claude Desktop extension it sits in
/// server\). It carries every snifrig program, so installing needs no download.
fn bundled_setup() -> Option<PathBuf> {
    exe_dir().map(|d| d.join("snifrig-setup.exe")).filter(|p| p.exists())
}

fn installed(cx: &Ctx) -> bool { cx.dir.join("bin").join("snifrig.exe").exists() || cx.dir.join("status.json").exists() }

fn install_status(cx: &Ctx) -> String {
    let mut o = String::new();
    if installed(cx) {
        let accepted = read(cx, "license-accepted.json").map_or(false, |s| s.contains("\"license\""));
        let fresh = fs::metadata(cx.dir.join("status.json")).and_then(|m| m.modified()).ok()
            .and_then(|m| m.elapsed().ok()).map_or(false, |e| e < Duration::from_secs(300));
        o += &format!("snifrig is installed for this Windows user ({}).\n", cx.dir.join("bin").display());
        o += &format!("License accepted: {}.\n", if accepted { "yes" } else { "no (the person accepts it in the setup window)" });
        o += &format!("Monitor running: {}.\n", if fresh { "yes" } else { "not right now (it starts at login; snifrig_install can start setup again)" });
        o += "snifrig fix (the paid fixer) is set up separately in the snifrig window.\n";
    } else {
        o += "snifrig is not installed for this Windows user yet.\n";
    }
    o += &match bundled_setup() {
        Some(p) => format!("Setup for snifrig {} is included with this extension ({}); snifrig_install opens it.", VERSION, p.display()),
        None => "No setup program is included here. Download snifrig-setup.exe from https://github.com/estejosh/snifrig/releases and run it.".into(),
    };
    o
}

/// Opens the setup window on the person's screen. The license step is theirs: they read it and
/// type the acceptance themselves. Nothing here accepts it for them.
fn install(cx: &Ctx) -> Result<String, String> {
    let setup = bundled_setup().ok_or("No setup program is included with this extension. The person can download snifrig-setup.exe from https://github.com/estejosh/snifrig/releases and run it.")?;
    // `start` gives setup its own console window, so the person types in it (this server's
    // pipes to the AI client are not passed on).
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/d", "/c", "start", "snifrig setup"]).arg(&setup).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x08000000);
    let st = cmd.status().map_err(|e| format!("could not open snifrig setup: {e}"))?;
    if !st.success() { return Err("Windows did not open the snifrig setup window.".into()); }
    Ok(format!("The snifrig {} setup window is now open on the screen{}. Tell the person: it shows the license (Usufruct License 3.7; the monitor is free), and they type the acceptance line in that window themselves. You must not type or click it for them. Setup needs no administrator rights and no download; it installs for this Windows user only, starts snifrig, and adds it to Settings > Apps for uninstalling. When they say it is done, call snifrig_install_status, then snifrig_status.",
        VERSION, if installed(cx) { " (snifrig is already installed: this updates it)" } else { "" }))
}

// ---------- tools ----------

fn status(cx: &Ctx) -> String {
    let mut o = String::new();
    let t = now();
    let st = read_json(cx, "status.json");
    let gv = read_json(cx, "governor.json");
    match &st {
        Some(s) => {
            o += &format!("Headline: {}\n", gs(s, "headline"));
            o += &format!("Level: {} (severity {})\n", gs(s, "level"), gn(s, "severity").unwrap_or(0.0));
            if let Some(c) = s.get("causes").and_then(|c| c.arr()) {
                if !c.is_empty() {
                    o += "Likely causes:\n";
                    for x in c.iter().filter_map(|x| x.str()) {
                        o += &format!("- {x}\n");
                    }
                }
            }
        }
        None => o += "No status.json yet: the snifrig monitor has not written any data.\n",
    }
    let mon_age = st.as_ref().and_then(|s| gn(s, "unix")).map(|u| t - u as i64);
    let fix_age = gv.as_ref().and_then(|s| gn(s, "unix")).map(|u| t - u as i64);
    let run = |a: Option<i64>, lim: i64| match a {
        Some(a) if a < lim => format!("running (updated {a}s ago)"),
        Some(a) => format!("not running (last update {a}s ago)"),
        None => "not running (no data)".to_string(),
    };
    let licensed = cx.dir.join("snifrig-fix.key").exists();
    let mode = read(cx, "fix-mode.txt").map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".into());
    o += &format!("Monitor: {}\n", run(mon_age, 180));
    o += &format!("Fixer: {}; mode: {}; licensed: {}\n", run(fix_age, 30), mode, if licensed { "yes" } else { "no" });
    let cpu = sys::cpu_now();
    let (used, total) = sys::ram();
    o += &format!("CPU now: {:.0}%\nRAM: {:.1} GB used of {:.1} GB\n", cpu, used / 1024.0, total / 1024.0);
    if let Some(g) = read_json(cx, "gpu.json") {
        let mut s = String::from("GPU:");
        if let Some(v) = gn(&g, "temp_c") {
            s += &format!(" {v:.0} C");
        }
        if let (Some(p), Some(l)) = (gn(&g, "power_w"), gn(&g, "limit_w")) {
            s += &format!(", {p:.0} W of {l:.0} W");
        }
        if let (Some(u), Some(tt)) = (gn(&g, "used_mb"), gn(&g, "total_mb")) {
            s += &format!(", VRAM {:.1}/{:.1} GB", u / 1024.0, tt / 1024.0);
        }
        let r = gs(&g, "reasons");
        if !r.is_empty() {
            s += &format!(" (throttle: {r})");
        }
        o += &s;
        o += "\n";
    }
    if let Some(g) = &gv {
        if let Some(f) = g.get("feel") {
            o += &format!("How it feels (feel meter): {} (mouse/keyboard input arrives {:.0} ms late over {} events, front app answers in {:.0} ms, scheduling lag {:.0} ms, desktop compositor {:.0}% of a core{})\n",
                gs(f, "word"), gn(f, "input_ms").unwrap_or(0.0), gn(f, "input_events").unwrap_or(0.0), gn(f, "ping_ms").unwrap_or(0.0), gn(f, "lag_ms").unwrap_or(0.0), gn(f, "desktop_pct").unwrap_or(0.0), if matches!(f.get("hung"), Some(J::Bool(true))) { ", front window not responding" } else { "" });
        }
    }
    if let Some(i) = read_json(cx, "incident-now.json") {
        let t = gs(&i, "text");
        if !t.is_empty() { o += &format!("Seen before (incident memory): {t}\n"); }
    }
    if let Some(g) = &gv {
        let dem = g.get("demoted").and_then(|d| d.arr()).unwrap_or(&[]);
        if dem.is_empty() {
            o += "Governor: not easing any app right now\n";
        } else {
            o += "Governor is easing:\n";
            for d in dem {
                o += &format!("- {} (pid {}, level {}): {}\n", gs(d, "name"), gn(d, "pid").unwrap_or(0.0), gn(d, "level").unwrap_or(0.0), gs(d, "reason"));
            }
        }
        match g.get("game") {
            Some(J::Str(s)) if !s.is_empty() => o += &format!("Performance mode: game in front: {s}\n"),
            Some(J::Obj(_)) => o += &format!("Performance mode: game in front: {}\n", g.get("game").unwrap().ser()),
            _ => {}
        }
    }
    let pend = read_json(cx, "pending.json").and_then(|p| p.get("items").and_then(|i| i.arr()).map(|a| a.len())).unwrap_or(0);
    o += &format!("Fixes waiting for the user's OK: {pend}\n");
    if let Some(s) = &st {
        o += &format!("Alerts in last 2h: {}\n", gn(s, "alerts_2h").unwrap_or(0.0));
        let la = gs(s, "last_alert");
        if !la.is_empty() {
            o += &format!("Last alert: {}\n", clip(la, 300));
        }
        let pu = gn(s, "paused_until").unwrap_or(0.0) as i64;
        if pu > t {
            o += &format!("Alerts are paused for another {} min\n", (pu - t) / 60 + 1);
        }
    }
    o
}

fn top_processes(a: &J) -> String {
    let limit = num_arg(a, "limit", 15, 1, 100) as usize;
    let mem = a.get("sort").and_then(|s| s.str()) == Some("memory");
    let (rows, busy) = sys::top(limit, mem);
    let mut o = format!(
        "Sampled over ~1 s, sorted by {}. Total CPU busy: {:.0}%. CPU% is the share of the whole machine (all cores).\n{:>6} {:>9}  {:>7} {:>7} {:>8}  {}\n",
        if mem { "memory" } else { "CPU" }, busy, "CPU%", "RAM MB", "PID", "PPID", "Handles", "Name"
    );
    for r in rows {
        o += &format!("{:>6.1} {:>9.0}  {:>7} {:>7} {:>8}  {}\n", r.cpu, r.ws_mb, r.pid, r.ppid, r.handles, r.name);
    }
    o
}

fn kind_of(key: &str) -> &'static str {
    if key.starts_with("gov:") {
        "governor"
    } else if key.starts_with("rule:") {
        "rules"
    } else if key.starts_with("perf:") {
        "performance"
    } else {
        "fixes"
    }
}

fn recent_activity(cx: &Ctx, a: &J) -> String {
    let limit = num_arg(a, "limit", 30, 1, 200) as usize;
    let kind = a.get("kind").and_then(|k| k.str()).unwrap_or("all");
    let mut o = String::new();
    let mut n = 0;
    for e in jsonl_newest(cx, "fixes.jsonl") {
        let k = kind_of(gs(&e, "key"));
        if kind != "all" && kind != k {
            continue;
        }
        let pid = gn(&e, "pid").filter(|p| *p > 0.0).map(|p| format!(" pid {p:.0}")).unwrap_or_default();
        o += &format!(
            "{} [{}] {} {}{} ({}) {}\n",
            fmt_ts(gn(&e, "unix").unwrap_or(0.0) as i64), k, gs(&e, "action"), gs(&e, "name"), pid, gs(&e, "status"), clip(gs(&e, "note"), 300)
        );
        n += 1;
        if n >= limit {
            break;
        }
    }
    if o.is_empty() {
        "No matching activity recorded.".into()
    } else {
        o
    }
}

fn alerts(cx: &Ctx, a: &J) -> String {
    let limit = num_arg(a, "limit", 20, 1, 200) as usize;
    let mut o = String::new();
    for e in jsonl_newest(cx, "alerts.jsonl").into_iter().take(limit) {
        o += &format!("{} {}: {}\n", gs(&e, "t"), gs(&e, "key"), clip(gs(&e, "msg"), 600));
    }
    if o.is_empty() {
        "No alerts recorded.".into()
    } else {
        o
    }
}

const GOV_DEFAULTS: [(&str, &str); 12] = [
    ("busy_pct", "75"), ("busy_secs", "3"), ("calm_pct", "50"), ("calm_secs", "10"), ("restore_every_secs", "5"),
    ("min_core_pct", "25"), ("max_per_tick", "2"), ("max_level", "3"), ("game_mode", "on"), ("game_plan", "auto"),
    ("game_busy_pct", "50"), ("reapply_secs", "30"),
];

fn rules(cx: &Ctx) -> String {
    let mut o = String::from("Per-app rules (fix-rules.txt):\n");
    let mut n = 0;
    for l in read(cx, "fix-rules.txt").unwrap_or_default().lines() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        o += &format!("- {l}\n");
        n += 1;
    }
    if n == 0 {
        o += "(none)\n";
    }
    let mut set: Vec<(String, String)> = GOV_DEFAULTS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut custom = Vec::new();
    for l in read(cx, "fix-governor.txt").unwrap_or_default().lines() {
        let l = l.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = l.split_once('=') {
            let (k, v) = (k.trim(), v.trim());
            if let Some(s) = set.iter_mut().find(|s| s.0 == k) {
                s.1 = v.to_string();
                custom.push(k.to_string());
            }
        }
    }
    o += "\nGovernor settings in effect:\n";
    for (k, v) in set {
        o += &format!("- {k} = {v}{}\n", if custom.contains(&k) { "" } else { " (default)" });
    }
    o
}

fn pending(cx: &Ctx) -> String {
    let items = read_json(cx, "pending.json").and_then(|p| p.get("items").and_then(|i| i.arr().map(|a| a.to_vec()))).unwrap_or_default();
    if items.is_empty() {
        return "Nothing is waiting for the user's OK.".into();
    }
    let mut o = format!("{} fix(es) waiting for the user's OK in the snifrig window (you cannot approve them):\n", items.len());
    for i in items {
        o += &format!(
            "- {} [{}] {} {} pid {}: {} Why: {}\n",
            fmt_ts(gn(&i, "unix").unwrap_or(0.0) as i64), gs(&i, "id"), gs(&i, "action"), gs(&i, "name"),
            gn(&i, "pid").unwrap_or(0.0), clip(gs(&i, "msg"), 300), clip(gs(&i, "why"), 300)
        );
    }
    o
}

// ---------- suggest ----------

fn num_in(v: &str, lo: u32, hi: u32) -> bool {
    v.parse::<u32>().map(|n| n >= lo && n <= hi).unwrap_or(false)
}

fn rule_pair_ok(k: &str, v: &str) -> Result<(), String> {
    let ok = match k {
        "priority" => matches!(v, "idle" | "below_normal" | "normal"),
        "efficiency" => matches!(v, "on" | "off"),
        "io" | "memory" => matches!(v, "very_low" | "low" | "normal"),
        "affinity" => {
            let h = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X"));
            match h {
                Some(h) => !h.is_empty() && h.len() <= 16 && h.chars().all(|c| c.is_ascii_hexdigit()),
                None => !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '-') && v.chars().next().unwrap().is_ascii_digit(),
            }
        }
        "cap" => num_in(v, 1, 100),
        "max_instances" => num_in(v, 1, 10000),
        "background" => v == "yes",
        "governor" => matches!(v, "exempt" | "on"),
        "max_level" => num_in(v, 0, 3),
        "game" => matches!(v, "yes" | "no"),
        _ => return Err(format!("unknown rule key '{k}'")),
    };
    if ok { Ok(()) } else { Err(format!("invalid value '{v}' for '{k}'")) }
}

/// Validates "pattern key=value ..." and returns the normalized line.
pub fn validate_rule(s: &str) -> Result<String, String> {
    let mut it = s.split_whitespace();
    let pat = it.next().ok_or("rule is empty")?;
    if pat.contains('=') || pat.len() > 100 {
        return Err("a rule must start with the program name (e.g. comet.exe), then key=value pairs".into());
    }
    let mut out = pat.to_string();
    let mut n = 0;
    for kv in it {
        let (k, v) = kv.split_once('=').ok_or_else(|| format!("'{kv}' is not key=value"))?;
        rule_pair_ok(k, v)?;
        out += &format!(" {k}={v}");
        n += 1;
    }
    if n == 0 {
        return Err("a rule needs at least one key=value".into());
    }
    Ok(out)
}

pub fn validate_setting(s: &str) -> Result<String, String> {
    let (k, v) = s.split_once('=').ok_or("setting must be key=value")?;
    let (k, v) = (k.trim(), v.trim());
    let ok = match k {
        "busy_pct" | "calm_pct" | "min_core_pct" | "game_busy_pct" => num_in(v, 1, 100),
        "busy_secs" | "calm_secs" | "restore_every_secs" | "max_per_tick" | "reapply_secs" => num_in(v, 1, 86400),
        "max_level" => num_in(v, 0, 3),
        "game_mode" => matches!(v, "on" | "off"),
        "game_plan" => !v.is_empty() && v.len() <= 80 && v.chars().all(|c| c.is_ascii_alphanumeric() || " -_{}.".contains(c)),
        _ => return Err(format!("unknown governor setting '{k}'")),
    };
    if ok { Ok(format!("{k}={v}")) } else { Err(format!("invalid value '{v}' for '{k}'")) }
}

fn clean(s: &str, max: usize) -> Result<String, String> {
    let t = s.trim();
    if t.chars().any(|c| c.is_control()) {
        return Err("text must not contain control characters or line breaks".into());
    }
    if t.chars().count() > max {
        return Err(format!("text is too long (max {max} characters)"));
    }
    Ok(t.to_string())
}

fn suggest(cx: &Ctx, a: &J) -> Result<String, String> {
    if !cx.dir.join("snifrig-fix.key").exists() {
        return Err("Suggestions need snifrig fix (the paid fixer), which is not licensed on this PC. Everything else in snifrig is available. You can still explain the problem to the user in your reply.".into());
    }
    let kind = a.get("kind").and_then(|k| k.str()).unwrap_or("");
    let reason = clean(gs(a, "reason"), 500)?;
    if reason.is_empty() {
        return Err("reason is required: say in plain words why this helps".into());
    }
    let app = clean(gs(a, "app"), 100)?;
    let (mut rule, mut setting) = (String::new(), String::new());
    match kind {
        "rule" => rule = validate_rule(&clean(gs(a, "rule"), 300)?)?,
        "ease" => {
            if app.is_empty() || app.contains(char::is_whitespace) || app.contains('=') {
                return Err("kind=ease needs app, an executable name like comet.exe".into());
            }
        }
        "setting" => setting = validate_setting(&clean(gs(a, "setting"), 120)?)?,
        "note" => {}
        _ => return Err("kind must be rule, ease, setting or note".into()),
    }
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let id = format!("ai{:x}", ms);
    let entry = J::obj(vec![
        ("id", J::Str(id.clone())),
        ("unix", J::Num(now() as f64)),
        ("kind", J::s(kind)),
        ("app", J::Str(app)),
        ("rule", J::Str(rule)),
        ("setting", J::Str(setting)),
        ("reason", J::Str(reason)),
        ("source", J::s("ai")),
    ]);
    let mut items = match read_json(cx, "ai-suggestions.json").and_then(|j| j.get("items").and_then(|i| i.arr().map(|a| a.to_vec()))) {
        Some(v) => v,
        None => Vec::new(),
    };
    items.push(entry);
    if items.len() > 20 {
        items.drain(0..items.len() - 20);
    }
    let body = J::obj(vec![("items", J::Arr(items))]).ser();
    let tmp = cx.dir.join(format!("ai-suggestions.json.{id}.tmp"));
    fs::write(&tmp, body).map_err(|e| format!("could not save suggestion: {e}"))?;
    fs::rename(&tmp, cx.dir.join("ai-suggestions.json")).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("could not save suggestion: {e}")
    })?;
    Ok(format!("Suggestion {id} saved. Nothing has been changed: it is waiting in the snifrig window/popup for the user's OK (Accept or Dismiss). Tell the user to look there."))
}

// ---------- main ----------

fn data_dir() -> PathBuf {
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--dir" {
            if let Some(p) = it.next() {
                return PathBuf::from(p);
            }
        } else if let Some(p) = a.strip_prefix("--dir=") {
            return PathBuf::from(p);
        }
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("snifrig")
}

fn main() {
    let cx = Ctx { dir: data_dir() };
    eprintln!("snifrig-mcp {VERSION} data dir {}", cx.dir.display());
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(r) = handle(&line, &cx) {
            if writeln!(out, "{r}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> Ctx {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("target-mcp").join(format!("t-{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Ctx { dir: d }
    }
    fn rpc(cx: &Ctx, s: &str) -> J {
        json::parse(&handle(s, cx).expect("reply")).unwrap()
    }
    fn text(r: &J) -> String {
        r.get("result").unwrap().get("content").unwrap().arr().unwrap()[0].get("text").unwrap().str().unwrap().to_string()
    }

    #[test]
    fn rules_validation() {
        assert_eq!(validate_rule("comet.exe  background=yes").unwrap(), "comet.exe background=yes");
        assert!(validate_rule("a.exe cap=0").is_err());
        assert!(validate_rule("a.exe cap=101").is_err());
        assert!(validate_rule("a.exe priority=high").is_err());
        assert!(validate_rule("a.exe bogus=1").is_err());
        assert!(validate_rule("a.exe").is_err());
        assert!(validate_rule("priority=idle").is_err());
        assert!(validate_rule("a.exe affinity=0x8000 efficiency=on io=low memory=very_low max_level=2 game=no governor=exempt").is_ok());
        assert!(validate_rule("a.exe affinity=0-3,6").is_ok());
        assert!(validate_rule("a.exe affinity=zz").is_err());
        assert!(validate_setting("busy_pct=80").is_ok());
        assert!(validate_setting("busy_pct=0").is_err());
        assert!(validate_setting("game_mode=maybe").is_err());
        assert!(validate_setting("nope=1").is_err());
    }

    #[test]
    fn protocol() {
        let cx = tmp("proto");
        let r = rpc(&cx, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#);
        let res = r.get("result").unwrap();
        assert_eq!(res.get("protocolVersion").unwrap().str(), Some("2025-03-26"));
        assert!(res.get("instructions").unwrap().str().unwrap().contains("user approves every change"));
        let r = rpc(&cx, r#"{"jsonrpc":"2.0","id":"x","method":"initialize","params":{"protocolVersion":"1999"}}"#);
        assert_eq!(r.get("result").unwrap().get("protocolVersion").unwrap().str(), Some("2025-06-18"));
        assert!(handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &cx).is_none());
        let r = rpc(&cx, r#"{"jsonrpc":"2.0","id":2,"method":"nope"}"#);
        assert_eq!(r.get("error").unwrap().get("code").unwrap().f64(), Some(-32601.0));
        let r = rpc(&cx, "{bad");
        assert_eq!(r.get("error").unwrap().get("code").unwrap().f64(), Some(-32700.0));
        rpc(&cx, r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#);
        let r = rpc(&cx, r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#);
        let tools = r.get("result").unwrap().get("tools").unwrap().arr().unwrap();
        assert_eq!(tools.len(), 11);
        // Not installed (no bin\snifrig.exe here): status says so and points at snifrig_install.
        let s = text(&rpc(&cx, r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"snifrig_status"}}"#));
        assert!(s.contains("not installed") && s.contains("snifrig_install"), "{s}");
        let s = text(&rpc(&cx, r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"snifrig_install_status"}}"#));
        assert!(s.contains("not installed"), "{s}");
        let r = rpc(&cx, r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"zzz"}}"#);
        assert!(r.get("error").is_some());
    }

    #[test]
    fn data_tools() {
        let cx = tmp("data");
        fs::write(cx.dir.join("fixes.jsonl"), concat!(
            "{\"unix\":1791826000,\"key\":\"gov:a.exe#1\",\"action\":\"report\",\"pid\":1,\"name\":\"a.exe\",\"status\":\"done\",\"note\":\"eased\"}\n",
            "{\"unix\":1791826100,\"key\":\"rule:b.exe\",\"action\":\"priority\",\"pid\":2,\"name\":\"b.exe\",\"status\":\"done\",\"note\":\"x\"}\n")).unwrap();
        fs::write(cx.dir.join("alerts.jsonl"), "{\"t\":\"T1\",\"key\":\"k1\",\"msg\":\"m1\"}\n{\"t\":\"T2\",\"key\":\"k2\",\"msg\":\"m2\"}\n").unwrap();
        fs::write(cx.dir.join("fix-rules.txt"), "# c\ncomet.exe background=yes\n").unwrap();
        fs::write(cx.dir.join("fix-governor.txt"), "# busy_pct = 1\nbusy_pct = 80 # hi\n").unwrap();
        let call = |n: &str, a: &str| text(&rpc(&cx, &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{n}","arguments":{a}}}}}"#)));
        let t = call("snifrig_recent_activity", r#"{"kind":"rules"}"#);
        assert!(t.contains("b.exe") && !t.contains("a.exe"));
        let t = call("snifrig_recent_activity", "{}");
        assert!(t.find("b.exe").unwrap() < t.find("a.exe").unwrap());
        assert!(call("snifrig_alerts", r#"{"limit":1}"#).contains("k2") );
        let t = call("snifrig_rules", "{}");
        assert!(t.contains("comet.exe background=yes") && t.contains("busy_pct = 80\n") && t.contains("busy_secs = 3 (default)"));
        assert!(call("snifrig_pending", "{}").contains("Nothing"));
    }

    #[test]
    fn suggest_flow() {
        let cx = tmp("suggest");
        let a = r#"{"kind":"rule","rule":"comet.exe background=yes","reason":"it is busy"}"#;
        let req = |a: &str| rpc(&cx, &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"snifrig_suggest","arguments":{a}}}}}"#));
        let r = req(a);
        assert_eq!(r.get("result").unwrap().get("isError").unwrap(), &J::Bool(true));
        fs::write(cx.dir.join("snifrig-fix.key"), "x").unwrap();
        for i in 0..22 {
            let r = req(&a.replace("busy", &format!("busy{i}")));
            assert_eq!(r.get("result").unwrap().get("isError").unwrap(), &J::Bool(false), "{}", text(&r));
            std::thread::sleep(Duration::from_millis(2));
        }
        let j = read_json(&cx, "ai-suggestions.json").unwrap();
        let items = j.get("items").unwrap().arr().unwrap();
        assert_eq!(items.len(), 20);
        assert_eq!(items[19].get("source").unwrap().str(), Some("ai"));
        assert!(gs(&items[19], "reason").contains("busy21"));
        assert_eq!(req(r#"{"kind":"rule","rule":"a.exe cap=500","reason":"r"}"#).get("result").unwrap().get("isError").unwrap(), &J::Bool(true));
        assert_eq!(req(r#"{"kind":"note","reason":""}"#).get("result").unwrap().get("isError").unwrap(), &J::Bool(true));
        assert_eq!(req(r#"{"kind":"setting","setting":"busy_pct=70","reason":"r"}"#).get("result").unwrap().get("isError").unwrap(), &J::Bool(false));
    }

    #[test]
    fn timestamps() {
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_ts(1791826704), "2026-10-12 17:38:24 UTC");
    }
}

//! The brain: asks a local OpenJev ("System One" decision server) what a program IS, never
//! what to do. Measured on beastly (docs/BRAIN-API.md): the 9B model is poor at reading CPU
//! numbers but good at semantic calls like "interactive app or background batch?". So numbers
//! stay with the rules and learner; the brain only classifies programs, and its answers shape
//! how hard the governor may lean on them.
//!
//! Cost control: each program is asked about once and the answer is cached for 7 days
//! (brain-cache.json); calls run on one worker thread, one at a time, at most one every
//! MIN_GAP_SECS, so the governor's 1 s tick never waits on the GPU. If the server is down the
//! brain simply has no opinion and everything works as before. Local only: plain HTTP to
//! 127.0.0.1, no other network, in line with UFL Section 10.

use crate::json::{esc, num_field, str_field};
use crate::governor::Hints;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_URL: &str = "http://127.0.0.1:8791/v1/systemone";
pub const CACHE_SECS: f64 = 7.0 * 86400.0;
pub const MIN_GAP_SECS: f64 = 20.0;
/// Below this choice confidence the brain's answer is ignored.
pub const MIN_CONF: f64 = 0.35;

/// What kind of program something is, as the brain sees it.
pub const KINDS: [(&str, &str); 8] = [
    ("interactive", "an app the user is actively using: browser, editor, chat, terminal, office or design app"),
    ("game", "a video game or game launcher"),
    ("media_batch", "video or audio encoding, decoding, rendering or conversion running unattended"),
    ("ai_model", "an AI model server, inference engine or training job"),
    ("dev_build", "a compiler, build tool, package manager or test runner"),
    ("sync_backup", "file sync, backup, cloud drive or search indexing"),
    ("system", "part of Windows or a hardware driver helper"),
    ("background", "some other background helper, agent or script"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict { pub kind: String, pub confidence: f64, pub importance: f64, pub unix: f64 }

/// How a verdict changes the governor: weight multiplier and the deepest level allowed.
pub fn shape(v: &Verdict) -> (f64, u8) {
    if v.confidence < MIN_CONF { return (1.0, 3); }
    let important = v.importance >= 2.5; // 0..4 scale
    match v.kind.as_str() {
        "interactive" | "game" if important => (0.3, 1), // efficiency mode at most
        "interactive" | "game" => (0.6, 2),
        "system" => (0.5, 1),
        "ai_model" => (1.0, 2), // local models the owner runs: never idle-starved
        "media_batch" | "dev_build" | "sync_backup" | "background" if !important => (1.5, 3),
        _ => (1.0, 3),
    }
}

/// Fold cached verdicts into the learner's hints.
pub fn apply(hints: &mut Hints, cache: &HashMap<String, Verdict>) {
    for (name, v) in cache {
        let (w, max) = shape(v);
        hints.boost.insert(name.clone(), w);
        hints.max_level.insert(name.clone(), max);
    }
}

fn state_for(name: &str, cmdline: &str, parent: &str) -> String {
    let cmd: String = cmdline.chars().take(600).collect();
    format!("Windows process. Image name: {}. Command line: {}. Started by: {}.", name, cmd, parent)
}

/// The request body. Pure, for tests.
pub fn request_body(name: &str, cmdline: &str, parent: &str) -> String {
    let kinds = KINDS.iter().map(|(k, d)| format!("\"{}\":\"{}\"", k, esc(d))).collect::<Vec<_>>().join(",");
    format!(
        "{{\"state\":\"{}\",\"questions\":{{\"kind\":{{\"type\":\"choice\",\"instructions\":\"What kind of program is this?\",\"criteria\":{{{}}}}},\"importance\":{{\"type\":\"score\",\"instructions\":\"How much would the person at this computer notice or mind if this program ran slower right now?\",\"criteria\":[\"not at all: unattended background work\",\"barely\",\"somewhat\",\"a lot\",\"immediately: it is what they are using\"]}}}}}}",
        esc(&state_for(name, cmdline, parent)), kinds)
}

/// Parse the response. Pure, for tests. Tolerates field order.
pub fn parse_response(body: &str, now: f64) -> Option<Verdict> {
    let body = &compact(body);
    let kpos = body.find("\"kind\"")?;
    let kind_part = &body[kpos..];
    let kind = str_field(kind_part, "choice")?;
    let confidence = num_field(kind_part, "confidence").unwrap_or(0.0);
    let ipos = body.find("\"importance\"")?;
    let importance = num_field(&body[ipos..], "score").unwrap_or(2.0);
    if !KINDS.iter().any(|(k, _)| *k == kind) { return None; }
    Some(Verdict { kind, confidence, importance, unix: now })
}

/// Drop whitespace outside strings, so `"k": "v"` (Python's json style) reads like `"k":"v"`.
fn compact(s: &str) -> String {
    let (mut out, mut in_str, mut esc) = (String::with_capacity(s.len()), false, false);
    for c in s.chars() {
        if in_str {
            out.push(c);
            if esc { esc = false } else if c == '\\' { esc = true } else if c == '"' { in_str = false }
        } else if c == '"' { in_str = true; out.push(c) } else if !c.is_whitespace() { out.push(c) }
    }
    out
}

/// Minimal HTTP/1.1 POST over a plain TCP socket to a loopback server.
fn post(url: &str, body: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or("brain url must be http:// on this machine")?;
    let (hostport, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{}", p))).unwrap_or((rest, "/".into()));
    let host = hostport.split(':').next().unwrap_or("");
    if !(host == "127.0.0.1" || host == "localhost" || host == "::1") { return Err("brain must be local (127.0.0.1)".into()); }
    let addr = if hostport.contains(':') { hostport.to_string() } else { format!("{}:80", hostport) };
    let mut s = std::net::TcpStream::connect(&addr).map_err(|e| format!("brain not reachable: {}", e))?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let req = format!("POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", path, hostport, body.len(), body);
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    let status = out.split_whitespace().nth(1).unwrap_or("");
    if status != "200" { return Err(format!("brain answered HTTP {}", status)); }
    Ok(out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

// ---- on demand: the brain server runs only while it has questions ----
// Josh, 16 Nov: the brain is there to make the PC faster, not to sit on 24 GB (about 15 GB of
// RAM plus 18 GB of graphics memory for the 9B model) all day. So snifrig starts it for a batch
// of questions, asks them back to back, and stops it once the queue has been empty for
// IDLE_STOP_SECS. Judging whether the fixes worked (learn.rs, impact.rs) needs no brain.

/// Stop the server after this long with nothing to ask.
pub const IDLE_STOP_SECS: f64 = 90.0;
/// Give the model this long to load before giving up on a start.
pub const START_WAIT_SECS: f64 = 240.0;
/// Do not try to start it more often than this.
pub const RETRY_START_SECS: f64 = 900.0;
/// The model needs about this much free graphics memory.
pub const NEED_VRAM_MB: f64 = 19000.0;
/// ...and the PC must have this much commit headroom (it adds about 15 GB).
pub const MAX_COMMIT_PCT: f64 = 75.0;
/// Leave the server alone (never start or stop it) while this file exists in the data dir.
pub const MANUAL_FILE: &str = "brain-manual";

fn reachable(url: &str) -> bool {
    let hp = url.strip_prefix("http://").and_then(|r| r.split('/').next()).unwrap_or("127.0.0.1:8791");
    hp.parse::<std::net::SocketAddr>().map_or(false, |a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok())
}

/// Room to start the model: fresh GPU reading with enough free memory, and commit headroom.
fn can_start(dir: &Path) -> Result<(), String> {
    let g = std::fs::read_to_string(dir.join("gpu.json")).unwrap_or_default();
    let fresh = num_field(&g, "unix").map_or(false, |u| crate::now_unix() - u < 900.0);
    let (used, total) = (num_field(&g, "used_mb").unwrap_or(0.0), num_field(&g, "total_mb").unwrap_or(0.0));
    if !fresh || total <= 0.0 { return Err("no fresh graphics card reading".into()); }
    if total - used < NEED_VRAM_MB { return Err(format!("graphics card has {:.1} GB free, needs {:.0}", (total - used) / 1024.0, NEED_VRAM_MB / 1024.0)); }
    if let Some((pct, _)) = crate::sys::memory_status() { if pct > MAX_COMMIT_PCT { return Err(format!("memory {:.0}% committed", pct)); } }
    Ok(())
}

fn run_cmd(dir: &Path, file: &str, default: &[&str]) -> Result<(), String> {
    let custom = std::fs::read_to_string(dir.join(file)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut c = match &custom {
        #[cfg(windows)]
        Some(c) => { let mut x = std::process::Command::new("cmd.exe"); x.args(["/c", c]); x }
        #[cfg(not(windows))]
        Some(c) => { let mut x = std::process::Command::new("sh"); x.args(["-c", c]); x }
        None => { let mut x = std::process::Command::new(default[0]); x.args(&default[1..]); x }
    };
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; c.creation_flags(0x0800_0000); }
    let st = c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err(format!("{} exited with {}", custom.as_deref().unwrap_or(default[0]), st)) }
}

#[cfg(windows)]
const START_CMD: &[&str] = &["wsl.exe", "-d", "Ubuntu", "--exec", "podman", "start", "open-jev"];
#[cfg(windows)]
const STOP_CMD: &[&str] = &["wsl.exe", "-d", "Ubuntu", "--exec", "podman", "stop", "open-jev"];
#[cfg(not(windows))]
const START_CMD: &[&str] = &["podman", "start", "open-jev"];
#[cfg(not(windows))]
const STOP_CMD: &[&str] = &["podman", "stop", "open-jev"];

/// Start the server (brain-start.txt in the data dir overrides the command).
pub fn start_server(dir: &Path) -> Result<(), String> { run_cmd(dir, "brain-start.txt", START_CMD) }
/// Stop the server (brain-stop.txt in the data dir overrides the command).
pub fn stop_server(dir: &Path) -> Result<(), String> { run_cmd(dir, "brain-stop.txt", STOP_CMD) }

fn note(dir: &Path, msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("brain-log.txt")) { let _ = writeln!(f, "{} {}", crate::now_unix() as u64, msg); }
}

struct Shared { queue: VecDeque<(String, String, String)>, queued: HashSet<String>, cache: HashMap<String, Verdict>, last_err: String }

pub struct Brain { shared: Arc<Mutex<Shared>>, path: PathBuf }

impl Brain {
    /// Starts the worker thread. `url` comes from jev.txt in the data dir if present.
    pub fn start(dir: &Path) -> Brain {
        let url = std::fs::read_to_string(dir.join("jev.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_URL.to_string());
        let path = dir.join("brain-cache.json");
        let cache = load_cache(&path);
        let shared = Arc::new(Mutex::new(Shared { queue: VecDeque::new(), queued: HashSet::new(), cache, last_err: String::new() }));
        let (sh, p, d) = (shared.clone(), path.clone(), dir.to_path_buf());
        std::thread::spawn(move || {
            // Already up when snifrig starts (autostart, an earlier run): treat it as ours, so it
            // is stopped once idle. brain-manual in the data dir leaves it to you.
            let manual = || d.join(MANUAL_FILE).exists();
            let mut ours = !manual() && reachable(&url);
            let (mut idle_since, mut last_start) = (crate::now_unix(), -1e12f64);
            loop {
                std::thread::sleep(Duration::from_secs_f64(if ours { 2.0 } else { MIN_GAP_SECS }));
                let now = crate::now_unix();
                // Came up on its own (WSL started later and the container autostarts): adopt it.
                if !ours && !manual() && reachable(&url) { ours = true; idle_since = now; }
                let has_job = sh.lock().map(|g| !g.queue.is_empty()).unwrap_or(false);
                if !has_job {
                    if ours && !manual() && now - idle_since >= IDLE_STOP_SECS {
                        match stop_server(&d) { Ok(()) => note(&d, "stopped the brain: nothing left to ask"), Err(e) => note(&d, &format!("could not stop the brain: {}", e)) }
                        ours = false;
                    }
                    continue;
                }
                if !reachable(&url) {
                    if manual() || now - last_start < RETRY_START_SECS { continue; }
                    last_start = now;
                    if let Err(why) = can_start(&d) { note(&d, &format!("questions waiting, brain not started: {}", why)); continue; }
                    if let Err(e) = start_server(&d) { note(&d, &format!("could not start the brain: {}", e)); continue; }
                    let t0 = crate::now_unix();
                    while !reachable(&url) && crate::now_unix() - t0 < START_WAIT_SECS { std::thread::sleep(Duration::from_secs(3)); }
                    if !reachable(&url) { note(&d, "started the brain but it did not come up; stopping it"); let _ = stop_server(&d); continue; }
                    note(&d, &format!("started the brain for {} question(s) (took {:.0} s)", sh.lock().map(|g| g.queue.len()).unwrap_or(0), crate::now_unix() - t0));
                    ours = true;
                }
                let job = sh.lock().ok().and_then(|mut g| g.queue.pop_front());
                let Some((name, cmd, parent)) = job else { continue };
                idle_since = crate::now_unix();
                let res = post(&url, &request_body(&name, &cmd, &parent)).and_then(|b| parse_response(&b, crate::now_unix()).ok_or_else(|| "brain gave an unreadable answer".to_string()));
                if let Ok(mut g) = sh.lock() {
                    g.queued.remove(&name);
                    match res {
                        Ok(v) => { g.cache.insert(name, v); save_cache(&p, &g.cache); g.last_err.clear(); }
                        Err(e) => { let _ = std::fs::write(p.with_file_name("brain-error.txt"), format!("{} {}: {}\n", crate::now_unix() as u64, name, e)); g.last_err = e; }
                    }
                }
            }
        });
        Brain { shared, path }
    }

    /// Ask about a program unless we know it (fresh cache) or already asked.
    pub fn want(&self, name: &str, cmdline: &str, parent: &str) {
        let key = name.to_lowercase();
        let now = crate::now_unix();
        if let Ok(mut g) = self.shared.lock() {
            let fresh = g.cache.get(&key).map_or(false, |v| now - v.unix < CACHE_SECS);
            if fresh || g.queued.contains(&key) || g.queue.len() >= 20 { return; }
            g.queued.insert(key.clone());
            g.queue.push_back((key, cmdline.to_string(), parent.to_string()));
        }
    }

    pub fn cache(&self) -> HashMap<String, Verdict> { self.shared.lock().map(|g| g.cache.clone()).unwrap_or_default() }
    pub fn last_error(&self) -> String { self.shared.lock().map(|g| g.last_err.clone()).unwrap_or_default() }
    pub fn cache_path(&self) -> &Path { &self.path }
}

/// One synchronous question, for `snifrig-fix brain <name>`; returns the verdict or the raw reply on failure.
pub fn ask_now(dir: &Path, name: &str, cmdline: &str, parent: &str) -> Result<Verdict, String> {
    let url = std::fs::read_to_string(dir.join("jev.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_URL.to_string());
    let body = post(&url, &request_body(name, cmdline, parent))?;
    parse_response(&body, crate::now_unix()).ok_or_else(|| format!("unreadable answer: {}", body.chars().take(600).collect::<String>()))
}

fn load_cache(path: &Path) -> HashMap<String, Verdict> {
    let mut m = HashMap::new();
    let Ok(s) = std::fs::read_to_string(path) else { return m };
    for line in s.lines() {
        if let (Some(n), Some(k)) = (str_field(line, "name"), str_field(line, "kind")) {
            m.insert(n, Verdict { kind: k, confidence: num_field(line, "confidence").unwrap_or(0.0), importance: num_field(line, "importance").unwrap_or(2.0), unix: num_field(line, "unix").unwrap_or(0.0) });
        }
    }
    m
}

/// One JSON object per line; written atomically.
fn save_cache(path: &Path, cache: &HashMap<String, Verdict>) {
    let mut s = String::new();
    for (n, v) in cache {
        s.push_str(&format!("{{\"name\":\"{}\",\"kind\":\"{}\",\"confidence\":{:.3},\"importance\":{:.2},\"unix\":{:.0}}}\n", esc(n), esc(&v.kind), v.confidence, v.importance, v.unix));
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, s).is_ok() { let _ = std::fs::rename(&tmp, path); }
}

#[cfg(test)]
mod brain_tests {
    use super::*;

    #[test]
    fn request_is_valid_shape() {
        let b = request_body("ffmpeg.exe", "ffmpeg -i \"X:\\a.mp4\" -f null -", "python build-playout.py");
        assert!(b.starts_with("{\"state\":\""));
        assert!(b.contains("\"type\":\"choice\"") && b.contains("\"type\":\"score\""));
        assert!(b.contains("\\\"X:\\\\a.mp4\\\""), "quotes and backslashes escaped: {}", b);
    }

    #[test]
    fn parses_spaced_python_json() {
        let body = r#"{"answers": {"kind": {"type": "choice", "choice": "game", "probabilities": {"game": 0.97}, "confidence": 0.97}, "importance": {"type": "score", "score": 3.0, "probabilities": {"4": 0.5}, "confidence": 0.5}}, "model": "x y"}"#;
        let v = parse_response(body, 1.0).unwrap();
        assert_eq!(v.kind, "game");
        assert!((v.confidence - 0.97).abs() < 1e-9 && (v.importance - 3.0).abs() < 1e-9);
        assert_eq!(shape(&v), (0.3, 1));
    }

    #[test]
    fn parses_server_answer() {
        let body = r#"{"answers":{"kind":{"type":"choice","choice":"media_batch","probabilities":{"media_batch":0.62},"confidence":0.55},"importance":{"type":"score","score":0.8,"probabilities":{},"confidence":0.4}}}"#;
        let v = parse_response(body, 10.0).unwrap();
        assert_eq!(v.kind, "media_batch");
        assert!((v.importance - 0.8).abs() < 1e-9 && (v.confidence - 0.55).abs() < 1e-9);
        assert!(parse_response(r#"{"answers":{"kind":{"choice":"nonsense","confidence":0.9},"importance":{"score":1}}}"#, 0.0).is_none());
    }

    #[test]
    fn shaping_protects_important_interactive_and_leans_on_batch() {
        let v = |k: &str, c: f64, i: f64| Verdict { kind: k.into(), confidence: c, importance: i, unix: 0.0 };
        assert_eq!(shape(&v("interactive", 0.6, 3.5)), (0.3, 1));
        assert_eq!(shape(&v("media_batch", 0.6, 0.8)), (1.5, 3));
        assert_eq!(shape(&v("media_batch", 0.2, 0.8)), (1.0, 3), "low confidence: no opinion");
        assert_eq!(shape(&v("system", 0.9, 1.0)), (0.5, 1));
    }

    #[test]
    fn refuses_non_local_urls() {
        assert!(post("http://example.com/v1/systemone", "{}").unwrap_err().contains("local"));
        assert!(post("https://127.0.0.1:8791/x", "{}").is_err());
    }
}

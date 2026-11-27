//! What a program is, in plain words, for the cards that ask the user to decide about it
//! (27 Nov: a card asked about "DPM.exe" with no hint that it is Dell Peripheral Manager).
//!
//! Sources, best first:
//! 1. What the user or an AI session taught snifrig (identities.tsv, `snifrig-fix whois teach`,
//!    MCP `snifrig_identify`). Kept forever, so snifrig learns each program once.
//! 2. The program file's own description, product and maker (Windows version info).
//! 3. Who starts it: the Windows service it runs under (or its parent's), else the parent program.
//!
//! identities.tsv: one line per program: key (full path or file name, lowercase) \t text \t source
//! (user | ai | unknown). Programs snifrig could not describe are listed as "unknown", so a session
//! can see what to look up.

use std::path::Path;

pub const FILE: &str = "identities.tsv";

/// What was found about a running program.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub path: String,
    pub description: String,
    pub product: String,
    pub company: String,
    /// Display name of the service that runs it (its own or its parent's).
    pub service: Option<String>,
    /// File name of the program that started it.
    pub parent: Option<String>,
    pub cmdline: String,
}

fn stem(name: &str) -> String { name.to_ascii_lowercase().trim_end_matches(".exe").to_string() }

/// Programs Windows will not describe (protected, or no version info), and snifrig's own.
const KNOWN: &[(&str, &str)] = &[
    ("svchost", "Windows' service host: it runs Windows services (Task Manager's Services tab shows which ones)"),
    ("vmmemwsl", "the memory WSL (Linux on Windows) is using"),
    ("vmmem", "the memory a virtual machine or WSL is using"),
    ("system", "the Windows kernel"),
    ("registry", "the Windows registry, held in memory"),
    ("memory compression", "Windows' compressed memory store"),
    ("msmpeng", "Microsoft Defender antivirus"),
    ("searchindexer", "Windows Search, indexing your files"),
    ("dwm", "Windows' desktop window manager; it draws the screen"),
    ("explorer", "Windows' desktop, taskbar and File Explorer"),
    ("wmiprvse", "Windows Management Instrumentation, which answers programs' questions about the PC"),
    ("conhost", "the window behind a command-line program"),
    ("snifrigd", "snifrig's monitor"),
    ("snifrig-fix", "snifrig's fixer"),
    ("snifrig-tray", "snifrig's tray icon"),
    ("python", "Python, which runs Python scripts"),
    ("pythonw", "Python, which runs Python scripts"),
    ("node", "Node.js, which runs JavaScript programs"),
    ("java", "Java"),
    ("powershell", "Windows PowerShell"),
    ("pwsh", "PowerShell"),
    ("cmd", "the Windows command prompt"),
    ("bash", "a bash shell (Git Bash or WSL)"),
    ("git", "Git"),
];

/// Programs that run something else; the command line says what.
const RUNNERS: &[&str] = &["python", "pythonw", "node", "java", "javaw", "pwsh", "powershell", "cmd", "bash", "wscript", "cscript", "ruby", "perl", "deno", "bun", "uv", "dotnet", "rundll32"];

fn known(name: &str) -> Option<&'static str> { let s = stem(name); KNOWN.iter().find(|k| k.0 == s).map(|k| k.1) }

/// Pure: "C:\Python314\python.exe  X:\natv\train.py --steps 900" -> "X:\natv\train.py --steps 900".
fn running(name: &str, cmdline: &str) -> Option<String> {
    if !RUNNERS.contains(&stem(name).as_str()) { return None; }
    let c = cmdline.trim();
    let rest = if let Some(r) = c.strip_prefix('"') { r.split_once('"').map(|x| x.1).unwrap_or("") } else { c.split_once(' ').map(|x| x.1).unwrap_or("") };
    let rest = rest.trim();
    if rest.is_empty() { return None; }
    Some(if rest.chars().count() > 160 { rest.chars().take(160).collect::<String>() + "…" } else { rest.to_string() })
}

/// Pure: whether the file itself says anything beyond its own name.
pub fn has_label(name: &str, f: &Facts) -> bool { label(name, f).is_some() }

fn label(name: &str, f: &Facts) -> Option<String> {
    let s = stem(name);
    [&f.description, &f.product].into_iter().map(|x| x.trim()).find(|x| !x.is_empty() && x.to_ascii_lowercase().trim_end_matches(".exe") != s).map(str::to_string)
}

/// Pure: the plain-words text for a card.
pub fn sentence(name: &str, f: &Facts, learned: Option<&str>) -> String {
    let learned = learned.map(str::to_string).or_else(|| known(name).map(str::to_string));
    let mut out = match (learned.as_deref(), label(name, f)) {
        (Some(t), _) => format!("{} is {}", name, t.trim().trim_end_matches('.').to_string() + "."),
        (None, Some(l)) => {
            let by = f.company.trim();
            let by = if by.is_empty() || l.to_ascii_lowercase().contains(&by.to_ascii_lowercase()) { String::new() } else { format!(" by {}", by) };
            format!("{} is {}{}", name, l, by).trim_end_matches('.').to_string() + "."
        }
        (None, None) if is_windows_path(&f.path) => format!("{} is part of Windows.", name),
        (None, None) if f.path.is_empty() => format!("snifrig does not know what {} is yet.", name),
        (None, None) => format!("snifrig does not know what {} is yet. It runs from {}.", name, folder(&f.path)),
    };
    if let Some(r) = running(name, &f.cmdline) { out.push_str(&format!(" It is running: {}.", r.trim_end_matches('.'))); }
    if let Some(s) = &f.service {
        out.push_str(&format!(" It runs as part of the Windows service \"{}\", so it starts again after it is closed.", s));
    } else if let Some(p) = &f.parent {
        if !p.eq_ignore_ascii_case(name) { out.push_str(&format!(" {} started it.", p)); }
    }
    out
}

fn is_windows_path(p: &str) -> bool {
    let p = p.to_ascii_lowercase().replace('/', "\\");
    p.contains(":\\windows\\") || p.starts_with("/usr/") || p.starts_with("/lib")
}

fn folder(p: &str) -> String { p.rsplit_once(['\\', '/']).map(|(d, _)| d.to_string()).unwrap_or_else(|| p.to_string()) }

/// Pure: "growing 1506 handles/h (now 10313, 60 samples" -> "growing 1506 handles/h, now 10313".
pub fn growth(msg: &str) -> Option<String> {
    let rest = &msg[msg.find("growing ")?..];
    let end = rest.find(", ").unwrap_or(rest.len());
    let s = rest[..end].replace(" (now ", ", now ").replace(')', "");
    Some(s.trim_end_matches('.').to_string())
}

fn load(dir: &Path) -> Vec<(String, String, String)> {
    std::fs::read_to_string(dir.join(FILE)).unwrap_or_default().lines().filter_map(|l| {
        let mut f = l.splitn(3, '\t');
        Some((f.next()?.to_string(), f.next()?.to_string(), f.next().unwrap_or("").trim().to_string()))
    }).collect()
}

fn save(dir: &Path, rows: &[(String, String, String)]) {
    let s: String = rows.iter().map(|(k, t, s)| format!("{}\t{}\t{}\n", k, t.replace(['\t', '\n', '\r'], " "), s)).collect();
    let _ = std::fs::write(dir.join(FILE), s);
}

/// What was taught for this program: by full path first, then by file name.
pub fn learned(dir: &Path, path: &str, name: &str) -> Option<String> {
    let rows = load(dir);
    let (p, n) = (path.to_ascii_lowercase(), name.to_ascii_lowercase());
    rows.iter().find(|r| r.2 != "unknown" && !p.is_empty() && r.0 == p)
        .or_else(|| rows.iter().find(|r| r.2 != "unknown" && r.0 == n))
        .map(|r| r.1.clone())
}

/// Teach snifrig what a program is. `key` is a file name ("dpm.exe") or a full path.
pub fn teach(dir: &Path, key: &str, text: &str, source: &str) {
    let k = key.trim().to_ascii_lowercase();
    let mut rows: Vec<_> = load(dir).into_iter().filter(|r| r.0 != k && !(r.2 == "unknown" && r.0.ends_with(&format!("\\{}", k)))).collect();
    rows.push((k, text.trim().to_string(), source.to_string()));
    save(dir, &rows);
}

/// Programs snifrig asked about but could not describe: (key, where it runs from).
pub fn unknown(dir: &Path) -> Vec<(String, String)> {
    load(dir).into_iter().filter(|r| r.2 == "unknown").map(|r| (r.0, r.1)).collect()
}

fn note_unknown(dir: &Path, name: &str, path: &str) {
    let k = name.to_ascii_lowercase();
    let mut rows = load(dir);
    if rows.iter().any(|r| r.0 == k || (!path.is_empty() && r.0 == path.to_ascii_lowercase())) { return; }
    rows.push((k, path.to_string(), "unknown".into()));
    save(dir, &rows);
}

/// The card text for a running process. Records programs it cannot describe.
pub fn describe(dir: &Path, pid: u32, name: &str, service: Option<&str>) -> String {
    let f = facts(pid, service);
    let learned = learned(dir, &f.path, name);
    if learned.is_none() && known(name).is_none() && !has_label(name, &f) && !is_windows_path(&f.path) { note_unknown(dir, name, &f.path); }
    sentence(name, &f, learned.as_deref())
}

#[cfg(windows)]
pub fn facts(pid: u32, service: Option<&str>) -> Facts {
    let path = crate::proc::image_path(pid).unwrap_or_default();
    let v = |k: &str| win::version_string(&path, k).unwrap_or_default();
    let parent = win::parent(pid);
    let svc = service.map(str::to_string).or_else(|| win::service_of(pid)).or_else(|| parent.as_ref().and_then(|(pp, _)| win::service_of(*pp)));
    Facts {
        description: v("FileDescription"),
        product: v("ProductName"),
        company: v("CompanyName"),
        service: svc.map(|s| win::service_display(&s).unwrap_or(s)),
        parent: parent.map(|(_, n)| n),
        cmdline: crate::sys::cmdline(pid),
        path,
    }
}

#[cfg(not(windows))]
pub fn facts(pid: u32, _service: Option<&str>) -> Facts {
    let path = std::fs::read_link(format!("/proc/{}/exe", pid)).map(|p| p.display().to_string()).unwrap_or_default();
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).unwrap_or_default();
    let ppid = stat.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().nth(1)).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let parent = std::fs::read_to_string(format!("/proc/{}/comm", ppid)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let service = std::fs::read_to_string(format!("/proc/{}/cgroup", pid)).unwrap_or_default()
        .split(['/', '\n']).find(|s| s.ends_with(".service")).map(|s| s.trim_end_matches(".service").to_string());
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", pid)).map(|b| String::from_utf8_lossy(&b).replace('\0', " ")).unwrap_or_default();
    Facts { path, service, parent, cmdline, ..Default::default() }
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    fn w(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }

    /// A string from the file's version info ("FileDescription", "CompanyName", ...).
    pub fn version_string(path: &str, key: &str) -> Option<String> {
        if path.is_empty() { return None; }
        let p = w(path);
        let size = unsafe { GetFileVersionInfoSizeW(p.as_ptr(), std::ptr::null_mut()) };
        if size == 0 { return None; }
        let mut buf = vec![0u8; size as usize];
        if unsafe { GetFileVersionInfoW(p.as_ptr(), 0, size, buf.as_mut_ptr() as _) } == 0 { return None; }
        let query = |q: &str| -> Option<(*const u8, u32)> {
            let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
            (unsafe { VerQueryValueW(buf.as_ptr() as _, w(q).as_ptr(), &mut ptr, &mut len) } != 0 && len > 0).then_some((ptr as *const u8, len))
        };
        let mut langs = vec!["040904b0".to_string(), "040904e4".into(), "000004b0".into()];
        if let Some((ptr, len)) = query("\\VarFileInfo\\Translation") {
            if len >= 4 { let t = unsafe { std::slice::from_raw_parts(ptr as *const u16, 2) }; langs.insert(0, format!("{:04x}{:04x}", t[0], t[1])); }
        }
        for l in langs {
            if let Some((ptr, len)) = query(&format!("\\StringFileInfo\\{}\\{}", l, key)) {
                let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr as *const u16, len as usize) });
                let s = s.trim_end_matches('\0').trim().to_string();
                if !s.is_empty() { return Some(s); }
            }
        }
        None
    }

    /// (parent pid, parent file name).
    pub fn parent(pid: u32) -> Option<(u32, String)> {
        let mut want = None;
        let mut names = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() || snap as isize == -1 { return None; }
            let mut pe: PROCESSENTRY32W = std::mem::zeroed();
            pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut pe);
            while ok != 0 {
                let n = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(pe.szExeFile.len());
                names.push((pe.th32ProcessID, String::from_utf16_lossy(&pe.szExeFile[..n])));
                if pe.th32ProcessID == pid { want = Some(pe.th32ParentProcessID); }
                ok = Process32NextW(snap, &mut pe);
            }
            CloseHandle(snap);
        }
        let pp = want?;
        names.into_iter().find(|(p, _)| *p == pp && pp != 0)
    }

    /// The one service a process hosts.
    pub fn service_of(pid: u32) -> Option<String> { crate::proc::service_of(pid) }

    /// "Dell Peripheral Manager Service" for "DPMService"; None when unreadable or a resource string.
    pub fn service_display(name: &str) -> Option<String> {
        let key = w(&format!("SYSTEM\\CurrentControlSet\\Services\\{}", name));
        let val = w("DisplayName");
        let mut buf = [0u16; 512];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as _, &mut len) };
        if r != 0 { return None; }
        let s = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
        (!s.is_empty() && !s.starts_with('@')).then_some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dpm() -> Facts {
        Facts { path: r"C:\Program Files\Dell\Dell Peripheral Manager\DPM.exe".into(), description: "Dell Peripheral Manager".into(),
            product: "Dell Peripheral Manager".into(), company: "Dell Inc.".into(), service: Some("Dell Peripheral Manager Service".into()), parent: Some("DPMService.exe".into()), cmdline: String::new() }
    }

    #[test]
    fn says_what_it_is_and_who_starts_it() {
        assert_eq!(sentence("DPM.exe", &dpm(), None),
            "DPM.exe is Dell Peripheral Manager by Dell Inc. It runs as part of the Windows service \"Dell Peripheral Manager Service\", so it starts again after it is closed.");
        let no_svc = Facts { service: None, ..dpm() };
        assert!(sentence("DPM.exe", &no_svc, None).ends_with("DPMService.exe started it."));
    }

    #[test]
    fn taught_text_wins_and_unknowns_say_so() {
        assert!(sentence("DPM.exe", &dpm(), Some("Dell's settings app for mice and keyboards")).starts_with("DPM.exe is Dell's settings app for mice and keyboards."));
        // Windows hides svchost's file details; snifrig knows it anyway.
        assert!(sentence("svchost.exe", &Facts::default(), None).starts_with("svchost.exe is Windows' service host"));
        // For python and friends, the script is what matters.
        let py = Facts { cmdline: r#""C:\Python314\python.exe" X:\NATV\train.py --steps 900"#.into(), ..Default::default() };
        assert_eq!(sentence("python.exe", &py, None), r"python.exe is Python, which runs Python scripts. It is running: X:\NATV\train.py --steps 900.");
        let bare = Facts { path: r"X:\tools\zz.exe".into(), description: "zz".into(), ..Default::default() };
        assert_eq!(sentence("zz.exe", &bare, None), r"snifrig does not know what zz.exe is yet. It runs from X:\tools.");
        let win = Facts { path: r"C:\Windows\System32\fontdrvhost.exe".into(), ..Default::default() };
        assert_eq!(sentence("fontdrvhost.exe", &win, None), "fontdrvhost.exe is part of Windows.");
    }

    #[test]
    fn growth_from_the_alert() {
        let m = "process handles ph:DPM.exe#20132 growing 1506 handles/h (now 10313, 60 samples, 100% steps up).";
        assert_eq!(growth(m).as_deref(), Some("growing 1506 handles/h, now 10313"));
        assert_eq!(growth("nothing here"), None);
    }

    #[test]
    fn learns_once_and_lists_unknowns() {
        let d = std::env::temp_dir().join(format!("snifrig-whois-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        note_unknown(&d, "zz.exe", r"X:\tools\zz.exe");
        assert_eq!(unknown(&d).len(), 1);
        teach(&d, "ZZ.exe", "a test tool", "user");
        assert!(unknown(&d).is_empty());
        assert_eq!(learned(&d, r"X:\other\zz.exe", "zz.exe").as_deref(), Some("a test tool"));
        let _ = std::fs::remove_dir_all(&d);
    }
}

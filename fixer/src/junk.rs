//! Junk scan and cleanup. The scan is free (the monitor): what takes space for nothing, by
//! category, with sizes and how safe it is to remove. The cleanup is snifrig fix: it moves files
//! into a quarantine on the same drive (instant, nothing is copied), keeps them 7 days so any
//! cleanup can be undone, then empties the quarantine. Files in use are skipped. The Recycle Bin
//! is the one thing that cannot be undone, and says so.
//!
//! Shared source: compiled into the monitor (scan) and the fixer (scan + clean).

use std::path::{Path, PathBuf};

pub const REPORT: &str = "junk-report.json";
pub const BATCHES: &str = "quarantine-batches.txt";
pub const KEEP_DAYS: f64 = 7.0;
const DAY: f64 = 86400.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Safety { Safe, Rebuilds, Review, Admin, NoUndo }

impl Safety {
    pub fn word(&self) -> &'static str { match self { Safety::Safe => "safe", Safety::Rebuilds => "rebuilds", Safety::Review => "review", Safety::Admin => "admin", Safety::NoUndo => "no-undo" } }
}

/// One kind of junk: where it lives and which files count (older than `min_age` days).
#[derive(Clone, Debug)]
pub struct Cat { pub id: String, pub name: String, pub note: String, pub safety: Safety, pub roots: Vec<PathBuf>, pub min_age: f64, pub exts: Vec<&'static str> }

#[derive(Clone, Debug, Default)]
pub struct Found { pub bytes: u64, pub files: u64, pub denied: bool }

fn env(k: &str) -> PathBuf { PathBuf::from(std::env::var(k).unwrap_or_default()) }

fn now() -> f64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }

fn age_days(m: &std::fs::Metadata, now: f64) -> f64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0.0, |d| (now - d.as_secs_f64()) / DAY)
}

#[cfg(windows)]
fn is_reparse(m: &std::fs::Metadata) -> bool { use std::os::windows::fs::MetadataExt; m.file_attributes() & 0x400 != 0 }
#[cfg(not(windows))]
fn is_reparse(m: &std::fs::Metadata) -> bool { m.file_type().is_symlink() }

/// Every file under `root` (no following of links/junctions), with its size and age in days.
pub fn walk(root: &Path, mut f: impl FnMut(&Path, u64, f64)) -> bool {
    let t = now();
    let mut denied = false;
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0u64;
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) { Ok(r) => r, Err(e) => { if e.kind() == std::io::ErrorKind::PermissionDenied { denied = true; } continue; } };
        for e in rd.flatten() {
            seen += 1;
            if seen > 3_000_000 { return denied; }
            let Ok(m) = e.metadata() else { continue };
            if is_reparse(&m) { continue; }
            if m.is_dir() { stack.push(e.path()); } else { f(&e.path(), m.len(), age_days(&m, t)); }
        }
    }
    denied
}

/// The browser profiles' cache folders (Chrome, Edge, Brave, Comet, Opera, Vivaldi, Firefox).
fn browser_caches() -> Vec<PathBuf> {
    let la = env("LOCALAPPDATA");
    let mut out = Vec::new();
    for base in ["Google\\Chrome\\User Data", "Microsoft\\Edge\\User Data", "BraveSoftware\\Brave-Browser\\User Data", "Perplexity\\Comet\\User Data", "Vivaldi\\User Data", "Chromium\\User Data"] {
        let b = la.join(base);
        if let Ok(rd) = std::fs::read_dir(&b) {
            for p in rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
                for c in ["Cache", "Code Cache", "GPUCache", "Service Worker\\CacheStorage"] { let x = p.join(c); if x.is_dir() { out.push(x); } }
            }
        }
    }
    for x in [env("APPDATA").join("Opera Software\\Opera Stable\\Cache"), la.join("Opera Software\\Opera Stable\\Cache")] { if x.is_dir() { out.push(x); } }
    if let Ok(rd) = std::fs::read_dir(la.join("Mozilla\\Firefox\\Profiles")) {
        for p in rd.flatten().map(|e| e.path()) { let x = p.join("cache2"); if x.is_dir() { out.push(x); } }
    }
    out
}

/// All the categories snifrig knows, for this user.
pub fn categories() -> Vec<Cat> {
    let (la, up, win) = (env("LOCALAPPDATA"), env("USERPROFILE"), env("SystemRoot"));
    let c = |id: &str, name: &str, note: &str, safety: Safety, roots: Vec<PathBuf>, min_age: f64| Cat { id: id.into(), name: name.into(), note: note.into(), safety, roots, min_age, exts: Vec::new() };
    let mut v = vec![
        c("temp", "Temporary files", "Files apps left in your temp folder more than a day ago.", Safety::Safe, vec![env("TEMP")], 1.0),
        c("crash", "Crash dumps and error reports", "Copies of memory from apps that crashed, and Windows error reports already sent.", Safety::Safe,
            vec![la.join("CrashDumps"), la.join("Microsoft\\Windows\\WER\\ReportArchive"), la.join("Microsoft\\Windows\\WER\\ReportQueue")], 0.0),
        c("thumbs", "Thumbnail cache", "Small previews of your pictures and videos; Windows makes them again when needed.", Safety::Rebuilds, vec![la.join("Microsoft\\Windows\\Explorer")], 0.0),
        c("browser", "Browser caches", "Copies of web pages and images your browsers keep; sites load a little slower once, then it fills again. Close a browser for its whole cache to clear.", Safety::Rebuilds, browser_caches(), 0.0),
        c("shader", "Graphics shader caches", "Compiled graphics for games and apps; the first launch of a game is slower once while they are rebuilt.", Safety::Rebuilds,
            vec![la.join("NVIDIA\\DXCache"), la.join("NVIDIA\\GLCache"), la.join("D3DSCache"), la.join("AMD\\DxCache"), la.join("AMD\\GLCache"), la.join("Intel\\ShaderCache")], 0.0),
        c("devcache", "Developer download caches", "Copies of packages pip, npm, yarn, uv and cargo downloaded once; they download again if a project needs them.", Safety::Rebuilds,
            vec![la.join("pip\\cache"), la.join("npm-cache"), la.join("Yarn\\Cache"), la.join("uv\\cache"), la.join("pnpm\\store"), up.join(".cargo\\registry\\cache"), up.join(".cargo\\git\\db"), la.join("NuGet\\v3-cache"), up.join(".gradle\\caches")], 0.0),
        c("installers", "Old installers in Downloads", "Setup files you downloaded more than 30 days ago. Check you do not need them again.", Safety::Review, vec![up.join("Downloads")], 30.0),
        c("wintemp", "Windows temporary files", "Windows' own temp folder; needs administrator rights to clear.", Safety::Admin, vec![win.join("Temp")], 1.0),
        c("winupdate", "Windows Update downloads", "Updates Windows already installed; needs administrator rights to clear.", Safety::Admin, vec![win.join("SoftwareDistribution\\Download")], 3.0),
        c("windowsold", "Previous Windows installation", "Windows.old, kept after an upgrade so you can go back; needs administrator rights.", Safety::Admin, vec![PathBuf::from(format!("{}\\", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()))).join("Windows.old")], 0.0),
    ];
    for x in v.iter_mut().filter(|x| x.id == "thumbs") { x.exts = vec!["db"]; }
    for x in v.iter_mut().filter(|x| x.id == "installers") { x.exts = vec!["exe", "msi", "msix", "msixbundle"]; }
    v
}

fn matches(c: &Cat, p: &Path, age: f64) -> bool {
    if age < c.min_age { return false; }
    if c.exts.is_empty() { return true; }
    let e = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if c.id == "thumbs" { return p.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.to_lowercase().starts_with("thumbcache_") || n.to_lowercase().starts_with("iconcache_")); }
    c.exts.contains(&e.as_str())
}

pub fn measure(c: &Cat) -> Found {
    let mut f = Found::default();
    for r in &c.roots {
        if !r.is_dir() { continue; }
        // Installers: only the top of Downloads, not everything below it.
        if c.id == "installers" {
            if let Ok(rd) = std::fs::read_dir(r) { for e in rd.flatten() { if let Ok(m) = e.metadata() { if m.is_file() && matches(c, &e.path(), age_days(&m, now())) { f.bytes += m.len(); f.files += 1; } } } }
            continue;
        }
        f.denied |= walk(r, |p, len, age| if matches(c, p, age) { f.bytes += len; f.files += 1; });
    }
    f
}

/// A forgotten build folder: Rust `target`, `node_modules`, a Python virtual environment, not
/// touched for 30+ days. Found under your profile and on your other drives, a few levels deep.
#[derive(Clone, Debug, PartialEq)]
pub struct Build { pub path: PathBuf, pub kind: &'static str, pub age_days: f64 }

/// Days since anything in this folder, or two levels into it, changed. A folder's own date only
/// changes when its top-level entries do, so a build that writes deeper would look old.
fn newest_inside(p: &Path, now: f64) -> f64 {
    let mut newest = std::fs::metadata(p).map(|m| age_days(&m, now)).unwrap_or(f64::MAX);
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten().take(400) {
            if let Ok(m) = e.metadata() {
                newest = newest.min(age_days(&m, now));
                if m.is_dir() && !is_reparse(&m) {
                    if let Ok(r2) = std::fs::read_dir(e.path()) { for x in r2.flatten().take(400) { if let Ok(m2) = x.metadata() { newest = newest.min(age_days(&m2, now)); } } }
                }
            }
        }
    }
    newest
}

/// Full paths of every running program (lowercase): a build folder or Python environment a
/// running program lives in is in use, and never offered for cleanup.
#[cfg(not(windows))]
pub fn running_paths() -> Vec<String> {
    std::fs::read_dir("/proc").map(|d| d.flatten().filter_map(|e| std::fs::read_link(e.path().join("exe")).ok()).map(|p| p.display().to_string().to_lowercase()).collect()).unwrap_or_default()
}

#[cfg(windows)]
pub fn running_paths() -> Vec<String> {
    #[link(name = "psapi", kind = "raw-dylib")]
    extern "system" { fn EnumProcesses(ids: *mut u32, cb: u32, needed: *mut u32) -> i32; }
    #[link(name = "kernel32", kind = "raw-dylib")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn QueryFullProcessImageNameW(h: isize, flags: u32, buf: *mut u16, len: *mut u32) -> i32;
        fn CloseHandle(h: isize) -> i32;
    }
    let mut ids = vec![0u32; 8192];
    let mut needed = 0u32;
    if unsafe { EnumProcesses(ids.as_mut_ptr(), (ids.len() * 4) as u32, &mut needed) } == 0 { return Vec::new(); }
    ids.truncate(needed as usize / 4);
    let mut out = Vec::new();
    for pid in ids.into_iter().filter(|p| *p > 4) {
        let h = unsafe { OpenProcess(0x1000, 0, pid) };
        if h == 0 { continue; }
        let mut buf = [0u16; 1024];
        let mut len = 1024u32;
        if unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) } != 0 { out.push(String::from_utf16_lossy(&buf[..len as usize]).to_lowercase()); }
        unsafe { CloseHandle(h); }
    }
    out
}

pub fn find_builds() -> Vec<Build> {
    #[cfg(windows)]
    let roots = {
        let mut roots = vec![env("USERPROFILE")];
        for l in b'D'..=b'Z' { let r = PathBuf::from(format!("{}:\\", l as char)); if r.is_dir() && drive_fixed(&r) { roots.push(r); } }
        roots
    };
    #[cfg(not(windows))]
    let roots = vec![env("HOME")];
    let skip = ["windows", "program files", "program files (x86)", "programdata", "$recycle.bin", "system volume information", "appdata", ".snifrig-quarantine", "steamlibrary", "steamapps"];
    let t = now();
    let running = running_paths();
    let mut out = Vec::new();
    for root in roots {
        let mut stack = vec![(root, 0u32)];
        while let Some((d, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let Ok(m) = e.metadata() else { continue };
                if !m.is_dir() || is_reparse(&m) { continue; }
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_lowercase();
                if skip.contains(&name.as_str()) { continue; }
                let kind = if name == "target" && (p.join("CACHEDIR.TAG").exists() || p.join(".rustc_info.json").exists()) { Some("Rust build output") }
                    else if name == "node_modules" && d.join("package.json").exists() { Some("node_modules") }
                    else if (name == ".venv" || name == "venv") && p.join("pyvenv.cfg").exists() { Some("Python virtual environment") }
                    else { None };
                if let Some(k) = kind {
                    let age = newest_inside(&p, t);
                    let inside = format!("{}{}", p.display().to_string().to_lowercase(), std::path::MAIN_SEPARATOR);
                    if age >= 30.0 && !running.iter().any(|r| r.starts_with(&inside)) { out.push(Build { path: p, kind: k, age_days: age }); }
                    continue; // never look inside a build folder
                }
                if depth < 5 && !name.starts_with('.') { stack.push((p, depth + 1)); }
            }
        }
    }
    out
}

#[cfg(windows)]
fn drive_fixed(root: &Path) -> bool {
    #[link(name = "kernel32", kind = "raw-dylib")]
    extern "system" { fn GetDriveTypeW(root: *const u16) -> u32; }
    let w: Vec<u16> = root.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
    unsafe { GetDriveTypeW(w.as_ptr()) == 3 }
}

/// The desktop trash (freedesktop.org standard: GNOME, KDE, Xfce and the rest): (bytes, items).
#[cfg(not(windows))]
pub fn recycle_bin() -> (u64, u64) {
    let files = env("HOME").join(".local/share/Trash/files");
    let (mut bytes, mut items) = (0u64, 0u64);
    if let Ok(rd) = std::fs::read_dir(&files) {
        for e in rd.flatten() { items += 1; walk(&e.path(), |_, len, _| bytes += len); if let Ok(m) = e.metadata() { if m.is_file() { bytes += m.len(); } } }
    }
    (bytes, items)
}

#[cfg(windows)]
pub fn recycle_bin() -> (u64, u64) {
    #[repr(C)] struct Q { cb: u32, size: i64, items: i64 }
    #[link(name = "shell32", kind = "raw-dylib")]
    extern "system" { fn SHQueryRecycleBinW(root: *const u16, q: *mut Q) -> i32; }
    let mut q = Q { cb: std::mem::size_of::<Q>() as u32, size: 0, items: 0 };
    if unsafe { SHQueryRecycleBinW(std::ptr::null(), &mut q) } == 0 { (q.size.max(0) as u64, q.items.max(0) as u64) } else { (0, 0) }
}

fn esc(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() { match c { '"' => o.push_str("\\\""), '\\' => o.push_str("\\\\"), c if (c as u32) < 0x20 => o.push(' '), c => o.push(c) } }
    o
}

/// Scan everything and write junk-report.json. Returns the total bytes found.
pub fn scan(dir: &Path) -> u64 {
    let mut items = Vec::new();
    let mut total = 0u64;
    for c in categories() {
        let f = measure(&c);
        if f.bytes == 0 && !f.denied { continue; }
        total += f.bytes;
        items.push(format!("{{\"id\":\"{}\",\"name\":\"{}\",\"note\":\"{}\",\"safety\":\"{}\",\"bytes\":{},\"files\":{},\"denied\":{}}}", c.id, esc(&c.name), esc(&c.note), c.safety.word(), f.bytes, f.files, f.denied));
    }
    for b in find_builds() {
        let mut bytes = 0u64;
        let mut files = 0u64;
        walk(&b.path, |_, len, _| { bytes += len; files += 1; });
        if bytes < 50 << 20 { continue; }
        total += bytes;
        items.push(format!("{{\"id\":\"build:{}\",\"name\":\"{} not used for {:.0} days\",\"note\":\"{}. The project rebuilds it when you work on it again.\",\"safety\":\"rebuilds\",\"bytes\":{},\"files\":{},\"denied\":false}}",
            esc(&b.path.display().to_string()), b.kind, b.age_days, esc(&b.path.display().to_string()), bytes, files));
    }
    let (rb, rn) = recycle_bin();
    if rb > 0 {
        total += rb;
        items.push(format!("{{\"id\":\"recycle\",\"name\":\"Recycle Bin\",\"note\":\"{} items you already deleted. Emptying it cannot be undone.\",\"safety\":\"no-undo\",\"bytes\":{},\"files\":{},\"denied\":false}}", rn, rb, rn));
    }
    let body = format!("{{\"unix\":{:.0},\"total\":{},\"items\":[{}]}}\n", now(), total, items.join(","));
    let tmp = dir.join(format!("{}.tmp", REPORT));
    if std::fs::write(&tmp, body).is_ok() { let _ = std::fs::rename(&tmp, dir.join(REPORT)); }
    total
}

// ---------------- cleanup (snifrig fix) ----------------

/// Where a file goes when it is cleaned: a quarantine on the same drive, so moving is instant.
pub fn quarantine_root(dir: &Path, file: &Path) -> PathBuf {
    let drive = file.components().next().map(|c| c.as_os_str().to_string_lossy().to_uppercase()).unwrap_or_default();
    let home = dir.components().next().map(|c| c.as_os_str().to_string_lossy().to_uppercase()).unwrap_or_default();
    if drive == home { dir.join("quarantine") } else { PathBuf::from(format!("{}\\.snifrig-quarantine", drive)) }
}

/// Pure: where `file` lands inside a batch folder (its full path kept, drive letter as a folder).
pub fn landing(batch_dir: &Path, file: &Path) -> PathBuf {
    let s = file.display().to_string().replacen(':', "", 1);
    batch_dir.join(s.trim_start_matches('\\'))
}

/// Move the files of these categories (ids from the report) to quarantine. Returns
/// (files moved, bytes, files skipped because in use or protected).
pub fn clean(dir: &Path, ids: &[String]) -> (u64, u64, u64, String) { clean_with(dir, ids, true) }

fn clean_with(dir: &Path, ids: &[String], check_fresh: bool) -> (u64, u64, u64, String) {
    let batch = format!("{:.0}", now());
    let mut busy = 0u64;
    let (mut moved, mut bytes, mut skipped) = (0u64, 0u64, 0u64);
    let mut manifest = String::new();
    let mut take = |p: &Path, len: u64| {
        let root = quarantine_root(dir, p).join(&batch);
        let to = landing(&root, p);
        if let Some(parent) = to.parent() { let _ = std::fs::create_dir_all(parent); }
        if std::fs::rename(p, &to).is_ok() { moved += 1; bytes += len; manifest.push_str(&format!("{}\t{}\n", p.display(), to.display())); } else { skipped += 1; }
    };
    let cats = categories();
    for id in ids {
        if let Some(path) = id.strip_prefix("build:") {
            let p = PathBuf::from(path);
            // Checked again at cleanup time: in use now, or used since the scan? Leave it.
            let inside = format!("{}\\", p.display().to_string().to_lowercase());
            if check_fresh && (running_paths().iter().any(|r| r.starts_with(&inside)) || newest_inside(&p, now()) < 30.0) { busy += 1; continue; }
            // A whole build folder moves as one rename.
            let mut len = 0u64;
            walk(&p, |_, l, _| len += l);
            if p.is_dir() { take(&p, len); }
            continue;
        }
        let Some(c) = cats.iter().find(|c| &c.id == id) else { continue };
        if matches!(c.safety, Safety::Admin | Safety::NoUndo) { continue; }
        for r in &c.roots {
            if c.id == "installers" {
                if let Ok(rd) = std::fs::read_dir(r) { for e in rd.flatten() { if let Ok(m) = e.metadata() { if m.is_file() && matches(c, &e.path(), age_days(&m, now())) { take(&e.path(), m.len()); } } } }
                continue;
            }
            let mut files = Vec::new();
            walk(r, |p, len, age| if matches(c, p, age) { files.push((p.to_path_buf(), len)); });
            for (p, len) in files { take(&p, len); }
        }
    }
    drop(take);
    skipped += busy;
    if moved > 0 {
        use std::io::Write;
        let m = dir.join("quarantine").join(format!("{}.manifest", batch));
        let _ = std::fs::create_dir_all(dir.join("quarantine"));
        let _ = std::fs::write(&m, &manifest);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(BATCHES)) { let _ = writeln!(f, "{}\t{}\t{}\t{}", batch, moved, bytes, ids.join(",")); }
    }
    (moved, bytes, skipped, batch)
}

/// Put a cleanup back (a batch id, or "last").
pub fn restore(dir: &Path, batch: &str) -> Result<(u64, u64), String> {
    let batch = if batch == "last" {
        std::fs::read_to_string(dir.join(BATCHES)).unwrap_or_default().lines().last().and_then(|l| l.split('\t').next()).ok_or("nothing to undo")?.to_string()
    } else { batch.to_string() };
    let m = std::fs::read_to_string(dir.join("quarantine").join(format!("{}.manifest", batch))).map_err(|_| "that cleanup is gone (older than 7 days) or never happened".to_string())?;
    let (mut back, mut failed) = (0u64, 0u64);
    for l in m.lines() {
        let Some((from, to)) = l.split_once('\t') else { continue };
        let (from, to) = (Path::new(from), Path::new(to));
        if let Some(p) = from.parent() { let _ = std::fs::create_dir_all(p); }
        if !from.exists() && std::fs::rename(to, from).is_ok() { back += 1; } else { failed += 1; }
    }
    let _ = std::fs::rename(dir.join("quarantine").join(format!("{}.manifest", batch)), dir.join("quarantine").join(format!("{}.restored", batch)));
    Ok((back, failed))
}

/// Empty quarantine batches older than 7 days, for good. Returns bytes freed.
pub fn purge_old(dir: &Path) -> u64 {
    let text = std::fs::read_to_string(dir.join(BATCHES)).unwrap_or_default();
    let t = now();
    let mut keep = String::new();
    let mut freed = 0u64;
    for l in text.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        let (Some(b), Some(bytes)) = (f.first(), f.get(2).and_then(|x| x.parse::<u64>().ok())) else { continue };
        if t - b.parse::<f64>().unwrap_or(t) < KEEP_DAYS * DAY { keep.push_str(l); keep.push('\n'); continue; }
        let manifest = dir.join("quarantine").join(format!("{}.manifest", b));
        let roots: std::collections::BTreeSet<PathBuf> = std::fs::read_to_string(&manifest).unwrap_or_default().lines()
            .filter_map(|l| l.split_once('\t').map(|(_, to)| quarantine_root(dir, Path::new(to)).join(b))).collect();
        for r in roots { let _ = std::fs::remove_dir_all(&r); }
        let _ = std::fs::remove_dir_all(dir.join("quarantine").join(b));
        let _ = std::fs::remove_file(&manifest);
        freed += bytes;
    }
    let _ = std::fs::write(dir.join(BATCHES), keep);
    freed
}

/// Empty the Recycle Bin (no undo; only when you press that button).
#[cfg(not(windows))]
pub fn empty_recycle_bin() -> bool { false }

#[cfg(windows)]
pub fn empty_recycle_bin() -> bool {
    #[link(name = "shell32", kind = "raw-dylib")]
    extern "system" { fn SHEmptyRecycleBinW(h: isize, root: *const u16, flags: u32) -> i32; }
    unsafe { SHEmptyRecycleBinW(0, std::ptr::null(), 0x1 | 0x2 | 0x4) == 0 }
}

pub fn human(b: u64) -> String {
    let b = b as f64;
    if b >= 1e12 { format!("{:.1} TB", b / 1e12) } else if b >= 1e9 { format!("{:.1} GB", b / 1e9) } else if b >= 1e6 { format!("{:.0} MB", b / 1e6) } else { format!("{:.0} KB", b / 1e3) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landing_keeps_the_path() {
        let l = landing(Path::new("X:\\.snifrig-quarantine\\123"), Path::new("X:\\proj\\target"));
        assert_eq!(l, PathBuf::from("X:\\.snifrig-quarantine\\123\\X\\proj\\target"));
    }

    #[test]
    fn clean_moves_and_restore_puts_back() {
        let base = std::env::temp_dir().join(format!("snif-junk-{}", std::process::id()));
        let dir = base.join("data");
        let proj = base.join("proj");
        let tgt = proj.join("target");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::create_dir_all(tgt.join("debug"));
        std::fs::write(tgt.join("CACHEDIR.TAG"), "x").unwrap();
        std::fs::write(tgt.join("debug").join("big.bin"), vec![0u8; 1000]).unwrap();
        let (moved, bytes, skipped, _) = clean_with(&dir, &[format!("build:{}", tgt.display())], false);
        let (m2, _, s2, _) = clean(&dir, &[format!("build:{}", tgt.display())]);
        assert_eq!((m2, s2), (0, 0), "already moved: nothing left to do");
        assert_eq!((moved, skipped), (1, 0));
        assert_eq!(bytes, 1001);
        assert!(!tgt.exists(), "moved to quarantine");
        let (back, failed) = restore(&dir, "last").unwrap();
        assert_eq!((back, failed), (1, 0));
        assert!(tgt.join("debug").join("big.bin").exists(), "put back exactly");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn categories_are_sane() {
        let c = categories();
        assert!(c.iter().any(|x| x.id == "temp" && x.safety == Safety::Safe));
        assert!(c.iter().filter(|x| x.safety == Safety::Admin).count() >= 2);
        let inst = c.iter().find(|x| x.id == "installers").unwrap();
        assert!(matches(inst, Path::new("C:\\u\\Downloads\\setup.EXE"), 40.0));
        assert!(!matches(inst, Path::new("C:\\u\\Downloads\\setup.exe"), 3.0), "recent installers stay");
        assert!(!matches(inst, Path::new("C:\\u\\Downloads\\photo.jpg"), 400.0));
    }
}

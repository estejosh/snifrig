//! Disk checks for people who run AI on their PC.
//!
//! 1. Duplicate model files: the same multi-GB model kept by ComfyUI, Ollama, LM Studio, a
//!    Hugging Face cache, a downloads folder... The scan (free) groups big model files by size and
//!    a sampled fingerprint. The fix (snifrig fix) compares the files byte for byte, then keeps one
//!    copy and turns the others into hard links to it: every app still finds its file at the same
//!    place, the drive stores it once. Copies on different drives cannot be linked and are only
//!    reported. Files already linked to each other are recognised and not counted twice. In use?
//!    Skipped. Undo for 7 days (the extra copies wait in quarantine).
//! 2. Virtual disks (WSL distributions, Docker, Podman): they grow and never shrink by themselves.
//!    The scan shows how big each file is and, for running WSL distributions, how much is used
//!    inside. The fix turns on WSL's sparse mode (space comes back by itself from then on) and
//!    trims the disk, run as a snifrig job when you are away (the distribution restarts).
//!
//! Shared source: compiled into the monitor (scan) and the fixer (scan + fix).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const REPORT: &str = "disk-report.json";
pub const MIN_MODEL: u64 = 256 << 20;
const EXTS: &[&str] = &["safetensors", "ckpt", "pt", "pth", "bin", "gguf", "ggml", "onnx", "h5", "pb", "tflite", "msgpack", "model", "engine", "trt"];

fn now() -> f64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }
#[cfg(windows)]
fn is_reparse(m: &std::fs::Metadata) -> bool { use std::os::windows::fs::MetadataExt; m.file_attributes() & 0x400 != 0 }
#[cfg(not(windows))]
fn is_reparse(m: &std::fs::Metadata) -> bool { m.file_type().is_symlink() }

/// Pure: does this look like a model file worth checking?
pub fn is_model(p: &Path, len: u64) -> bool {
    if len < MIN_MODEL { return false; }
    let e = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if EXTS.contains(&e.as_str()) { return true; }
    // Ollama and Hugging Face keep models as content-named blobs without an extension.
    let s = p.display().to_string().to_lowercase();
    s.contains("\\blobs\\") && (s.contains("ollama") || s.contains("huggingface") || s.contains("lm-studio") || s.contains("lmstudio"))
}

/// Real disks' mount points (not /proc, /sys, tmpfs, network or snap squashfs mounts).
#[cfg(not(windows))]
fn fixed_drives() -> Vec<PathBuf> {
    let real = ["ext4", "ext3", "xfs", "btrfs", "f2fs", "zfs", "bcachefs", "ntfs", "ntfs3", "exfat", "vfat"];
    let mut v: Vec<PathBuf> = std::fs::read_to_string("/proc/mounts").unwrap_or_default().lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 2 && real.contains(&f[2]) && !f[1].starts_with("/boot") && !f[1].starts_with("/snap")).then(|| PathBuf::from(f[1].replace("\\040", " ")))
    }).collect();
    v.sort(); v.dedup();
    v
}

#[cfg(windows)]
fn fixed_drives() -> Vec<PathBuf> {
    #[link(name = "kernel32", kind = "raw-dylib")]
    extern "system" { fn GetDriveTypeW(root: *const u16) -> u32; }
    (b'C'..=b'Z').map(|l| PathBuf::from(format!("{}:\\", l as char))).filter(|r| {
        let w: Vec<u16> = r.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        r.is_dir() && unsafe { GetDriveTypeW(w.as_ptr()) } == 3
    }).collect()
}

/// Every big model file on your fixed drives.
pub fn find_models() -> Vec<(PathBuf, u64)> {
    let skip = ["windows", "$recycle.bin", "system volume information", ".snifrig-quarantine", "node_modules", ".git", "winsxs", "$windows.~bt", "$windows.~ws", "config.msi"];
    let mut out = Vec::new();
    for root in fixed_drives() {
        let mut stack = vec![root];
        let mut seen = 0u64;
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                seen += 1;
                if seen > 8_000_000 { break; }
                let Ok(m) = e.metadata() else { continue };
                if is_reparse(&m) { continue; }
                let p = e.path();
                if m.is_dir() {
                    let n = e.file_name().to_string_lossy().to_lowercase();
                    if !skip.contains(&n.as_str()) { stack.push(p); }
                } else if is_model(&p, m.len()) { out.push((p, m.len())); }
            }
        }
    }
    out
}

/// Inside a folder a sync app manages (MEGA, Syncthing, OneDrive, Dropbox, Google Drive,
/// iCloud)? Linking files there can confuse the sync app, so snifrig leaves them alone.
pub fn in_sync_folder(p: &Path) -> bool {
    let s = p.display().to_string().to_lowercase();
    if ["\\mega\\", "\\megasync", "\\onedrive", "\\dropbox\\", "\\google drive\\", "\\my drive\\", "\\icloud"].iter().any(|k| s.contains(k)) { return true; }
    p.ancestors().skip(1).take(10).any(|a| a.join(".stfolder").exists() || a.join(".sync").is_dir() && a.join(".sync").join("ID").exists())
}

/// Device + inode + link count on Linux: the same id means the same file on disk.
#[cfg(not(windows))]
pub fn file_id(p: &Path) -> Option<(u32, u64, u32)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some((m.dev() as u32, m.ino(), m.nlink() as u32))
}

/// Volume serial + file index + link count: the same id means the same file on disk.
#[cfg(windows)]
pub fn file_id(p: &Path) -> Option<(u32, u64, u32)> {
    #[repr(C)] struct Info { attrs: u32, c: [u32; 2], a: [u32; 2], w: [u32; 2], serial: u32, size_hi: u32, size_lo: u32, links: u32, idx_hi: u32, idx_lo: u32 }
    #[link(name = "kernel32", kind = "raw-dylib")]
    extern "system" { fn GetFileInformationByHandle(h: isize, i: *mut Info) -> i32; }
    use std::os::windows::io::AsRawHandle;
    let f = std::fs::File::open(p).ok()?;
    let mut i: Info = unsafe { std::mem::zeroed() };
    (unsafe { GetFileInformationByHandle(f.as_raw_handle() as isize, &mut i) } != 0).then(|| (i.serial, ((i.idx_hi as u64) << 32) | i.idx_lo as u64, i.links))
}

/// A cheap fingerprint: 1 MB from the start, middle and end, plus the size.
pub fn fingerprint(p: &Path, len: u64) -> Option<u64> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut h: u64 = 0xcbf29ce484222325 ^ len;
    let mut buf = vec![0u8; 1 << 20];
    for at in [0, len / 2, len.saturating_sub(1 << 20)] {
        f.seek(SeekFrom::Start(at)).ok()?;
        let n = f.read(&mut buf).ok()?;
        for b in &buf[..n] { h ^= *b as u64; h = h.wrapping_mul(0x100000001b3); }
    }
    Some(h)
}

/// SHA-256 of a whole file, streamed by sha256sum (coreutils, on every Linux). Lowercase hex.
#[cfg(not(windows))]
pub fn sha256_file(p: &Path) -> Option<String> {
    let o = std::process::Command::new("sha256sum").arg("--").arg(p).output().ok()?;
    let h = String::from_utf8_lossy(&o.stdout).split_whitespace().next()?.trim_start_matches('\\').to_lowercase();
    (o.status.success() && h.len() == 64).then_some(h)
}

/// SHA-256 of a whole file, streamed (Windows' own hashing, hardware-accelerated where the CPU
/// has it). Lowercase hex.
#[cfg(windows)]
pub fn sha256_file(p: &Path) -> Option<String> {
    #[link(name = "bcrypt", kind = "raw-dylib")]
    extern "system" {
        fn BCryptOpenAlgorithmProvider(h: *mut isize, alg: *const u16, imp: *const u16, flags: u32) -> i32;
        fn BCryptCreateHash(alg: isize, h: *mut isize, obj: *mut u8, objlen: u32, secret: *const u8, slen: u32, flags: u32) -> i32;
        fn BCryptHashData(h: isize, data: *const u8, len: u32, flags: u32) -> i32;
        fn BCryptFinishHash(h: isize, out: *mut u8, len: u32, flags: u32) -> i32;
        fn BCryptDestroyHash(h: isize) -> i32;
        fn BCryptCloseAlgorithmProvider(h: isize, flags: u32) -> i32;
    }
    let mut f = std::fs::File::open(p).ok()?;
    let alg_name: Vec<u16> = "SHA256".encode_utf16().chain(Some(0)).collect();
    let (mut alg, mut h) = (0isize, 0isize);
    unsafe {
        if BCryptOpenAlgorithmProvider(&mut alg, alg_name.as_ptr(), std::ptr::null(), 0) != 0 { return None; }
        if BCryptCreateHash(alg, &mut h, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0) != 0 { BCryptCloseAlgorithmProvider(alg, 0); return None; }
    }
    let mut buf = vec![0u8; 8 << 20];
    let mut ok = true;
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => if unsafe { BCryptHashData(h, buf.as_ptr(), n as u32, 0) } != 0 { ok = false; break; },
            Err(_) => { ok = false; break; }
        }
    }
    let mut out = [0u8; 32];
    unsafe {
        if ok && BCryptFinishHash(h, out.as_mut_ptr(), 32, 0) != 0 { ok = false; }
        BCryptDestroyHash(h);
        BCryptCloseAlgorithmProvider(alg, 0);
    }
    ok.then(|| out.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Pure: the checksum a file's own name promises, if any. Ollama names its blobs
/// sha256-<hash>; Hugging Face names large-file blobs by their SHA-256.
pub fn expected_sha(p: &Path) -> Option<String> {
    let n = p.file_name()?.to_str()?.to_lowercase();
    let hex = |s: &str| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit());
    if let Some(h) = n.strip_prefix("sha256-").or_else(|| n.strip_prefix("sha256:")) { if hex(h) { return Some(h.to_string()); } }
    let s = p.display().to_string().to_lowercase();
    (s.contains("\\blobs\\") && hex(&n)).then_some(n)
}

/// What checking the copies of one model found.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// All the same: keep this one (it matches its own checksum, or is the oldest).
    Same { keep: PathBuf, others: Vec<PathBuf> },
    /// One copy is proven right by its checksum; these others are different inside (corrupt or
    /// another version). Snifrig asks before replacing them.
    Proven { keep: PathBuf, same: Vec<PathBuf>, differ: Vec<(PathBuf, String)> },
    /// They differ inside and nothing proves which is right: snifrig asks, changes nothing.
    Unclear(Vec<(PathBuf, String)>),
}

/// Pure: decide from (file, its SHA-256, the checksum its name promises), oldest first.
pub fn verdict(copies: &[(PathBuf, String, Option<String>)]) -> Option<Verdict> {
    if copies.len() < 2 { return None; }
    // A copy whose content matches the checksum in its own name is the right one.
    let proven = copies.iter().find(|(_, h, e)| e.as_deref() == Some(h.as_str()));
    let all_same = copies.iter().all(|(_, h, _)| *h == copies[0].1);
    // A copy that fails its own checksum is never the one kept.
    let broken = |c: &&(PathBuf, String, Option<String>)| c.2.as_deref().map_or(false, |e| e != c.1);
    if all_same {
        let keep = proven.or_else(|| copies.iter().find(|c| !broken(c))).map(|c| c.0.clone())?;
        return Some(Verdict::Same { others: copies.iter().filter(|c| c.0 != keep).map(|c| c.0.clone()).collect(), keep });
    }
    match proven {
        Some(p) => Some(Verdict::Proven { keep: p.0.clone(), same: copies.iter().filter(|c| c.0 != p.0 && c.1 == p.1).map(|c| c.0.clone()).collect(),
            differ: copies.iter().filter(|c| c.1 != p.1).map(|c| (c.0.clone(), c.1.clone())).collect() }),
        None => Some(Verdict::Unclear(copies.iter().map(|c| (c.0.clone(), c.1.clone())).collect())),
    }
}

/// Pure: two files are the same, byte for byte.
pub fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(mut x), Ok(mut y)) = (std::fs::File::open(a), std::fs::File::open(b)) else { return false };
    let (mut p, mut q) = (vec![0u8; 4 << 20], vec![0u8; 4 << 20]);
    loop {
        let n = match x.read(&mut p) { Ok(n) => n, Err(_) => return false };
        let mut m = 0;
        while m < n { match y.read(&mut q[m..n]) { Ok(0) => return false, Ok(k) => m += k, Err(_) => return false } }
        if n == 0 { return y.read(&mut q[..1]).map_or(false, |k| k == 0); }
        if p[..n] != q[..n] { return false; }
    }
}

#[derive(Clone, Debug)]
pub struct Group { pub size: u64, pub files: Vec<PathBuf>, pub reclaim: u64, pub cross_drive: bool }

#[cfg(windows)]
fn drive(p: &Path) -> String { p.components().next().map(|c| c.as_os_str().to_string_lossy().to_uppercase()).unwrap_or_default() }
/// Linux: the filesystem a file lives on (hard links only work within one).
#[cfg(not(windows))]
fn drive(p: &Path) -> String { use std::os::unix::fs::MetadataExt; std::fs::metadata(p).map(|m| m.dev().to_string()).unwrap_or_default() }

/// Group model files that are likely the same (size + fingerprint), counting hard-linked
/// copies once. `reclaim` is what linking same-drive copies would free.
pub fn duplicate_groups(models: &[(PathBuf, u64)]) -> Vec<Group> {
    let mut by_size: HashMap<u64, Vec<&PathBuf>> = HashMap::new();
    for (p, l) in models { by_size.entry(*l).or_default().push(p); }
    let mut out = Vec::new();
    for (size, ps) in by_size.into_iter().filter(|(_, v)| v.len() >= 2) {
        let mut by_fp: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        for p in ps { if let Some(fp) = fingerprint(p, size) { by_fp.entry(fp).or_default().push(p.clone()); } }
        for (_, files) in by_fp.into_iter().filter(|(_, v)| v.len() >= 2) {
            // Already the same file (linked)? Count each physical file once per drive.
            let mut physical: HashMap<(u32, u64), PathBuf> = HashMap::new();
            for f in &files { if let Some((s, i, _)) = file_id(f) { physical.entry((s, i)).or_insert_with(|| f.clone()); } }
            if physical.len() < 2 { continue; }
            let mut per_drive: HashMap<String, u64> = HashMap::new();
            for p in physical.values().filter(|p| !in_sync_folder(p)) { *per_drive.entry(drive(p)).or_default() += 1; }
            let reclaim = per_drive.values().map(|n| n.saturating_sub(1) * size).sum();
            out.push(Group { size, files, reclaim, cross_drive: per_drive.len() > 1 });
        }
    }
    out.sort_by(|a, b| b.reclaim.cmp(&a.reclaim));
    out
}

#[derive(Clone, Debug)]
pub struct VDisk { pub name: String, pub path: PathBuf, pub bytes: u64, pub used: Option<u64>, pub wsl: Option<String> }

/// WSL distributions (from the registry), Docker and Podman disks.
pub fn virtual_disks() -> Vec<VDisk> {
    use crate::os_ext::CommandExt;
    let mut out = Vec::new();
    let reg = std::process::Command::new("reg.exe").args(["query", "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Lxss", "/s"]).creation_flags(0x0800_0000).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let (mut name, mut base) = (String::new(), String::new());
    let push = |name: &str, base: &str, out: &mut Vec<VDisk>| {
        if name.is_empty() || base.is_empty() { return; }
        let p = PathBuf::from(base.trim_start_matches("\\\\?\\")).join("ext4.vhdx");
        if let Ok(m) = std::fs::metadata(&p) { out.push(VDisk { name: format!("WSL: {}", name), path: p, bytes: m.len(), used: None, wsl: Some(name.to_string()) }); }
    };
    for l in reg.lines() {
        let t = l.trim();
        if t.starts_with("HKEY_") { push(&name, &base, &mut out); name.clear(); base.clear(); continue; }
        let f: Vec<&str> = t.splitn(3, "    ").collect();
        if f.len() == 3 && f[0] == "DistributionName" { name = f[2].trim().to_string(); }
        if f.len() == 3 && f[0] == "BasePath" { base = f[2].trim().to_string(); }
    }
    push(&name, &base, &mut out);
    let la = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default());
    let up = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default());
    for (n, p) in [("Docker data", la.join("Docker\\wsl\\disk\\docker_data.vhdx")), ("Docker data", la.join("Docker\\wsl\\data\\ext4.vhdx")), ("Docker", la.join("Docker\\wsl\\distro\\ext4.vhdx"))] {
        if out.iter().any(|v| v.path == p) { continue; }
        if let Ok(m) = std::fs::metadata(&p) { out.push(VDisk { name: n.into(), path: p, bytes: m.len(), used: None, wsl: None }); }
    }
    if let Ok(rd) = std::fs::read_dir(up.join(".local\\share\\containers\\podman\\machine\\wsl\\wsldist")) {
        for e in rd.flatten() { let p = e.path().join("ext4.vhdx"); if let Ok(m) = std::fs::metadata(&p) { out.push(VDisk { name: format!("Podman: {}", e.file_name().to_string_lossy()), path: p, bytes: m.len(), used: None, wsl: None }); } }
    }
    // Used space inside: only for distributions already running (never start one to look).
    let running = std::process::Command::new("wsl.exe").args(["-l", "--running", "-q"]).creation_flags(0x0800_0000).output()
        .map(|o| String::from_utf16_lossy(&o.stdout.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect::<Vec<u16>>()).lines().map(|l| l.trim().trim_matches('\0').to_string()).filter(|l| !l.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();
    for v in out.iter_mut() {
        let Some(d) = v.wsl.clone() else { continue };
        if !running.iter().any(|r| r.eq_ignore_ascii_case(&d)) { continue; }
        let o = std::process::Command::new("wsl.exe").args(["-d", &d, "-e", "df", "-B1", "--output=used", "/"]).creation_flags(0x0800_0000).output();
        v.used = o.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).lines().nth(1).and_then(|x| x.trim().parse().ok()));
    }
    out
}

fn esc(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() { match c { '"' => o.push_str("\\\""), '\\' => o.push_str("\\\\"), c if (c as u32) < 0x20 => o.push(' '), c => o.push(c) } }
    o
}

/// Scan and write disk-report.json. Returns (bytes reclaimable from duplicates, from virtual disks).
pub fn scan(dir: &Path) -> (u64, u64) {
    let models = find_models();
    let groups = duplicate_groups(&models);
    let vds = virtual_disks();
    let g: Vec<String> = groups.iter().map(|g| format!("{{\"size\":{},\"reclaim\":{},\"cross_drive\":{},\"files\":[{}]}}", g.size, g.reclaim, g.cross_drive,
        g.files.iter().map(|f| format!("\"{}\"", esc(&f.display().to_string()))).collect::<Vec<_>>().join(","))).collect();
    let v: Vec<String> = vds.iter().map(|v| format!("{{\"name\":\"{}\",\"path\":\"{}\",\"bytes\":{},\"used\":{},\"wsl\":\"{}\"}}", esc(&v.name), esc(&v.path.display().to_string()), v.bytes,
        v.used.map_or("null".to_string(), |u| u.to_string()), esc(v.wsl.as_deref().unwrap_or("")))).collect();
    let dup: u64 = groups.iter().map(|g| g.reclaim).sum();
    let vd: u64 = vds.iter().filter_map(|v| v.used.map(|u| v.bytes.saturating_sub(u))).sum();
    let body = format!("{{\"unix\":{:.0},\"models\":{},\"model_bytes\":{},\"dup_reclaim\":{},\"vdisk_reclaim\":{},\"groups\":[{}],\"vdisks\":[{}]}}\n",
        now(), models.len(), models.iter().map(|m| m.1).sum::<u64>(), dup, vd, g.join(","), v.join(","));
    let tmp = dir.join(format!("{}.tmp", REPORT));
    if std::fs::write(&tmp, body).is_ok() { let _ = std::fs::rename(&tmp, dir.join(REPORT)); }
    (dup, vd)
}

// ---------------- the fixes (snifrig fix) ----------------

/// What a dedupe found that needs you: copies that differ inside, as (file, sha256) lists.
pub type Questions = Vec<Vec<(PathBuf, String)>>;

/// Link the copies of one model to a single file on each drive, after checking every copy's
/// full SHA-256: only copies identical to the one kept are linked, and the kept one is the copy
/// that matches its own checksum (Ollama, Hugging Face) or else the oldest; never one that fails
/// its checksum. Copies that differ are left alone and come back as questions. The replaced
/// copies wait in quarantine (undo for 7 days). Returns (linked, bytes freed, skipped, batch, questions).
pub fn dedupe_checked(dir: &Path, files: &[PathBuf]) -> (u64, u64, u64, String, Questions) {
    let mut by_drive: HashMap<String, Vec<&PathBuf>> = HashMap::new();
    for f in files.iter().filter(|f| !in_sync_folder(f) && f.is_file()) { by_drive.entry(drive(f)).or_default().push(f); }
    let (mut linked, mut freed, mut skipped) = (0u64, 0u64, 0u64);
    let mut questions = Vec::new();
    let mut batch = String::new();
    for (_, mut fs) in by_drive {
        fs.sort_by_key(|f| std::fs::metadata(f).and_then(|m| m.created()).ok());
        let copies: Vec<(PathBuf, String, Option<String>)> = fs.iter().filter_map(|f| sha256_file(f).map(|h| ((*f).clone(), h, expected_sha(f)))).collect();
        match verdict(&copies) {
            Some(Verdict::Same { keep, others }) | Some(Verdict::Proven { keep, same: others, .. }) if !others.is_empty() => {
                let mut all = vec![keep.clone()];
                all.extend(others);
                let (l, f, s, b) = link_to_first(dir, &all, &batch);
                linked += l; freed += f; skipped += s;
                if !b.is_empty() { batch = b; }
            }
            _ => {}
        }
        match verdict(&copies) {
            Some(Verdict::Proven { keep, differ, .. }) => { let mut q = vec![(keep.clone(), copies.iter().find(|c| c.0 == keep).map(|c| c.1.clone()).unwrap_or_default())]; q.extend(differ); questions.push(q); }
            Some(Verdict::Unclear(all)) => questions.push(all),
            _ => {}
        }
    }
    (linked, freed, skipped, batch, questions)
}

/// You said which copy is right: the other copies on its drive are replaced by links to it
/// (they wait in quarantine; undo for 7 days).
pub fn keep_this(dir: &Path, right: &Path, group: &[PathBuf]) -> (u64, u64, u64, String) {
    let mut all = vec![right.to_path_buf()];
    all.extend(group.iter().filter(|f| f.as_path() != right && drive(f) == drive(right) && !in_sync_folder(f) && f.is_file()).cloned());
    link_to_first(dir, &all, "")
}

/// Link files[1..] to files[0] (same drive), quarantining each replaced copy.
fn link_to_first(dir: &Path, files: &[PathBuf], batch: &str) -> (u64, u64, u64, String) {
    let batch = if batch.is_empty() { format!("{:.0}", now()) } else { batch.to_string() };
    let (mut linked, mut freed, mut skipped) = (0u64, 0u64, 0u64);
    let mut manifest = String::new();
    let Some(keep) = files.first() else { return (0, 0, 0, String::new()) };
    let kid = file_id(keep).map(|x| (x.0, x.1));
    for dup in files.iter().skip(1) {
        if file_id(dup).map(|x| (x.0, x.1)) == kid { continue; }
        let len = std::fs::metadata(dup).map(|m| m.len()).unwrap_or(0);
        let q = crate::junk::landing(&crate::junk::quarantine_root(dir, dup).join(&batch), dup);
        if let Some(p) = q.parent() { let _ = std::fs::create_dir_all(p); }
        if std::fs::rename(dup, &q).is_err() { skipped += 1; continue; }
        if std::fs::hard_link(keep, dup).is_err() { let _ = std::fs::rename(&q, dup); skipped += 1; continue; }
        linked += 1;
        freed += len;
        manifest.push_str(&format!("link\t{}\t{}\n", dup.display(), q.display()));
    }
    if linked > 0 {
        use std::io::Write;
        let _ = std::fs::create_dir_all(dir.join("quarantine"));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("quarantine").join(format!("{}.manifest", batch))) { let _ = f.write_all(manifest.as_bytes()); }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(crate::junk::BATCHES)) { let _ = writeln!(f, "{}\t{}\t{}\tmodels", batch, linked, freed); }
    }
    (linked, freed, skipped, if linked > 0 { batch } else { String::new() })
}

/// Link the copies of one model to a single file on each drive. The copies go to quarantine
/// (undo for 7 days). Returns (linked, bytes freed, skipped, batch).
pub fn dedupe(dir: &Path, files: &[PathBuf]) -> (u64, u64, u64, String) {
    let batch = format!("{:.0}", now());
    let (mut linked, mut freed, mut skipped) = (0u64, 0u64, 0u64);
    let mut manifest = String::new();
    let mut by_drive: HashMap<String, Vec<&PathBuf>> = HashMap::new();
    for f in files.iter().filter(|f| !in_sync_folder(f)) { by_drive.entry(drive(f)).or_default().push(f); }
    for (_, fs) in by_drive {
        // Keep the oldest copy as the one real file.
        let mut fs: Vec<&PathBuf> = fs.into_iter().filter(|f| f.is_file()).collect();
        fs.sort_by_key(|f| std::fs::metadata(f).and_then(|m| m.created()).ok());
        let Some(keep) = fs.first().copied() else { continue };
        let kid = file_id(keep);
        for dup in fs.iter().skip(1) {
            if file_id(dup).map(|x| (x.0, x.1)) == kid.map(|x| (x.0, x.1)) { continue; } // already one file
            let len = std::fs::metadata(dup).map(|m| m.len()).unwrap_or(0);
            if !same_bytes(keep, dup) { skipped += 1; continue; }
            let q = crate::junk::landing(&crate::junk::quarantine_root(dir, dup).join(&batch), dup);
            if let Some(p) = q.parent() { let _ = std::fs::create_dir_all(p); }
            if std::fs::rename(dup, &q).is_err() { skipped += 1; continue; } // in use
            if std::fs::hard_link(keep, dup).is_err() {
                let _ = std::fs::rename(&q, dup); // put it back as it was
                skipped += 1;
                continue;
            }
            linked += 1;
            freed += len;
            manifest.push_str(&format!("link\t{}\t{}\n", dup.display(), q.display()));
        }
    }
    if linked > 0 {
        use std::io::Write;
        let _ = std::fs::create_dir_all(dir.join("quarantine"));
        let _ = std::fs::write(dir.join("quarantine").join(format!("{}.manifest", batch)), &manifest);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(crate::junk::BATCHES)) { let _ = writeln!(f, "{}\t{}\t{}\tmodels", batch, linked, freed); }
    }
    (linked, freed, skipped, batch)
}

/// Undo a dedupe batch: each link is replaced by the copy it replaced.
pub fn undo_links(dir: &Path, batch: &str) -> Result<u64, String> {
    let m = std::fs::read_to_string(dir.join("quarantine").join(format!("{}.manifest", batch))).map_err(|_| "that change is gone (older than 7 days)".to_string())?;
    let mut back = 0;
    for l in m.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() != 3 || f[0] != "link" { continue; }
        let (dup, q) = (Path::new(f[1]), Path::new(f[2]));
        if q.exists() && std::fs::remove_file(dup).is_ok() && std::fs::rename(q, dup).is_ok() { back += 1; }
    }
    let _ = std::fs::rename(dir.join("quarantine").join(format!("{}.manifest", batch)), dir.join("quarantine").join(format!("{}.restored", batch)));
    Ok(back)
}

/// WSL: turn on sparse mode (space comes back by itself from then on) and trim now. Stops the
/// distribution first; run it as a job when you are away.
pub fn wsl_compact(distro: &str) -> Result<String, String> {
    use crate::os_ext::CommandExt;
    let run = |args: &[&str]| std::process::Command::new("wsl.exe").args(args).creation_flags(0x0800_0000).output();
    let _ = run(&["--terminate", distro]);
    let o = run(&["--manage", distro, "--set-sparse", "true"]).map_err(|e| e.to_string())?;
    let said = String::from_utf16_lossy(&o.stdout.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect::<Vec<u16>>());
    if !o.status.success() { return Err(format!("WSL could not switch {} to sparse mode: {}", distro, said.trim())); }
    let _ = run(&["-d", distro, "-u", "root", "-e", "fstrim", "-av"]);
    Ok(format!("{} now gives space back by itself (sparse), and was trimmed", distro))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_decides_which_copy_is_right() {
        let (a, c) = (PathBuf::from("X:/hone/m.gguf"), PathBuf::from("X:/dl/m.gguf"));
        let good = "a".repeat(64);
        let b = PathBuf::from(format!("X:/ollama/models/blobs/sha256-{}", good));
        assert_eq!(expected_sha(&b), Some(good.clone()));
        // all the same: the copy proven by its own name is kept even though another is older
        match verdict(&[(a.clone(), good.clone(), None), (b.clone(), good.clone(), Some(good.clone()))]).unwrap() {
            Verdict::Same { keep, others } => { assert_eq!(keep, b); assert_eq!(others, vec![a.clone()]); }
            v => panic!("{:?}", v),
        }
        // one differs, one proven: keep the proven, link the same, ask about the different one
        match verdict(&[(a.clone(), "b".repeat(64), None), (b.clone(), good.clone(), Some(good.clone())), (c.clone(), good.clone(), None)]).unwrap() {
            Verdict::Proven { keep, same, differ } => { assert_eq!(keep, b); assert_eq!(same, vec![c.clone()]); assert_eq!(differ.len(), 1); assert_eq!(differ[0].0, a); }
            v => panic!("{:?}", v),
        }
        // they differ and nothing proves either: ask, change nothing
        assert!(matches!(verdict(&[(a.clone(), "b".repeat(64), None), (c.clone(), good.clone(), None)]).unwrap(), Verdict::Unclear(_)));
        // a copy that fails its own checksum is never the one kept
        let bad = PathBuf::from(format!("X:/ollama/models/blobs/sha256-{}", "c".repeat(64)));
        match verdict(&[(bad.clone(), good.clone(), Some("c".repeat(64))), (a.clone(), good.clone(), None)]).unwrap() {
            Verdict::Same { keep, .. } => assert_eq!(keep, a),
            v => panic!("{:?}", v),
        }
    }

    #[test]
    fn real_sha256() {
        let p = std::env::temp_dir().join(format!("snif-sha-{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sync_folders_are_left_alone() {
        assert!(in_sync_folder(Path::new("X:\\Mega\\Business Folder\\m.safetensors")));
        assert!(in_sync_folder(Path::new("C:\\Users\\a\\OneDrive\\models\\m.gguf")));
        assert!(!in_sync_folder(Path::new("X:\\ollama\\models\\blobs\\sha256-1")));
    }

    #[test]
    fn model_files() {
        assert!(is_model(Path::new("X:\\c\\models\\a.safetensors"), 2 << 30));
        assert!(!is_model(Path::new("X:\\c\\models\\a.safetensors"), 10 << 20), "small files are not worth it");
        assert!(is_model(Path::new("C:\\Users\\a\\.ollama\\models\\blobs\\sha256-abc"), 4 << 30));
        assert!(!is_model(Path::new("C:\\data\\video.mp4"), 4 << 30));
    }

    #[test]
    fn duplicates_are_linked_once_and_can_be_undone() {
        let base = std::env::temp_dir().join(format!("snif-disk-{}", std::process::id()));
        let dir = base.join("data");
        let _ = std::fs::create_dir_all(&dir);
        let a = base.join("comfy").join("m.safetensors");
        let b = base.join("ollama").join("m.safetensors");
        let c = base.join("other").join("n.safetensors");
        for p in [&a, &b, &c] { let _ = std::fs::create_dir_all(p.parent().unwrap()); }
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &data).unwrap();
        std::fs::write(&b, &data).unwrap();
        let mut other = data.clone(); other[1_500_000] ^= 1;
        std::fs::write(&c, &other).unwrap();
        assert!(same_bytes(&a, &b) && !same_bytes(&a, &c));
        let (linked, freed, skipped, batch) = dedupe(&dir, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!((linked, skipped), (1, 1), "the different file is never linked");
        assert_eq!(freed, 3_000_000);
        assert_eq!(file_id(&a).map(|x| (x.0, x.1)), file_id(&b).map(|x| (x.0, x.1)), "one file on disk now");
        assert_eq!(std::fs::read(&b).unwrap(), data, "the app still finds the same bytes");
        assert_eq!(undo_links(&dir, &batch).unwrap(), 1);
        assert_ne!(file_id(&a).map(|x| (x.0, x.1)), file_id(&b).map(|x| (x.0, x.1)), "separate copies again");
        let _ = std::fs::remove_dir_all(&base);
    }
}

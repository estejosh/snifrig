//! Drive health: what each drive says about itself, how full each volume is and how fast it is
//! filling, and which drives hang off USB.
//!
//! - NVMe health log (the same numbers smartctl shows): critical warning, temperature against the
//!   drive's own warning point, spare blocks, wear ("percentage used"), data errors, unsafe
//!   shutdowns, total written. No admin needed: Windows answers IOCTL_STORAGE_QUERY_PROPERTY on
//!   \\.\PhysicalDriveN opened with no access rights. Read at start and then hourly.
//! - Free space every minute: a volume running out, or filling fast enough to run out within two
//!   hours, gets a card naming the busiest programs on disk. A full system drive leaves no room for
//!   the page file, and then apps crash and the PC can freeze.
//! - USB: a drive on USB that holds the page file or the programs you run gets a card. On 16 and
//!   19 Nov 2026 a USB enclosure dropping its link took beastly down twice.
//!
//! The judging (`judge`, `space`) is pure and tested; only `scan` and `volumes` touch Windows.

use std::collections::HashMap;
use std::path::Path;

pub const FILE: &str = "drives.txt";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Health {
    pub critical: u8,
    pub temp_c: f64,
    /// The drive's own warning and critical temperatures (0 when it does not say).
    pub warn_c: f64,
    pub crit_c: f64,
    pub spare: u8,
    pub spare_min: u8,
    pub used_pct: u8,
    pub read_tb: f64,
    pub written_tb: f64,
    pub hours: u64,
    pub cycles: u64,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
    pub warn_temp_min: u32,
    pub crit_temp_min: u32,
    /// SATA only: sectors moved to spares (permanent) and sectors waiting to be (unreadable now).
    pub realloc: u64,
    pub pending: u64,
}

/// ATA Device Statistics (log 04h), one 512-byte page each: 1 general, 3 rotating media,
/// 4 general errors, 5 temperature, 7 solid state. Each statistic is a qword; bit 63 says it is
/// supported and bit 62 that the value is valid.
pub fn parse_ata_stats(p1: Option<&[u8]>, p3: Option<&[u8]>, p4: Option<&[u8]>, p5: Option<&[u8]>, p7: Option<&[u8]>) -> Health {
    let stat = |p: Option<&[u8]>, off: usize| -> Option<u64> {
        let p = p?;
        if p.len() < off + 8 { return None; }
        let v = le(p, off, 8);
        (v >> 63 & 1 == 1 && v >> 62 & 1 == 1).then(|| v & 0x0000_FFFF_FFFF_FFFF)
    };
    let temp = |p: Option<&[u8]>, off: usize| stat(p, off).map(|v| (v as u8) as i8 as f64).unwrap_or(0.0);
    let warn = temp(p5, 0x58);
    Health {
        cycles: stat(p1, 0x08).unwrap_or(0),
        hours: stat(p1, 0x10).unwrap_or(0) & 0xFFFF_FFFF,
        written_tb: stat(p1, 0x18).unwrap_or(0) as f64 * 512.0 / 1e12,
        read_tb: stat(p1, 0x28).unwrap_or(0) as f64 * 512.0 / 1e12,
        realloc: stat(p3, 0x20).unwrap_or(0) & 0xFFFF_FFFF,
        pending: stat(p3, 0x38).unwrap_or(0) & 0xFFFF_FFFF,
        media_errors: stat(p4, 0x08).unwrap_or(0) & 0xFFFF_FFFF,
        temp_c: temp(p5, 0x08),
        warn_c: warn,
        crit_c: warn,
        used_pct: stat(p7, 0x08).unwrap_or(0).min(255) as u8,
        // ATA reports no spare-block percentage; 100 so the spare check stays quiet.
        spare: 100,
        ..Default::default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Drive {
    pub index: u32,
    pub model: String,
    pub serial: String,
    pub usb: bool,
    pub letters: Vec<char>,
    pub health: Option<Health>,
    /// How the health was read, or why it could not be ("nvme", "ata (bus 11)", "usb: ...").
    pub probe: String,
}

impl Drive {
    /// "X: (PCIe SSD)" or "disk 2 (Samsung SSD 980)".
    pub fn label(&self) -> String {
        let at = if self.letters.is_empty() { format!("disk {}", self.index) } else { self.letters.iter().map(|c| format!("{}:", c)).collect::<Vec<_>>().join(" ") };
        if self.model.is_empty() { at } else { format!("{} ({})", at, self.model) }
    }
    pub fn key(&self) -> String {
        let s: String = self.serial.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        if s.is_empty() { format!("disk{}", self.index) } else { s }
    }
}

fn le(b: &[u8], off: usize, n: usize) -> u64 {
    (0..n.min(8)).fold(0u64, |a, i| a | (*b.get(off + i).unwrap_or(&0) as u64) << (8 * i))
}

/// NVMe SMART / health log (log page 2, 512 bytes) plus, if present, the controller identify
/// data (4096 bytes; WCTEMP at 266, CCTEMP at 268, both in kelvin).
pub fn parse_health(log: &[u8], ident: Option<&[u8]>) -> Health {
    let k = |v: u64| if v > 0 { v as f64 - 273.0 } else { 0.0 };
    // Data units are thousands of 512-byte blocks: 512,000 bytes each.
    let tb = |units: u64| units as f64 * 512_000.0 / 1e12;
    Health {
        critical: le(log, 0, 1) as u8,
        temp_c: k(le(log, 1, 2)),
        spare: le(log, 3, 1) as u8,
        spare_min: le(log, 4, 1) as u8,
        used_pct: le(log, 5, 1) as u8,
        read_tb: tb(le(log, 32, 8)),
        written_tb: tb(le(log, 48, 8)),
        cycles: le(log, 112, 8),
        hours: le(log, 128, 8),
        unsafe_shutdowns: le(log, 144, 8),
        media_errors: le(log, 160, 8),
        warn_temp_min: le(log, 192, 4) as u32,
        crit_temp_min: le(log, 196, 4) as u32,
        warn_c: ident.map(|i| k(le(i, 266, 2))).unwrap_or(0.0),
        crit_c: ident.map(|i| k(le(i, 268, 2))).unwrap_or(0.0),
        realloc: 0,
        pending: 0,
    }
}

/// What was last seen per drive, to tell what changed: "key base_unix base_written last_media last_unsafe".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen { pub base_unix: f64, pub base_written: f64, pub media: u64, pub unsafe_shutdowns: u64 }

pub fn load(dir: &Path) -> HashMap<String, Seen> {
    std::fs::read_to_string(dir.join(FILE)).unwrap_or_default().lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 5 { return None; }
        Some((f[0].to_string(), Seen { base_unix: f[1].parse().ok()?, base_written: f[2].parse().ok()?, media: f[3].parse().ok()?, unsafe_shutdowns: f[4].parse().ok()? }))
    }).collect()
}

pub fn save(dir: &Path, seen: &HashMap<String, Seen>) {
    let mut keys: Vec<&String> = seen.keys().collect();
    keys.sort();
    let s: String = keys.iter().map(|k| { let v = &seen[*k]; format!("{} {:.0} {:.3} {} {}\n", k, v.base_unix, v.base_written, v.media, v.unsafe_shutdowns) }).collect();
    crate::rules::write_atomic(&dir.join(FILE), &s);
}

/// One thing worth a card. `rule` is the card's dedupe key; `ask` marks the urgent ones.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding { pub rule: String, pub msg: String, pub urgent: bool }

/// Judge one drive against what was seen before; returns findings and the updated record.
/// `uses` says what on this drive matters (page file, programs running from it), for USB drives.
pub fn judge(d: &Drive, prev: Option<&Seen>, now: f64, uses: &[String]) -> (Vec<Finding>, Option<Seen>) {
    let mut out = Vec::new();
    let name = d.label();
    let key = d.key();
    let day = (now / 86400.0).floor();
    if d.usb && !uses.is_empty() {
        out.push(Finding { rule: format!("drive:usb:{}:{:.0}", key, (now / 604800.0).floor()), urgent: false,
            msg: format!("{} is connected over USB and holds {}. If its USB link drops (a loose cable, a hot or cheap enclosure, a hub), everything using it freezes and the PC can go down with it, as on 16 and 19 Nov. Put it inside the PC (a free M.2 slot or SATA port) or move these off it.", name, uses.join(", ")) });
    }
    let Some(h) = &d.health else { return (out, None) };
    if h.critical != 0 {
        let mut why = Vec::new();
        if h.critical & 0x01 != 0 { why.push("its spare blocks are nearly used up"); }
        if h.critical & 0x02 != 0 { why.push("it is too hot"); }
        if h.critical & 0x04 != 0 { why.push("its reliability has dropped (internal errors)"); }
        if h.critical & 0x08 != 0 { why.push("it has switched itself to read-only"); }
        if h.critical & 0x10 != 0 { why.push("its power-loss protection failed"); }
        if why.is_empty() { why.push("it reports a problem"); }
        out.push(Finding { rule: format!("drive:critical:{}:{:.0}", key, day), urgent: true,
            msg: format!("{} says it is failing: {}. Back up what matters on it now and plan to replace it.", name, why.join(", ")) });
    }
    if h.warn_c > 0.0 && h.temp_c >= h.warn_c - 10.0 {
        out.push(Finding { rule: format!("drive:hot:{}:{:.0}", key, day), urgent: h.temp_c >= h.warn_c,
            msg: format!("{} is at {:.0} °C; it slows itself down at {:.0} °C and can drop out at {:.0} °C. Give it airflow: a heatsink on the drive, the case fans, or space from the graphics card.", name, h.temp_c, h.warn_c, h.crit_c) });
    }
    if h.spare < 20 || (h.spare_min > 0 && h.spare <= h.spare_min) {
        out.push(Finding { rule: format!("drive:spare:{}:{:.0}", key, day), urgent: true,
            msg: format!("{} has {}% of its spare blocks left. When they run out it turns read-only. Back it up and plan to replace it.", name, h.spare) });
    }
    if h.pending > 0 || h.realloc >= 10 {
        out.push(Finding { rule: format!("drive:sectors:{}:{:.0}", key, (now / 604800.0).floor()), urgent: h.pending > 0,
            msg: format!("{} has {} bad sector{} moved to spares and {} it cannot read right now. Sectors going bad is how hard drives and older SSDs start to fail. Back it up now; if the numbers grow, replace it.", name, h.realloc, if h.realloc == 1 { "" } else { "s" }, h.pending) });
    }
    if h.used_pct >= 90 {
        out.push(Finding { rule: format!("drive:worn:{}:{:.0}", key, (now / 604800.0).floor()), urgent: false,
            msg: format!("{} says it has used {}% of its rated life. Plan to replace it.", name, h.used_pct) });
    }
    let seen = prev.cloned().unwrap_or(Seen { base_unix: now, base_written: h.written_tb, media: h.media_errors, unsafe_shutdowns: h.unsafe_shutdowns });
    if h.media_errors > seen.media {
        out.push(Finding { rule: format!("drive:media:{}:{}", key, h.media_errors), urgent: true,
            msg: format!("{} found {} new data error{} it could not correct ({} in all). Some files on it may be damaged. Back it up now.", name, h.media_errors - seen.media, if h.media_errors - seen.media == 1 { "" } else { "s" }, h.media_errors) });
    }
    if h.unsafe_shutdowns > seen.unsafe_shutdowns {
        let n = h.unsafe_shutdowns - seen.unsafe_shutdowns;
        out.push(Finding { rule: format!("drive:unsafe:{}:{}", key, h.unsafe_shutdowns), urgent: false,
            msg: format!("{} lost power without a clean shutdown {} time{} since snifrig last looked ({} in its life): the PC crashed, froze or was switched off at the wall. Each one risks the files being written at that moment.", name, n, if n == 1 { "" } else { "s" }, h.unsafe_shutdowns) });
    }
    // Wear-out date: the drive's own rated life (written / used%) at its write rate. The rate is
    // measured since snifrig's first look once that is 3+ days old; until then the lifetime average.
    if h.used_pct >= 5 && h.written_tb > 0.0 {
        let rated = h.written_tb / h.used_pct as f64 * 100.0;
        let span = (now - seen.base_unix) / 86400.0;
        let per_day = if span >= 3.0 && h.written_tb > seen.base_written { (h.written_tb - seen.base_written) / span }
            else if h.hours > 24 { h.written_tb / (h.hours as f64 / 24.0) } else { 0.0 };
        if per_day > 0.0 {
            let days = (rated - h.written_tb).max(0.0) / per_day;
            if days < 365.0 {
                out.push(Finding { rule: format!("drive:wear:{}:{:.0}", key, (now / 604800.0).floor()), urgent: days < 90.0,
                    msg: format!("{} writes about {:.0} GB a day; at that pace it reaches its rated life in about {:.0} days ({:.0} of about {:.0} TB written). Move the heaviest writers (swap file, build output, model output) to another drive, or plan to replace it.", name, per_day * 1000.0, days, h.written_tb, rated) });
            }
        }
    }
    // Keep the first look as the base for 30 days, then start a new one.
    let base = if now - seen.base_unix > 30.0 * 86400.0 { (now, h.written_tb) } else { (seen.base_unix, seen.base_written) };
    (out, Some(Seen { base_unix: base.0, base_written: base.1, media: h.media_errors, unsafe_shutdowns: h.unsafe_shutdowns }))
}

/// Free space over time per volume, sampled each minute; keeps the last 30 minutes.
#[derive(Default)]
pub struct Space { hist: HashMap<char, Vec<(f64, f64)>> }

#[derive(Clone, Debug, PartialEq)]
pub enum SpaceStep {
    /// Under 10 GB or 5% free.
    Low { letter: char, free_gb: f64, total_gb: f64 },
    /// Will be full within two hours at the pace of the last 10+ minutes.
    Filling { letter: char, free_gb: f64, gb_per_min: f64, mins_left: f64 },
}

impl Space {
    /// `vols`: (letter, free GB, total GB) of fixed volumes.
    pub fn tick(&mut self, now: f64, vols: &[(char, f64, f64)]) -> Vec<SpaceStep> {
        let mut out = Vec::new();
        for &(l, free, total) in vols {
            let h = self.hist.entry(l).or_default();
            h.push((now, free));
            h.retain(|&(t, _)| now - t <= 1800.0);
            if total <= 0.0 { continue; }
            // Low: under 5% of the volume, at least 10 GB and at most 50 GB (and never over half).
            if free < (total * 0.05).clamp(10.0, 50.0).min(total * 0.5) { out.push(SpaceStep::Low { letter: l, free_gb: free, total_gb: total }); }
            // Pace over the window: only a steady drop counts, not one big copy that finished.
            if let Some(&(t0, f0)) = h.iter().find(|&&(t, _)| now - t >= 600.0) {
                let mins = (now - t0) / 60.0;
                let drop = f0 - free;
                let recent = h.iter().rev().find(|&&(t, _)| now - t >= 120.0).map_or(0.0, |&(_, f)| f - free);
                if drop >= 2.0 && recent > 0.0 {
                    let rate = drop / mins;
                    let left = free / rate;
                    if left < 120.0 { out.push(SpaceStep::Filling { letter: l, free_gb: free, gb_per_min: rate, mins_left: left }); }
                }
            }
        }
        out
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
    const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;
    const BUS_USB: u32 = 7;

    struct H(isize);
    impl Drop for H { fn drop(&mut self) { unsafe { CloseHandle(self.0 as _); } } }

    fn open(path: &str) -> Option<H> {
        let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let h = unsafe { CreateFileW(w.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        if h == INVALID_HANDLE_VALUE || h.is_null() { None } else { Some(H(h as isize)) }
    }

    fn ioctl(h: &H, code: u32, inb: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut n = 0u32;
        let ok = unsafe { DeviceIoControl(h.0 as _, code, inb.as_ptr() as *const c_void, inb.len() as u32, out.as_mut_ptr() as *mut c_void, out.len() as u32, &mut n, std::ptr::null_mut()) };
        if ok == 0 { None } else { Some(n as usize) }
    }

    /// STORAGE_DEVICE_DESCRIPTOR: (bus type, model, serial).
    fn describe(h: &H) -> Option<(u32, String, String)> {
        let q = [0u8; 12]; // StorageDeviceProperty (0), PropertyStandardQuery (0)
        let mut out = vec![0u8; 1024];
        let n = ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out)?;
        let s = |off: u64| -> String {
            let off = off as usize;
            if off == 0 || off >= n { return String::new(); }
            String::from_utf8_lossy(&out[off..n].split(|&c| c == 0).next().unwrap_or(&[])).trim().to_string()
        };
        Some((le(&out, 28, 4) as u32, s(le(&out, 16, 4)), s(le(&out, 24, 4))))
    }

    /// NVMe protocol query: property 49 (adapter) or 50 (device), data type 1 identify / 2 log page.
    fn nvme(h: &H, property: u32, data_type: u32, value: u32, len: usize) -> Option<Vec<u8>> { proto(h, 3, property, data_type, value, 0, len) }

    /// Protocol-specific query: protocol 2 ATA / 3 NVMe; for ATA log pages `value` is the log
    /// address and `sub` the page number.
    fn proto(h: &H, ptype: u32, property: u32, data_type: u32, value: u32, sub: u32, len: usize) -> Option<Vec<u8>> {
        let mut b = vec![0u8; 8 + 40 + len];
        b[0..4].copy_from_slice(&property.to_le_bytes());
        // STORAGE_PROTOCOL_SPECIFIC_DATA at 8: ProtocolType, DataType, RequestValue,
        // RequestSubValue, ProtocolDataOffset = 40, ProtocolDataLength.
        b[8..12].copy_from_slice(&ptype.to_le_bytes());
        b[12..16].copy_from_slice(&data_type.to_le_bytes());
        b[16..20].copy_from_slice(&value.to_le_bytes());
        b[20..24].copy_from_slice(&sub.to_le_bytes());
        b[24..28].copy_from_slice(&40u32.to_le_bytes());
        b[28..32].copy_from_slice(&(len as u32).to_le_bytes());
        let inb = b.clone();
        ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &inb, &mut b)?;
        // Output: STORAGE_PROTOCOL_DATA_DESCRIPTOR { Version, Size, ProtocolSpecificData }.
        let off = 8 + le(&b, 8 + 16, 4) as usize;
        let got = le(&b, 8 + 20, 4) as usize;
        if got == 0 || off + got.min(len) > b.len() { return None; }
        Some(b[off..off + got.min(len)].to_vec())
    }

    /// NVMe health log, or for SATA/ATA drives the Device Statistics log (04h). Returns the
    /// health and what was tried, for the field report ("nvme", "ata", "ata: no answer").
    fn health(h: &H, bus: u32) -> (Option<Health>, String) {
        if let Some(log) = nvme(h, 50, 2, 2, 512).or_else(|| nvme(h, 49, 2, 2, 512)) {
            let ident = nvme(h, 49, 1, 1, 4096).or_else(|| nvme(h, 50, 1, 1, 4096));
            return (Some(parse_health(&log, ident.as_deref())), "nvme".into());
        }
        if bus == BUS_USB { return (None, "usb: the enclosure hides the drive's health".into()); }
        // SATA (11) / ATA (3) / RAID (8): ATA Device Statistics pages 1, 3, 4, 5, 7.
        let page = |p: u32| proto(h, 2, 50, 2, 0x04, p, 512).or_else(|| proto(h, 2, 49, 2, 0x04, p, 512));
        let p1 = page(1);
        if p1.is_none() { return (None, format!("ata (bus {}): no answer without admin", bus)); }
        let hl = parse_ata_stats(p1.as_deref(), page(3).as_deref(), page(4).as_deref(), page(5).as_deref(), page(7).as_deref());
        (Some(hl), format!("ata (bus {})", bus))
    }

    fn disk_number(letter: char) -> Option<u32> {
        let h = open(&format!("\\\\.\\{}:", letter))?;
        let mut out = [0u8; 12]; // STORAGE_DEVICE_NUMBER { DeviceType, DeviceNumber, PartitionNumber }
        ioctl(&h, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out)?;
        Some(le(&out, 4, 4) as u32)
    }

    fn fixed_letters() -> Vec<char> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u32).filter(|i| mask >> i & 1 == 1).map(|i| (b'A' + i as u8) as char).filter(|c| {
            let root: Vec<u16> = format!("{}:\\", c).encode_utf16().chain(Some(0)).collect();
            unsafe { GetDriveTypeW(root.as_ptr()) == 3 }
        }).collect()
    }

    /// Every physical drive with its letters and, for NVMe, its health log.
    pub fn scan() -> Vec<Drive> {
        let mut letters: HashMap<u32, Vec<char>> = HashMap::new();
        for l in fixed_letters() { if let Some(n) = disk_number(l) { letters.entry(n).or_default().push(l); } }
        let mut out = Vec::new();
        for i in 0..16u32 {
            let Some(h) = open(&format!("\\\\.\\PhysicalDrive{}", i)) else { continue };
            let (bus, model, serial) = describe(&h).unwrap_or_default();
            let (health, probe) = health(&h, bus);
            out.push(Drive { index: i, model, serial, usb: bus == BUS_USB, letters: letters.remove(&i).unwrap_or_default(), health, probe });
        }
        out
    }

    /// (letter, free GB, total GB) of fixed volumes.
    pub fn volumes() -> Vec<(char, f64, f64)> {
        fixed_letters().into_iter().filter_map(|c| {
            let root: Vec<u16> = format!("{}:\\", c).encode_utf16().chain(Some(0)).collect();
            let (mut free, mut tot, mut tf) = (0u64, 0u64, 0u64);
            (unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut tot, &mut tf) } != 0).then(|| (c, free as f64 / 1e9, tot as f64 / 1e9))
        }).collect()
    }

    /// The PagingFiles registry entries ("C:\pagefile.sys 0 0", "?:\pagefile.sys"), readable without admin.
    /// None when the value could not be read (then nothing is judged).
    pub fn pagefile_entries() -> Option<Vec<String>> {
        let o = std::process::Command::new("reg").args(["query", r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management", "/v", "PagingFiles"])
            .stdin(std::process::Stdio::null()).creation_flags_nowindow().output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let line = o.lines().find(|l| l.contains("PagingFiles") && l.contains("REG_MULTI_SZ"))?;
        Some(line.split("REG_MULTI_SZ").nth(1).unwrap_or("").trim().split("\\0").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
    }

    /// Drive letters holding a page file.
    pub fn pagefile_letters() -> Vec<char> {
        let v = pagefile_entries().unwrap_or_default().join("\\0");
        let mut out: Vec<char> = v.split("\\0").filter_map(|p| {
            let p = p.trim();
            if p.starts_with("?:") { return std::env::var("SystemDrive").ok().and_then(|s| s.chars().next()); }
            p.chars().next().filter(|c| c.is_ascii_alphabetic() && p.get(1..2) == Some(":")).map(|c| c.to_ascii_uppercase())
        }).collect();
        out.dedup();
        out
    }

    trait NoWindow { fn creation_flags_nowindow(&mut self) -> &mut Self; }
    impl NoWindow for std::process::Command {
        fn creation_flags_nowindow(&mut self) -> &mut Self { use std::os::windows::process::CommandExt; self.creation_flags(0x0800_0000) }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn reads_this_pcs_drives() {
            let d = super::scan();
            assert!(!d.is_empty());
            for x in &d { println!("{} usb={} health={:?}", x.label(), x.usb, x.health); }
            assert!(super::volumes().iter().any(|v| v.2 > 0.0));
            println!("page file on {:?}", super::pagefile_letters());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(temp_k: u16, spare: u8, used: u8, written_units: u64, hours: u64, unsafe_n: u64, media: u64) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[1..3].copy_from_slice(&temp_k.to_le_bytes());
        b[3] = spare; b[4] = 10; b[5] = used;
        b[48..56].copy_from_slice(&written_units.to_le_bytes());
        b[128..136].copy_from_slice(&hours.to_le_bytes());
        b[144..152].copy_from_slice(&unsafe_n.to_le_bytes());
        b[160..168].copy_from_slice(&media.to_le_bytes());
        b
    }
    fn ident(warn_k: u16, crit_k: u16) -> Vec<u8> { let mut b = vec![0u8; 4096]; b[266..268].copy_from_slice(&warn_k.to_le_bytes()); b[268..270].copy_from_slice(&crit_k.to_le_bytes()); b }
    fn drive(h: Health) -> Drive { Drive { index: 0, model: "Test".into(), serial: "ABC".into(), usb: false, letters: vec!['C'], health: Some(h), probe: "nvme".into() } }

    #[test]
    fn sata_device_statistics() {
        let q = |p: &mut Vec<u8>, off: usize, v: u64| p[off..off + 8].copy_from_slice(&((3u64 << 62) | v).to_le_bytes());
        let (mut p1, mut p3, mut p4, mut p5, mut p7) = (vec![0u8; 512], vec![0u8; 512], vec![0u8; 512], vec![0u8; 512], vec![0u8; 512]);
        q(&mut p1, 0x08, 1200); q(&mut p1, 0x10, 30_000); q(&mut p1, 0x18, 100_000_000_000); // 51.2 TB written
        q(&mut p3, 0x20, 24); q(&mut p3, 0x38, 3);
        q(&mut p4, 0x08, 0);
        q(&mut p5, 0x08, 41); q(&mut p5, 0x58, 60);
        q(&mut p7, 0x08, 12);
        // A statistic not flagged valid is ignored.
        p1[0x28..0x30].copy_from_slice(&5u64.to_le_bytes());
        let h = parse_ata_stats(Some(&p1), Some(&p3), Some(&p4), Some(&p5), Some(&p7));
        assert_eq!((h.cycles, h.hours, h.realloc, h.pending, h.used_pct, h.spare), (1200, 30_000, 24, 3, 12, 100));
        assert!((h.written_tb - 51.2).abs() < 0.01 && h.read_tb == 0.0);
        assert_eq!((h.temp_c, h.warn_c), (41.0, 60.0));
        let (f, _) = judge(&drive(h), None, 1e9, &[]);
        assert!(f.iter().any(|x| x.rule.starts_with("drive:sectors") && x.urgent), "{:?}", f);
        assert!(!f.iter().any(|x| x.rule.starts_with("drive:spare")), "no spare figure on SATA");
        // A drive that answers only page 1 still gives hours and writes, and says nothing else.
        let h = parse_ata_stats(Some(&p1), None, None, None, None);
        assert_eq!(h.hours, 30_000);
        assert!(judge(&drive(h), None, 1e9, &[]).0.is_empty());
    }

    #[test]
    fn parses_like_smartctl() {
        // beastly's C: on 19 Nov: 61 C, 99% spare, 42% used, 1,538,640,478 units (787 TB written).
        let h = parse_health(&log(334, 99, 42, 1_538_640_478, 39_995, 154, 0), Some(&ident(351, 354)));
        assert_eq!(h.temp_c, 61.0);
        assert_eq!((h.warn_c, h.crit_c), (78.0, 81.0));
        assert_eq!((h.spare, h.used_pct, h.hours, h.unsafe_shutdowns), (99, 42, 39_995, 154));
        assert!((h.written_tb - 787.8).abs() < 0.5, "{}", h.written_tb);
    }

    #[test]
    fn a_healthy_drive_says_nothing_and_is_remembered() {
        let h = parse_health(&log(320, 99, 42, 1_538_640_478, 39_995, 154, 0), Some(&ident(351, 354)));
        let (f, s) = judge(&drive(h), None, 1e9, &[]);
        assert!(f.is_empty(), "{:?}", f);
        assert_eq!(s.unwrap().unsafe_shutdowns, 154);
    }

    #[test]
    fn changes_since_last_look_are_reported_once() {
        let h = parse_health(&log(320, 99, 42, 1_538_640_478, 40_000, 156, 2), Some(&ident(351, 354)));
        let prev = Seen { base_unix: 1e9 - 86400.0, base_written: 787.0, media: 0, unsafe_shutdowns: 154 };
        let (f, s) = judge(&drive(h.clone()), Some(&prev), 1e9, &[]);
        assert!(f.iter().any(|x| x.rule.starts_with("drive:media") && x.urgent));
        assert!(f.iter().any(|x| x.rule.starts_with("drive:unsafe") && x.msg.contains("2 times")));
        let (again, _) = judge(&drive(h), s.as_ref(), 1e9 + 3600.0, &[]);
        assert!(again.is_empty(), "{:?}", again);
    }

    #[test]
    fn hot_failing_and_worn_drives() {
        let mut h = parse_health(&log(345, 15, 95, 1_000_000_000, 1000, 0, 0), Some(&ident(351, 354)));
        h.critical = 0x04;
        let r: Vec<String> = judge(&drive(h), None, 1e9, &[]).0.into_iter().map(|f| f.rule.split(':').nth(1).unwrap().to_string()).collect();
        for k in ["critical", "hot", "spare", "worn", "wear"] { assert!(r.iter().any(|x| x == k), "{} missing from {:?}", k, r); }
    }

    #[test]
    fn wear_uses_the_measured_pace_once_it_has_three_days() {
        // 50% used after 500 TB: rated 1000 TB. Lifetime pace is slow, but the last 4 days wrote 8 TB
        // (2 TB/day): 500 TB left / 2 = 250 days, under a year.
        let h = parse_health(&log(320, 99, 50, (500e12 / 512_000.0) as u64, 87_600, 0, 0), None);
        let prev = Seen { base_unix: 1e9 - 4.0 * 86400.0, base_written: 492.0, media: 0, unsafe_shutdowns: 0 };
        let (f, _) = judge(&drive(h.clone()), Some(&prev), 1e9, &[]);
        assert!(f.iter().any(|x| x.rule.starts_with("drive:wear") && x.msg.contains("250 days")), "{:?}", f);
        // Lifetime pace alone (500 TB over 10 years) is fine.
        assert!(judge(&drive(h), None, 1e9, &[]).0.is_empty());
    }

    #[test]
    fn usb_drive_holding_what_you_run_is_named() {
        let mut d = drive(Health::default());
        d.usb = true;
        d.letters = vec!['X'];
        d.health = None;
        let (f, _) = judge(&d, None, 1e9, &["the programs you run (python.exe)".into()]);
        assert!(f[0].rule.starts_with("drive:usb") && f[0].msg.contains("X:"));
        assert!(judge(&d, None, 1e9, &[]).0.is_empty(), "a USB drive nothing runs from is fine");
    }

    #[test]
    fn space_low_and_filling() {
        let mut s = Space::default();
        assert!(s.tick(0.0, &[('C', 500.0, 2000.0)]).is_empty());
        // 1 GB a minute for 15 minutes: 485 GB left is 8 hours, no card.
        for m in 1..=15 { assert!(s.tick(m as f64 * 60.0, &[('C', 500.0 - m as f64, 2000.0)]).is_empty()); }
        // Low and dropping 1 GB/min: 40 minutes left.
        let mut s = Space::default();
        let mut last = Vec::new();
        for m in 0..=12 { last = s.tick(m as f64 * 60.0, &[('C', 52.0 - m as f64, 2000.0)]); }
        assert!(last.iter().any(|x| matches!(x, SpaceStep::Filling { letter: 'C', .. })), "{:?}", last);
        assert!(last.iter().any(|x| matches!(x, SpaceStep::Low { .. })), "40 GB of 2 TB is under the 50 GB cap");
        // A copy that finished (no recent drop) is not "filling".
        let mut s = Space::default();
        for m in 0..=12 { last = s.tick(m as f64 * 60.0, &[('D', if m < 3 { 50.0 } else { 40.0 }, 100.0)]); }
        assert!(!last.iter().any(|x| matches!(x, SpaceStep::Filling { .. })), "{:?}", last);
    }
}

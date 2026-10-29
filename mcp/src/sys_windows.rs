//! Live system sampling via raw-dylib imports (no dependencies).
use std::time::Duration;

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtQuerySystemInformation(class: u32, buf: *mut u8, len: u32, ret: *mut u32) -> i32;
}

#[repr(C)]
struct MemStatus {
    length: u32,
    load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page: u64,
    avail_page: u64,
    total_virt: u64,
    avail_virt: u64,
    avail_ext: u64,
}

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn GlobalMemoryStatusEx(m: *mut MemStatus) -> i32;
    fn GetSystemTimes(idle: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
}

pub struct Proc {
    pub pid: u64,
    pub ppid: u64,
    pub name: String,
    pub handles: u32,
    pub ws: u64,
    pub create: i64,
    pub cpu_time: u64, // user + kernel, 100ns units
}

pub struct Row {
    pub pid: u64,
    pub ppid: u64,
    pub name: String,
    pub handles: u32,
    pub ws_mb: f64,
    pub cpu: f64,
}

unsafe fn rd<T: Copy>(base: *const u8, off: usize) -> T {
    std::ptr::read_unaligned(base.add(off) as *const T)
}

pub fn procs() -> Vec<Proc> {
    let mut buf: Vec<u64> = vec![0; 1 << 17]; // 1 MiB, 8-byte aligned
    loop {
        let mut ret = 0u32;
        let st = unsafe { NtQuerySystemInformation(5, buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut ret) };
        if st == 0xC0000004u32 as i32 {
            let need = (ret as usize / 8 + 8192).max(buf.len() * 2);
            buf = vec![0; need];
            continue;
        }
        if st < 0 {
            return Vec::new();
        }
        break;
    }
    let base = buf.as_ptr() as *const u8;
    let total = buf.len() * 8;
    let mut out = Vec::new();
    let mut off = 0usize;
    loop {
        if off + 160 > total {
            break;
        }
        let p = unsafe { base.add(off) };
        let (next, len, ptr, pid, ppid, handles, ws, create, user, kernel) = unsafe {
            (
                rd::<u32>(p, 0) as usize,
                rd::<u16>(p, 56) as usize,
                rd::<usize>(p, 64),
                rd::<usize>(p, 80) as u64,
                rd::<usize>(p, 88) as u64,
                rd::<u32>(p, 96),
                rd::<usize>(p, 144) as u64,
                rd::<i64>(p, 32),
                rd::<i64>(p, 40),
                rd::<i64>(p, 48),
            )
        };
        let name = if pid == 0 {
            "System Idle Process".to_string()
        } else if len > 0 && ptr != 0 {
            let mut v = Vec::with_capacity(len / 2);
            for i in 0..len / 2 {
                v.push(unsafe { std::ptr::read_unaligned((ptr as *const u16).add(i)) });
            }
            String::from_utf16_lossy(&v)
        } else {
            String::new()
        };
        out.push(Proc { pid, ppid, name, handles, ws, create, cpu_time: (user + kernel).max(0) as u64 });
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

/// (idle, kernel+user) in 100ns units. kernel from the API already includes idle.
fn times() -> (u64, u64) {
    let (mut i, mut k, mut u) = (0u64, 0u64, 0u64);
    unsafe { GetSystemTimes(&mut i, &mut k, &mut u) };
    (i, k + u)
}

pub fn cpu_now() -> f64 {
    let a = times();
    std::thread::sleep(Duration::from_millis(500));
    let b = times();
    let tot = b.1.saturating_sub(a.1) as f64;
    if tot <= 0.0 {
        return 0.0;
    }
    (100.0 * (1.0 - b.0.saturating_sub(a.0) as f64 / tot)).clamp(0.0, 100.0)
}

/// (used_mb, total_mb)
pub fn ram() -> (f64, f64) {
    let mut m: MemStatus = unsafe { std::mem::zeroed() };
    m.length = std::mem::size_of::<MemStatus>() as u32;
    unsafe { GlobalMemoryStatusEx(&mut m) };
    let t = m.total_phys as f64 / 1048576.0;
    (t - m.avail_phys as f64 / 1048576.0, t)
}

/// Returns (rows sorted, total machine CPU %).
pub fn top(limit: usize, by_mem: bool) -> (Vec<Row>, f64) {
    let a = procs();
    let t1 = times();
    std::thread::sleep(Duration::from_millis(1000));
    let b = procs();
    let t2 = times();
    let dsys = t2.1.saturating_sub(t1.1) as f64;
    let busy = if dsys > 0.0 { 100.0 * (1.0 - t2.0.saturating_sub(t1.0) as f64 / dsys) } else { 0.0 };
    let mut prev = std::collections::HashMap::new();
    for p in &a {
        prev.insert((p.pid, p.create), p.cpu_time);
    }
    let mut rows: Vec<Row> = b
        .into_iter()
        .filter(|p| p.pid != 0)
        .map(|p| {
            let d = prev.get(&(p.pid, p.create)).map(|o| p.cpu_time.saturating_sub(*o)).unwrap_or(0) as f64;
            Row {
                pid: p.pid,
                ppid: p.ppid,
                name: p.name,
                handles: p.handles,
                ws_mb: p.ws as f64 / 1048576.0,
                cpu: if dsys > 0.0 { (100.0 * d / dsys).min(100.0) } else { 0.0 },
            }
        })
        .collect();
    if by_mem {
        rows.sort_by(|x, y| y.ws_mb.partial_cmp(&x.ws_mb).unwrap());
    } else {
        rows.sort_by(|x, y| y.cpu.partial_cmp(&x.cpu).unwrap().then(y.ws_mb.partial_cmp(&x.ws_mb).unwrap()));
    }
    rows.truncate(limit);
    (rows, busy.clamp(0.0, 100.0))
}
